//! A small NNTP client (RFC 3977 with RFC 4642 STARTTLS and RFC 4643
//! AUTHINFO USER/PASS), written for reading and posting to Usenet servers.
//!
//! Every read is bounded in line length, total size and time, so a broken or
//! hostile server cannot exhaust memory or hang a sync worker.

use crate::error::{CoreError, Result};
use crate::imap::{account_tls_connector, open_tcp};
use crate::models::{ConnectionSecurity, MailConnectionSettings};
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest status or data line accepted from the server.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Largest article (or header block) read into memory.
pub const MAX_ARTICLE_BYTES: usize = 8 * 1024 * 1024;
/// Largest `LIST` / `OVER` reply accepted.
const MAX_LIST_BYTES: usize = 64 * 1024 * 1024;
/// Largest article accepted for posting.
pub const MAX_POST_BYTES: usize = 8 * 1024 * 1024;

/// Anything an NNTP session can run over (TCP, TLS, or an in-memory pipe).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

fn news_error(message: impl Into<String>) -> CoreError {
    CoreError::Network(format!("news: {}", message.into()))
}

/// Credentials for AUTHINFO USER/PASS.
#[derive(Clone)]
pub struct Login {
    pub user: String,
    pub password: String,
}

/// One entry of `LIST ACTIVE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveGroup {
    pub name: String,
    pub high: u64,
    pub low: u64,
    /// `y` (posting allowed), `n`, `m` (moderated) or another status letter.
    pub status: char,
}

/// Reply to `GROUP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupStats {
    pub count: u64,
    pub low: u64,
    pub high: u64,
}

/// The fields of an overview line the importer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    pub number: u64,
    pub message_id: String,
    pub bytes: Option<u64>,
}

pub struct NntpClient {
    reader: BufReader<Box<dyn Io>>,
    timeout: Duration,
    capabilities: HashSet<String>,
    posting_allowed: bool,
}

/// Newsgroup names are limited to the characters real servers use; this also
/// guarantees a name can never carry a line break into a command.
pub fn valid_group_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-_.".contains(&b))
        && !name.starts_with('.')
        && !name.ends_with('.')
}

/// Split a subscription list (commas, spaces, new lines) into unique, valid
/// group names, preserving order. Returns the valid names and the rejected ones.
pub fn parse_group_list(text: &str) -> (Vec<String>, Vec<String>) {
    let mut seen = HashSet::new();
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for token in text.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if valid_group_name(token) {
            if seen.insert(token.to_ascii_lowercase()) {
                valid.push(token.to_owned());
            }
        } else {
            invalid.push(token.to_owned());
        }
    }
    (valid, invalid)
}

/// Parse a `LIST ACTIVE` line: `group high low status`.
pub fn parse_active_line(line: &str) -> Option<ActiveGroup> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let high = parts.next()?.parse().ok()?;
    let low = parts.next()?.parse().ok()?;
    let status = parts.next()?.chars().next()?;
    valid_group_name(name).then(|| ActiveGroup {
        name: name.to_owned(),
        high,
        low,
        status,
    })
}

/// Parse an `OVER`/`XOVER` line (tab separated: number, subject, from, date,
/// message-id, references, bytes, lines, ...). Malformed lines yield `None`.
pub fn parse_overview_line(line: &str) -> Option<Overview> {
    let mut fields = line.split('\t');
    let number: u64 = fields.next()?.trim().parse().ok()?;
    if number == 0 {
        return None;
    }
    let _subject = fields.next()?;
    let _from = fields.next()?;
    let _date = fields.next()?;
    let message_id = fields.next()?.trim().to_owned();
    let _references = fields.next();
    let bytes = fields.next().and_then(|value| value.trim().parse().ok());
    Some(Overview {
        number,
        message_id,
        bytes,
    })
}

/// Parse the data of a `211 count low high group` reply.
pub fn parse_group_reply(text: &str) -> Option<GroupStats> {
    let mut parts = text.split_whitespace();
    let count = parts.next()?.parse().ok()?;
    let low = parts.next()?.parse().ok()?;
    let high = parts.next()?.parse().ok()?;
    Some(GroupStats { count, low, high })
}

