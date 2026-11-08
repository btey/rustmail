//! A small POP3 client (RFC 1939 with RFC 2595 STLS, UIDL and TOP).
//!
//! Every read is bounded in line length, total size and time, so a broken or
//! hostile server cannot exhaust memory or hang a sync worker. A rejected
//! `STLS` is an error: credentials are never sent over a connection that was
//! meant to be encrypted.

use super::md5::md5_hex;
use crate::error::{CoreError, Result};
use crate::imap::{account_tls_connector, open_tcp};
use crate::models::{ConnectionSecurity, MailConnectionSettings, Pop3Auth};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest status or data line accepted from the server.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Largest single message read into memory.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Largest `LIST` / `UIDL` reply accepted.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
/// Largest header block read with `TOP`.
const MAX_TOP_BYTES: usize = 256 * 1024;
/// Most messages one mailbox listing may contain.
pub const MAX_LISTED: usize = 200_000;

/// Anything a POP3 session can run over (TCP, TLS, or an in-memory pipe).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

fn pop_error(message: impl Into<String>) -> CoreError {
    CoreError::Network(format!("pop3: {}", message.into()))
}

#[derive(Clone)]
pub struct Login {
    pub user: String,
    pub password: String,
}

pub struct Pop3Client {
    reader: BufReader<Box<dyn Io>>,
    timeout: Duration,
    /// The `<timestamp>` from the greeting, when the server offers APOP.
    apop_stamp: Option<String>,
}

/// The `<...>` token of a greeting, when it can be used for APOP.
pub fn apop_timestamp(greeting: &str) -> Option<String> {
    let start = greeting.find('<')?;
    let end = start + greeting[start..].find('>')?;
    let stamp = &greeting[start..=end];
    (stamp.contains('@') && stamp.len() <= 256 && stamp.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .then(|| stamp.to_owned())
}

/// `APOP` digest: hex MD5 of the timestamp followed by the shared secret.
pub fn apop_digest(stamp: &str, password: &str) -> String {
    md5_hex(format!("{stamp}{password}").as_bytes())
}

/// A UIDL must be 1 to 70 printable ASCII characters (RFC 1939).
pub fn valid_uidl(uidl: &str) -> bool {
    (1..=70).contains(&uidl.len()) && uidl.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Parse `n size` lines from `LIST`.
pub fn parse_list(data: &[u8]) -> Vec<(u32, u64)> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let number: u32 = parts.next()?.parse().ok()?;
            let size: u64 = parts.next()?.parse().ok()?;
            (number > 0).then_some((number, size))
        })
        .collect()
}

/// Parse `n uidl` lines from `UIDL`. Invalid identifiers are dropped.
pub fn parse_uidl(data: &[u8]) -> Vec<(u32, String)> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let number: u32 = parts.next()?.parse().ok()?;
            let uidl = parts.next()?;
            (number > 0 && valid_uidl(uidl)).then(|| (number, uidl.to_owned()))
        })
        .collect()
}

impl Pop3Client {
    /// Connect, secure the session and log in.
    pub async fn connect(
        host: &str,
        port: u16,
        settings: &MailConnectionSettings,
        login: &Login,
    ) -> Result<Self> {
        let timeout = settings.timeout();
        tokio::time::timeout(timeout, Self::connect_inner(host, port, settings, login))
            .await
            .map_err(|_| pop_error(format!("connect {host}:{port}: timed out")))?
    }