/// Prepare an article for `POST`: normalise line endings to CRLF, drop `Bcc`
/// (a posted article must not leak it) and dot-stuff. The terminating
/// `.` line is added by the caller.
pub fn prepare_post_body(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 32 + 8);
    let mut in_headers = true;
    let mut skipping_bcc = false;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if in_headers {
            if line.is_empty() {
                in_headers = false;
                skipping_bcc = false;
            } else if line.starts_with([' ', '\t']) {
                if skipping_bcc {
                    continue;
                }
            } else {
                skipping_bcc = line
                    .split_once(':')
                    .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("bcc"));
                if skipping_bcc {
                    continue;
                }
            }
        }
        if line.starts_with('.') {
            out.push(b'.');
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // `split('\n')` yields a trailing empty piece for text ending in a newline.
    if out.ends_with(b"\r\n\r\n") && raw.ends_with(b"\n") {
        out.truncate(out.len() - 2);
    }
    out
}

impl NntpClient {
    /// Connect, secure the session, identify as a reader and authenticate.
    pub async fn connect(
        host: &str,
        port: u16,
        settings: &MailConnectionSettings,
        login: Option<Login>,
    ) -> Result<Self> {
        let timeout = settings.timeout();
        tokio::time::timeout(timeout, Self::connect_inner(host, port, settings, login))
            .await
            .map_err(|_| news_error(format!("connect {host}:{port}: timed out")))?
    }

    async fn connect_inner(
        host: &str,
        port: u16,
        settings: &MailConnectionSettings,
        login: Option<Login>,
    ) -> Result<Self> {
        let connector = account_tls_connector(settings)?;
        let tcp = open_tcp(host, port, settings).await?;
        tcp.set_nodelay(true).ok();
        let (implicit_tls, starttls) = match settings.imap_security {
            ConnectionSecurity::Tls => (true, false),
            ConnectionSecurity::Starttls => (false, true),
            ConnectionSecurity::None => (false, false),
            ConnectionSecurity::Auto => (port != 119, port == 119),
        };
        let server_name = || {
            rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|e| CoreError::Tls(e.to_string()))
        };
        let tls_failure = |e: std::io::Error| {
            CoreError::Tls(format!(
                "news {host}:{port}: {e}. Check SSL/TLS versus STARTTLS and use the hostname on the server certificate. If you trust this server, enable \"Accept untrusted certificate\" under Connection security."
            ))
        };
        let timeout = settings.timeout();
        let stream: Box<dyn Io> = if implicit_tls {
            let tls = connector
                .connect(server_name()?, tcp)
                .await
                .map_err(tls_failure)?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        let mut client = Self {
            reader: BufReader::new(stream),
            timeout,
            capabilities: HashSet::new(),
            posting_allowed: false,
        };
        let (code, text) = client.read_status().await?;
        match code {
            200 => client.posting_allowed = true,
            201 => client.posting_allowed = false,
            400 | 502 => {
                return Err(news_error(format!(
                    "the server refused the connection ({code} {text})"
                )));
            }
            _ => return Err(news_error(format!("unexpected greeting: {code} {text}"))),
        }
        if starttls {
            client.refresh_capabilities().await;
            let (code, text) = client.command("STARTTLS").await?;
            if code != 382 {
                // Never send credentials over a connection that was meant to be
                // encrypted but could not be.
                return Err(CoreError::Tls(format!(
                    "news STARTTLS was rejected ({code} {text}). Check the server port and security mode."
                )));
            }
            // The server sends nothing until the TLS handshake starts, so any
            // buffered plaintext would be an injection attempt.
            if !client.reader.buffer().is_empty() {
                return Err(CoreError::Tls(
                    "the news server sent data before the TLS handshake".into(),
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
            client.capabilities.clear();
        }
        client.refresh_capabilities().await;
        if client.capabilities.contains("MODE-READER") && !client.capabilities.contains("READER") {
            let (code, _) = client.command("MODE READER").await?;
            match code {
                200 => client.posting_allowed = true,
                201 => client.posting_allowed = false,
                _ => {}
            }
            client.refresh_capabilities().await;
        }
        if let Some(login) = login {
            client.authenticate(&login).await?;
            client.refresh_capabilities().await;
        }
        Ok(client)
    }

    pub fn posting_allowed(&self) -> bool {
        self.posting_allowed
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.contains(&name.to_ascii_uppercase())
    }

    /// Best effort: servers without CAPABILITIES answer 500 and are treated as
    /// having none.
    async fn refresh_capabilities(&mut self) {
        self.capabilities.clear();
        let Ok((101, _)) = self.command("CAPABILITIES").await else {
            return;
        };
        let Ok(data) = self.read_multiline(MAX_LINE_BYTES * 4).await else {
            return;
        };
        for line in String::from_utf8_lossy(&data).lines() {
            if let Some(word) = line.split_whitespace().next() {
                self.capabilities.insert(word.to_ascii_uppercase());
            }
        }
    }

    async fn authenticate(&mut self, login: &Login) -> Result<()> {
        if login.user.contains(['\r', '\n']) || login.password.contains(['\r', '\n']) {
            return Err(CoreError::Auth("news login contains a line break".into()));
        }
        let (code, text) = self
            .command(&format!("AUTHINFO USER {}", login.user))
            .await?;
        match code {
            281 => return Ok(()),
            381 => {}
            _ => {
                return Err(CoreError::Auth(format!(
                    "news login refused ({code} {text})"
                )));
            }
        }
        let (code, text) = self
            .command(&format!("AUTHINFO PASS {}", login.password))
            .await?;
        if code == 281 {
            Ok(())
        } else {
            Err(CoreError::Auth(format!(
                "news login refused ({code} {text})"
            )))
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
        .map_err(|_| news_error("timed out sending a command"))?
        .map_err(|e| news_error(format!("connection lost: {e}")))
    }

    /// Read one status line (without its terminator), bounded by `MAX_LINE_BYTES`.
    async fn read_line(&mut self) -> Result<Vec<u8>> {
        self.read_line_limited(MAX_LINE_BYTES).await
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
                    .map_err(|e| news_error(format!("connection lost: {e}")))?;
                if buffer.is_empty() {
                    return Err(news_error("the server closed the connection"));
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
                    return Err(news_error("the server sent an over-long line"));
                }
            }
            if line.len() > max {
                return Err(news_error("the server sent an over-long line"));
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            Ok(line)
        })
        .await
        .map_err(|_| news_error("timed out waiting for the server"))?
    }

    async fn read_status(&mut self) -> Result<(u16, String)> {
        let line = self.read_line().await?;
        let text = String::from_utf8_lossy(&line).into_owned();
        let code = text
            .get(..3)
            .and_then(|digits| digits.parse::<u16>().ok())
            .ok_or_else(|| news_error(format!("malformed reply: {}", text.trim())))?;
        let rest = text.get(3..).unwrap_or("").trim().to_owned();
        Ok((code, rest))
    }

    /// Send a command and read its one-line status reply.
    pub async fn command(&mut self, line: &str) -> Result<(u16, String)> {
        self.write_line(line).await?;
        let reply = self.read_status().await?;
        if reply.0 == 400 {
            return Err(news_error(format!(
                "the server is closing the connection ({})",
                reply.1
            )));
        }
        Ok(reply)
    }

    /// Read a dot-terminated block, undoing dot-stuffing. Lines are returned
    /// with CRLF endings. Fails (and the session must be dropped) when the block
    /// exceeds `limit` bytes.
    async fn read_multiline(&mut self, limit: usize) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        loop {
            // A single line may be as long as the rest of the block allows.
            let line = self
                .read_line_limited(limit.saturating_sub(data.len()).max(2))
                .await?;
            if line == b"." {
                return Ok(data);
            }
            let line = line.strip_prefix(b".").unwrap_or(&line);
            if data.len() + line.len() + 2 > limit {
                return Err(news_error("the server sent more data than allowed"));
            }
            data.extend_from_slice(line);
            data.extend_from_slice(b"\r\n");
        }
    }

    /// `LIST ACTIVE` (falling back to plain `LIST`), for an optional wildmat.
    pub async fn list_active(&mut self, wildmat: Option<&str>) -> Result<Vec<ActiveGroup>> {
        let command = match wildmat {
            Some(pattern) if valid_wildmat(pattern) => format!("LIST ACTIVE {pattern}"),
            Some(_) => return Err(news_error("invalid group pattern")),
            None => "LIST ACTIVE".to_owned(),
        };
        let (mut code, mut text) = self.command(&command).await?;
        if code != 215 && wildmat.is_none() {
            (code, text) = self.command("LIST").await?;
        }
        if code != 215 {
            return Err(news_error(format!("LIST failed ({code} {text})")));
        }
        let data = self.read_multiline(MAX_LIST_BYTES).await?;
        Ok(String::from_utf8_lossy(&data)
            .lines()
            .filter_map(parse_active_line)
            .collect())
    }

    /// `LIST NEWSGROUPS`: group name and description pairs.
    pub async fn list_newsgroups(
        &mut self,
        wildmat: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        let command = match wildmat {
            Some(pattern) if valid_wildmat(pattern) => format!("LIST NEWSGROUPS {pattern}"),
            Some(_) => return Err(news_error("invalid group pattern")),
            None => "LIST NEWSGROUPS".to_owned(),
        };
        let (code, text) = self.command(&command).await?;
        if code != 215 {
            return Err(news_error(format!(
                "LIST NEWSGROUPS failed ({code} {text})"
            )));
        }
        let data = self.read_multiline(MAX_LIST_BYTES).await?;
        Ok(String::from_utf8_lossy(&data)
            .lines()
            .filter_map(|line| {
                let (name, description) = line
                    .split_once(['\t', ' '])
                    .map_or((line, ""), |(name, rest)| (name, rest));
                valid_group_name(name).then(|| (name.to_owned(), description.trim().to_owned()))
            })
            .collect())
    }

    /// Select a group. `Ok(None)` means the server does not carry it.
    pub async fn group(&mut self, name: &str) -> Result<Option<GroupStats>> {
        if !valid_group_name(name) {
            return Err(news_error(format!("invalid group name {name}")));
        }
        let (code, text) = self.command(&format!("GROUP {name}")).await?;
        match code {
            211 => parse_group_reply(&text)
                .map(Some)
                .ok_or_else(|| news_error(format!("malformed GROUP reply: {text}"))),
            411 => Ok(None),
            480 | 483 => Err(CoreError::Auth(format!(
                "the server requires a login to read {name}"
            ))),
            _ => Err(news_error(format!("GROUP {name} failed ({code} {text})"))),
        }
    }

    /// Overview lines for `first..=last` in the selected group. Tries `OVER`,
    /// then `XOVER`; `Ok(None)` means the server supports neither.
    pub async fn overview(&mut self, first: u64, last: u64) -> Result<Option<Vec<Overview>>> {
        let range = if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        };
        for verb in ["OVER", "XOVER"] {
            let (code, text) = self.command(&format!("{verb} {range}")).await?;
            match code {
                224 => {
                    let data = self.read_multiline(MAX_LIST_BYTES).await?;
                    return Ok(Some(
                        String::from_utf8_lossy(&data)
                            .lines()
                            .filter_map(parse_overview_line)
                            .collect(),
                    ));
                }
                // No articles in range.
                423 | 420 => return Ok(Some(Vec::new())),
                500 | 501 | 503 => continue,
                480 | 483 => return Err(CoreError::Auth("the server requires a login".into())),
                _ => return Err(news_error(format!("{verb} failed ({code} {text})"))),
            }
        }
        Ok(None)
    }