    async fn connect_inner(
        host: &str,
        port: u16,
        settings: &MailConnectionSettings,
        login: &Login,
    ) -> Result<Self> {
        let connector = account_tls_connector(settings)?;
        let tcp = open_tcp(host, port, settings).await?;
        tcp.set_nodelay(true).ok();
        let (implicit_tls, starttls) = match settings.imap_security {
            ConnectionSecurity::Tls => (true, false),
            ConnectionSecurity::Starttls => (false, true),
            ConnectionSecurity::None => (false, false),
            ConnectionSecurity::Auto => (port != 110, port == 110),
        };
        let encrypted = implicit_tls || starttls;
        let server_name = || {
            rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|e| CoreError::Tls(e.to_string()))
        };
        let tls_failure = |e: std::io::Error| {
            CoreError::Tls(format!(
                "pop3 {host}:{port}: {e}. Check SSL/TLS versus STARTTLS and use the hostname on the server certificate. If you trust this server, enable \"Accept untrusted certificate\" under Connection security."
            ))
        };
        let stream: Box<dyn Io> = if implicit_tls {
            Box::new(
                connector
                    .connect(server_name()?, tcp)
                    .await
                    .map_err(tls_failure)?,
            )
        } else {
            Box::new(tcp)
        };
        let mut client = Self {
            reader: BufReader::new(stream),
            timeout: settings.timeout(),
            apop_stamp: None,
        };
        let greeting = client.read_status().await?;
        if !greeting.0 {
            return Err(pop_error(format!(
                "the server refused the connection ({})",
                greeting.1
            )));
        }
        client.apop_stamp = apop_timestamp(&greeting.1);
        if starttls {
            let (ok, text) = client.command("STLS").await?;
            if !ok {
                return Err(CoreError::Tls(format!(
                    "pop3 STLS was rejected ({text}). Check the server port and security mode."
                )));
            }
            // The server sends nothing until the TLS handshake starts, so any
            // buffered plaintext would be an injection attempt.
            if !client.reader.buffer().is_empty() {
                return Err(CoreError::Tls(
                    "the pop3 server sent data before the TLS handshake".into(),
                ));
            }
            let inner = std::mem::replace(
                &mut client.reader,
                BufReader::new(Box::new(tokio::io::empty()) as Box<dyn Io>),
            )
            .into_inner();
            let tls = connector
                .connect(server_name()?, inner)
                .await
                .map_err(tls_failure)?;
            client.reader = BufReader::new(Box::new(tls));
        }
        client
            .authenticate(login, settings.pop3_auth, encrypted)
            .await?;
        Ok(client)
    }

    async fn authenticate(&mut self, login: &Login, mode: Pop3Auth, encrypted: bool) -> Result<()> {
        if login.user.is_empty()
            || login.user.contains(['\r', '\n'])
            || login.password.contains(['\r', '\n'])
        {
            return Err(CoreError::Auth(
                "pop3 login is empty or has a line break".into(),
            ));
        }
        let use_apop = match mode {
            Pop3Auth::Plain => false,
            Pop3Auth::Apop => true,
            Pop3Auth::Auto => !encrypted && self.apop_stamp.is_some(),
        };
        if use_apop {
            let stamp = self
                .apop_stamp
                .clone()
                .ok_or_else(|| CoreError::Auth("the pop3 server does not offer APOP".into()))?;
            let digest = apop_digest(&stamp, &login.password);
            let (ok, text) = self
                .command(&format!("APOP {} {digest}", login.user))
                .await?;
            return if ok {
                Ok(())
            } else {
                Err(CoreError::Auth(format!("pop3 login refused ({text})")))
            };
        }
        let (ok, text) = self.command(&format!("USER {}", login.user)).await?;
        if !ok {
            return Err(CoreError::Auth(format!("pop3 login refused ({text})")));
        }
        let (ok, text) = self.command(&format!("PASS {}", login.password)).await?;
        if ok {
            Ok(())
        } else {
            Err(CoreError::Auth(format!("pop3 login refused ({text})")))
        }
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        debug_assert!(!line.contains(['\r', '\n']));
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            self.reader.get_mut().write_all(&bytes).await?;
            self.reader.get_mut().flush().await
        })
        .await
        .map_err(|_| pop_error("timed out sending a command"))?
        .map_err(|e| pop_error(format!("connection lost: {e}")))
    }

    /// Read one line (without its terminator) of at most `max` bytes.
    async fn read_line_limited(&mut self, max: usize) -> Result<Vec<u8>> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            let mut line = Vec::new();
            loop {
                let buffer = self
                    .reader
                    .fill_buf()
                    .await
                    .map_err(|e| pop_error(format!("connection lost: {e}")))?;
                if buffer.is_empty() {
                    return Err(pop_error("the server closed the connection"));
                }
                match buffer.iter().position(|&b| b == b'\n') {
                    Some(end) => {
                        line.extend_from_slice(&buffer[..end]);
                        self.reader.consume(end + 1);
                        break;
                    }
                    None => {
                        let used = buffer.len();
                        line.extend_from_slice(buffer);
                        self.reader.consume(used);
                    }
                }
                if line.len() > max {
                    return Err(pop_error("the server sent an over-long line"));
                }
            }
            if line.len() > max {
                return Err(pop_error("the server sent an over-long line"));
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            Ok(line)
        })
        .await
        .map_err(|_| pop_error("timed out waiting for the server"))?
    }

    /// Read a status line: `(true, text)` for `+OK`, `(false, text)` for `-ERR`.
    async fn read_status(&mut self) -> Result<(bool, String)> {
        let line = self.read_line_limited(MAX_LINE_BYTES).await?;
        let text = String::from_utf8_lossy(&line).into_owned();
        if let Some(rest) = text.strip_prefix("+OK") {
            Ok((true, rest.trim().to_owned()))
        } else if let Some(rest) = text.strip_prefix("-ERR") {
            Ok((false, rest.trim().to_owned()))
        } else {
            Err(pop_error("the server sent an unrecognised reply"))
        }
    }

    async fn command(&mut self, line: &str) -> Result<(bool, String)> {
        self.write_line(line).await?;
        self.read_status().await
    }

    /// Read a dot-terminated block, undoing dot-stuffing. Lines come back
    /// with CRLF endings. Fails (and the session must be dropped) when the
    /// block exceeds `limit` bytes.
    async fn read_multiline(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            let line = self
                .read_line_limited(limit.saturating_sub(data.len()).max(2))
                .await?;
            if line == b"." {
                return Ok(data);
            }
            let line = line.strip_prefix(b".").unwrap_or(&line);
            if data.len() + line.len() + 2 > limit {
                return Err(pop_error("the server sent more data than allowed"));
            }
            data.extend_from_slice(line);
            data.extend_from_slice(b"\r\n");
        }
    }

    /// `(message count, total octets)`.
    pub async fn stat(&mut self) -> Result<(u32, u64)> {
        let (ok, text) = self.command("STAT").await?;
        if !ok {
            return Err(pop_error(format!("STAT failed ({text})")));
        }
        let mut parts = text.split_whitespace();
        let count = parts.next().and_then(|v| v.parse().ok());
        let octets = parts.next().and_then(|v| v.parse().ok());
        match (count, octets) {
            (Some(count), Some(octets)) => Ok((count, octets)),
            _ => Err(pop_error("malformed STAT reply")),
        }
    }

    /// Message numbers and sizes.
    pub async fn list(&mut self) -> Result<Vec<(u32, u64)>> {
        let (ok, text) = self.command("LIST").await?;
        if !ok {
            return Err(pop_error(format!("LIST failed ({text})")));
        }
        let data = self.read_multiline(MAX_LIST_BYTES).await?;
        let mut list = parse_list(&data);
        list.truncate(MAX_LISTED);
        Ok(list)
    }

    /// Message numbers and unique ids; `None` when the server has no UIDL.
    pub async fn uidl(&mut self) -> Result<Option<Vec<(u32, String)>>> {
        let (ok, _) = self.command("UIDL").await?;
        if !ok {
            return Ok(None);
        }
        let data = self.read_multiline(MAX_LIST_BYTES).await?;
        let mut list = parse_uidl(&data);
        list.truncate(MAX_LISTED);
        Ok(Some(list))
    }

    /// Download one message (at most `MAX_MESSAGE_BYTES`).
    pub async fn retr(&mut self, number: u32) -> Result<Vec<u8>> {
        let (ok, text) = self.command(&format!("RETR {number}")).await?;
        if !ok {
            return Err(pop_error(format!("RETR {number} failed ({text})")));
        }
        self.read_multiline(MAX_MESSAGE_BYTES).await
    }

    /// The header block of one message (`TOP n 0`); `None` when unsupported.
    pub async fn top_headers(&mut self, number: u32) -> Result<Option<Vec<u8>>> {
        let (ok, _) = self.command(&format!("TOP {number} 0")).await?;
        if !ok {
            return Ok(None);
        }
        Ok(Some(self.read_multiline(MAX_TOP_BYTES).await?))
    }

    /// Mark a message for deletion; it goes when the session ends with `quit`.
    pub async fn dele(&mut self, number: u32) -> Result<()> {
        let (ok, text) = self.command(&format!("DELE {number}")).await?;
        if ok {
            Ok(())
        } else {
            Err(pop_error(format!("DELE {number} failed ({text})")))
        }
    }

    /// End the session. Deletions take effect only when the server accepts
    /// this, so a failure is reported.
    pub async fn quit(mut self) -> Result<()> {
        let (ok, text) = self.command("QUIT").await?;
        if ok {
            Ok(())
        } else {
            Err(pop_error(format!(
                "QUIT failed ({text}); deletions may not have been applied"
            )))
        }
    }

    /// End the session without caring about the outcome.
    pub async fn quit_quietly(mut self) {
        let _ = self.write_line("QUIT").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apop_example_from_rfc1939() {
        let greeting = "POP3 server ready <1896.697170952@dbc.mtview.ca.us>";
        let stamp = apop_timestamp(greeting).unwrap();
        assert_eq!(
            apop_digest(&stamp, "tanstaaf"),
            "c4c9334bac560ecc979e58001b3e22fb"
        );
    }

    #[test]
    fn greetings_without_a_usable_timestamp_are_ignored() {
        assert_eq!(apop_timestamp("ready"), None);
        assert_eq!(apop_timestamp("ready <no-at-sign>"), None);
        assert_eq!(apop_timestamp("ready <a b@c>"), None);
        assert_eq!(apop_timestamp("ready <a@b"), None);
    }

    #[test]
    fn listings_skip_malformed_lines() {
        assert_eq!(
            parse_list(b"1 120\r\nx 5\r\n2\r\n0 9\r\n3 7\r\n"),
            vec![(1, 120), (3, 7)]
        );
        assert_eq!(
            parse_uidl(b"1 abc\r\n2 \x7f\r\n3 ok-3\r\n4\r\n"),
            vec![(1, "abc".to_owned()), (3, "ok-3".to_owned())]
        );
        assert!(!valid_uidl(&"x".repeat(71)));
        assert!(!valid_uidl(""));
    }
}