    /// Fetch an article by number (headers and body). `Ok(None)` when it has
    /// expired or was cancelled.
    pub async fn article(&mut self, number: u64) -> Result<Option<Vec<u8>>> {
        self.fetch("ARTICLE", 220, number).await
    }

    /// Fetch only the header block of an article.
    pub async fn head(&mut self, number: u64) -> Result<Option<Vec<u8>>> {
        self.fetch("HEAD", 221, number).await
    }

    async fn fetch(&mut self, verb: &str, ok: u16, number: u64) -> Result<Option<Vec<u8>>> {
        let (code, text) = self.command(&format!("{verb} {number}")).await?;
        match code {
            c if c == ok => Ok(Some(self.read_multiline(MAX_ARTICLE_BYTES).await?)),
            423 | 430 | 420 => Ok(None),
            480 | 483 => Err(CoreError::Auth("the server requires a login".into())),
            _ => Err(news_error(format!(
                "{verb} {number} failed ({code} {text})"
            ))),
        }
    }

    /// Post an article (`POST`). `raw` is the complete message.
    pub async fn post(&mut self, raw: &[u8]) -> Result<()> {
        if raw.len() > MAX_POST_BYTES {
            return Err(news_error("the article is too large to post"));
        }
        let (code, text) = self.command("POST").await?;
        match code {
            340 => {}
            440 => return Err(news_error("posting is not allowed on this server")),
            480 | 483 => {
                return Err(CoreError::Auth(
                    "the server requires a login to post".into(),
                ));
            }
            _ => return Err(news_error(format!("POST refused ({code} {text})"))),
        }
        let mut body = prepare_post_body(raw);
        if !body.ends_with(b"\r\n") {
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b".\r\n");
        let timeout = self.timeout.max(Duration::from_secs(60));
        tokio::time::timeout(timeout, async {
            self.reader.get_mut().write_all(&body).await?;
            self.reader.get_mut().flush().await
        })
        .await
        .map_err(|_| news_error("timed out sending the article"))?
        .map_err(|e| news_error(format!("connection lost: {e}")))?;
        let (code, text) = self.read_status().await?;
        match code {
            240 => Ok(()),
            441 => Err(CoreError::Other(format!(
                "the news server rejected the article: {text}"
            ))),
            _ => Err(news_error(format!("posting failed ({code} {text})"))),
        }
    }

    pub async fn quit(mut self) {
        let _ = self.write_line("QUIT").await;
    }
}

/// A wildmat for `LIST`: group-name characters plus `*`, `?`, `,` and `!`.
fn valid_wildmat(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() <= 255
        && pattern
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-_.*?,!".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_names_are_validated() {
        assert!(valid_group_name("comp.lang.rust"));
        assert!(valid_group_name("alt.fan.c++"));
        assert!(!valid_group_name(""));
        assert!(!valid_group_name("bad name"));
        assert!(!valid_group_name("x\r\nQUIT"));
        assert!(!valid_group_name(".leading"));
        assert!(!valid_group_name("trailing."));
    }

    #[test]
    fn group_lists_split_and_dedupe() {
        let (valid, invalid) =
            parse_group_list("comp.lang.rust, alt.test\nCOMP.LANG.RUST;bad/name  gmane.org");
        assert_eq!(valid, vec!["comp.lang.rust", "alt.test", "gmane.org"]);
        assert_eq!(invalid, vec!["bad/name"]);
    }

    #[test]
    fn active_lines_parse() {
        let group = parse_active_line("comp.lang.rust 0000012345 0000000001 y").unwrap();
        assert_eq!(group.name, "comp.lang.rust");
        assert_eq!((group.high, group.low, group.status), (12345, 1, 'y'));
        assert!(parse_active_line("garbage").is_none());
        assert!(parse_active_line("a.b x y z").is_none());
    }

    #[test]
    fn overview_lines_parse_and_reject_garbage() {
        let line = "42\tHello\tA <a@b>\tMon, 1 Jan 2024 00:00:00 +0000\t<id@x>\t<ref@x>\t1234\t10";
        let overview = parse_overview_line(line).unwrap();
        assert_eq!(overview.number, 42);
        assert_eq!(overview.message_id, "<id@x>");
        assert_eq!(overview.bytes, Some(1234));
        assert!(parse_overview_line("notanumber\tx\ty\tz\t<id>").is_none());
        assert!(parse_overview_line("0\tx\ty\tz\t<id>").is_none());
        assert!(parse_overview_line("5\tonly\ttwo").is_none());
        assert_eq!(parse_overview_line("7\ts\tf\td\t<i>").unwrap().bytes, None);
    }

    #[test]
    fn group_reply_parses() {
        assert_eq!(
            parse_group_reply("1234 3000234 3002322 misc.test"),
            Some(GroupStats {
                count: 1234,
                low: 3000234,
                high: 3002322
            })
        );
        assert!(parse_group_reply("x y z").is_none());
    }

    #[test]
    fn post_body_is_crlf_dot_stuffed_and_drops_bcc() {
        let raw = b"From: a@b\nBcc: secret@x\n  continued@x\nSubject: s\n\n.hidden\nbody\n..two\n";
        let prepared = String::from_utf8(prepare_post_body(raw)).unwrap();
        assert_eq!(
            prepared,
            "From: a@b\r\nSubject: s\r\n\r\n..hidden\r\nbody\r\n...two\r\n"
        );
    }

    #[tokio::test]
    async fn multiline_reads_undo_dot_stuffing() {
        let (client_side, mut server_side) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            server_side
                .write_all(b"line one\r\n..dotted\r\n.\r\nafter\r\n")
                .await
                .unwrap();
        });
        let mut client = NntpClient {
            reader: BufReader::new(Box::new(client_side)),
            timeout: Duration::from_secs(5),
            capabilities: HashSet::new(),
            posting_allowed: false,
        };
        let data = client.read_multiline(1024).await.unwrap();
        assert_eq!(data, b"line one\r\n.dotted\r\n");
        assert_eq!(client.read_line().await.unwrap(), b"after");
    }

    #[tokio::test]
    async fn oversized_blocks_and_lines_are_rejected() {
        let (client_side, mut server_side) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let mut big = vec![b'x'; 70 * 1024];
            big.extend_from_slice(b"\r\n");
            let _ = server_side.write_all(&big).await;
            let _ = server_side.write_all(b"a\r\nb\r\nc\r\n.\r\n").await;
        });
        let mut client = NntpClient {
            reader: BufReader::new(Box::new(client_side)),
            timeout: Duration::from_secs(5),
            capabilities: HashSet::new(),
            posting_allowed: false,
        };
        assert!(client.read_line().await.is_err());
        // Total size limit.
        let (client_side, mut server_side) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let _ = server_side.write_all(b"aaaa\r\nbbbb\r\n.\r\n").await;
        });
        let mut client = NntpClient {
            reader: BufReader::new(Box::new(client_side)),
            timeout: Duration::from_secs(5),
            capabilities: HashSet::new(),
            posting_allowed: false,
        };
        assert!(client.read_multiline(8).await.is_err());
    }
}
