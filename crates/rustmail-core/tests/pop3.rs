//! POP3 accounts against an in-process fake POP3 server: download, UIDL
//! tracking, leave-on-server, deletion rules, size limit, TLS/STLS, USER/PASS,
//! APOP, filter-on-receive and safety limits.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustmail_core::Core;
use rustmail_core::accounts::secrets::DevelopmentFileCredentialStore;
use rustmail_core::config::Paths;
use rustmail_core::models::*;
use rustmail_core::pop3::client::Io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const CERT: &str = include_str!("fixtures/tls/server.pem");
const KEY: &str = include_str!("fixtures/tls/server-key.pem");
const STAMP: &str = "<1896.697170952@dbc.mtview.ca.us>";

// ---- fake server --------------------------------------------------------

#[derive(Default)]
struct Server {
    /// Messages on the server: (uidl, raw message with CRLF lines).
    messages: Vec<(String, String)>,
    user: String,
    password: String,
    /// Offer an APOP timestamp in the greeting.
    apop: bool,
    no_uidl: bool,
    refuse_stls: bool,
    /// Answer every status line with this many bytes of junk.
    long_line: bool,
    log: Vec<String>,
}

type Shared = Arc<Mutex<Server>>;

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Plain,
    ImplicitTls,
    Stls,
}

fn mail(subject: &str, from: &str, body: &str) -> String {
    format!(
        "From: {from}\r\nTo: me@example.org\r\nSubject: {subject}\r\nMessage-ID: <{}@example.org>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\r\n{body}\r\n",
        subject.replace(' ', "-")
    )
}

fn stuffed(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_terminator("\r\n") {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

fn acceptor() -> tokio_rustls::TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from_pem_slice(CERT.as_bytes()).unwrap()],
        PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap(),
    )
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

async fn serve(wire: Wire, shared: Shared) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                let io: Box<dyn Io> = if wire == Wire::ImplicitTls {
                    match acceptor().accept(tcp).await {
                        Ok(tls) => Box::new(tls),
                        Err(_) => return,
                    }
                } else {
                    Box::new(tcp)
                };
                session(io, shared, wire).await;
            });
        }
    });
    port
}

async fn session(io: Box<dyn Io>, shared: Shared, wire: Wire) {
    let mut reader = BufReader::new(io);
    let greeting = {
        let server = shared.lock().unwrap();
        if server.apop {
            format!("+OK POP3 ready {STAMP}\r\n")
        } else {
            "+OK POP3 ready\r\n".to_owned()
        }
    };
    reader.write_all(greeting.as_bytes()).await.unwrap();
    let mut secured = wire != Wire::Stls;
    let mut user = String::new();
    let mut authed = false;
    let mut deleted: Vec<usize> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return; // dropped without QUIT: nothing is deleted
        }
        let line = line.trim_end().to_owned();
        let mut parts = line.splitn(2, ' ');
        let command = parts.next().unwrap_or("").to_ascii_uppercase();
        let argument = parts.next().unwrap_or("").to_owned();
        let long_line = {
            let mut server = shared.lock().unwrap();
            let shown = if command == "PASS" {
                "PASS ***".to_owned()
            } else {
                line.clone()
            };
            server.log.push(shown);
            server.long_line
        };
        if long_line {
            let junk = format!("+OK {}\r\n", "x".repeat(200_000));
            let _ = reader.write_all(junk.as_bytes()).await;
            return;
        }
        let reply: String = match command.as_str() {
            "STLS" => {
                if wire != Wire::Stls || shared.lock().unwrap().refuse_stls {
                    "-ERR no STLS\r\n".into()
                } else {
                    reader.write_all(b"+OK begin TLS\r\n").await.unwrap();
                    let inner = reader.into_inner();
                    let tls = match acceptor().accept(inner).await {
                        Ok(tls) => tls,
                        Err(_) => return,
                    };
                    reader = BufReader::new(Box::new(tls) as Box<dyn Io>);
                    secured = true;
                    continue;
                }
            }
            "USER" => {
                if !secured {
                    "-ERR STLS first\r\n".into()
                } else {
                    user = argument;
                    "+OK\r\n".into()
                }
            }
            "PASS" => {
                let server = shared.lock().unwrap();
                if secured && user == server.user && argument == server.password {
                    authed = true;
                    "+OK logged in\r\n".into()
                } else {
                    "-ERR bad login\r\n".into()
                }
            }
            "APOP" => {
                let server = shared.lock().unwrap();
                let mut it = argument.split(' ');
                let (name, digest) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
                let expected = md5_hex(format!("{STAMP}{}", server.password).as_bytes());
                if server.apop && name == server.user && digest == expected {
                    authed = true;
                    "+OK logged in\r\n".into()
                } else {
                    "-ERR bad apop\r\n".into()
                }
            }
            _ if !authed => "-ERR not authenticated\r\n".into(),
            "STAT" => {
                let server = shared.lock().unwrap();
                let live: Vec<_> = server
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !deleted.contains(i))
                    .collect();
                let octets: usize = live.iter().map(|(_, m)| m.1.len()).sum();
                format!("+OK {} {octets}\r\n", live.len())
            }
            "LIST" => {
                let server = shared.lock().unwrap();
                let mut out = String::from("+OK\r\n");
                for (i, (_, raw)) in server.messages.iter().enumerate() {
                    if !deleted.contains(&i) {
                        out.push_str(&format!("{} {}\r\n", i + 1, raw.len()));
                    }
                }
                out.push_str(".\r\n");
                out
            }
            "UIDL" => {
                let server = shared.lock().unwrap();
                if server.no_uidl {
                    "-ERR not supported\r\n".into()
                } else {
                    let mut out = String::from("+OK\r\n");
                    for (i, (uidl, _)) in server.messages.iter().enumerate() {
                        if !deleted.contains(&i) {
                            out.push_str(&format!("{} {uidl}\r\n", i + 1));
                        }
                    }
                    out.push_str(".\r\n");
                    out
                }
            }
            "RETR" | "TOP" => {
                let server = shared.lock().unwrap();
                let number: usize = argument
                    .split(' ')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                match server.messages.get(number.wrapping_sub(1)) {
                    Some((_, raw)) if !deleted.contains(&(number - 1)) => {
                        let text = if command == "TOP" {
                            let head = raw.split("\r\n\r\n").next().unwrap_or("");
                            format!("{head}\r\n")
                        } else {
                            raw.clone()
                        };
                        format!("+OK\r\n{}.\r\n", stuffed(&text))
                    }
                    _ => "-ERR no such message\r\n".into(),
                }
            }
            "DELE" => {
                let number: usize = argument.parse().unwrap_or(0);
                let count = shared.lock().unwrap().messages.len();
                if number >= 1 && number <= count && !deleted.contains(&(number - 1)) {
                    deleted.push(number - 1);
                    "+OK marked\r\n".into()
                } else {
                    "-ERR no such message\r\n".into()
                }
            }
            "QUIT" => {
                if authed {
                    let mut server = shared.lock().unwrap();
                    let mut index = 0;
                    server.messages.retain(|_| {
                        let keep = !deleted.contains(&index);
                        index += 1;
                        keep
                    });
                }
                let _ = reader.write_all(b"+OK bye\r\n").await;
                return;
            }
            _ => "-ERR unknown command\r\n".into(),
        };
        reader.write_all(reply.as_bytes()).await.unwrap();
    }
}

/// Independent MD5 for the fake server (the client has its own).
#[allow(clippy::needless_range_loop)]
fn md5_hex(data: &[u8]) -> String {
    // RFC 1321 via a tiny straightforward implementation.
    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let k = ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32;
            let t = a
                .wrapping_add(f)
                .wrapping_add(k)
                .wrapping_add(m[g])
                .rotate_left(s[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(t);
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---- helpers ------------------------------------------------------------

fn server_with(messages: &[(&str, String)]) -> Shared {
    Arc::new(Mutex::new(Server {
        messages: messages
            .iter()
            .map(|(uidl, raw)| ((*uidl).to_owned(), raw.clone()))
            .collect(),
        user: "me".into(),
        password: "secret".into(),
        ..Default::default()
    }))
}

async fn start(temp: &Path) -> Core {
    Core::start_mail_ui_with_credentials(
        Paths::for_tests(&temp.join("profile")),
        Arc::new(DevelopmentFileCredentialStore::new(
            temp.join("credentials.json"),
        )),
    )
    .await
    .unwrap()
}

fn args(
    port: u16,
    security: ConnectionSecurity,
    tweak: impl FnOnce(&mut MailConnectionSettings),
) -> AddPasswordAccountArgs {
    let mut connection = MailConnectionSettings {
        imap_security: security,
        trusted_certificate_pem: CERT.into(),
        pop3_leave_on_server: true,
        ..Default::default()
    };
    tweak(&mut connection);
    AddPasswordAccountArgs {
        email: "me@example.org".into(),
        display_name: Some("Me".into()),
        username: "me".into(),
        password: "secret".into(),
        mail_protocol: MailProtocol::Pop3,
        jmap_url: String::new(),
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        smtp_host: String::new(),
        smtp_port: 0,
        connection,
        smtp_password: None,
        client_key_pem: None,
    }
}

async fn settle(core: &Core, account_id: i64) {
    core.sync_now(Some(account_id)).await.unwrap();
}

async fn subjects(core: &Core) -> Vec<(String, String)> {
    core.db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.subject, f.role FROM messages m JOIN folders f ON f.id = m.folder_id
                 ORDER BY m.subject",
            )?;
            Ok(stmt
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

fn log(shared: &Shared) -> Vec<String> {
    shared.lock().unwrap().log.clone()
}

fn three() -> Shared {
    server_with(&[
        ("u1", mail("One", "a@example.org", "first body")),
        ("u2", mail("Two", "b@example.org", ".dot line\r\n..two")),
        ("u3", mail("Three", "c@example.org", "third")),
    ])
}

// ---- tests --------------------------------------------------------------

#[tokio::test]
async fn downloads_into_the_inbox_and_never_twice_when_left_on_server() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |_| {}))
        .await
        .unwrap();
    assert_eq!(account.mail_protocol, MailProtocol::Pop3);
    settle(&core, account.id).await;
    let all = subjects(&core).await;
    assert_eq!(all.len(), 3, "{all:?}");
    assert!(all.iter().all(|(_, role)| role == "inbox"), "{all:?}");
    assert_eq!(shared.lock().unwrap().messages.len(), 3, "left on server");

    // A second sync fetches nothing again; a new message is picked up.
    let retr_before = log(&shared)
        .iter()
        .filter(|l| l.starts_with("RETR"))
        .count();
    settle(&core, account.id).await;
    shared
        .lock()
        .unwrap()
        .messages
        .push(("u4".into(), mail("Four", "d@example.org", "new")));
    settle(&core, account.id).await;
    let retr_after = log(&shared)
        .iter()
        .filter(|l| l.starts_with("RETR"))
        .count();
    assert_eq!(retr_after - retr_before, 1, "only the new message");
    assert_eq!(subjects(&core).await.len(), 4);

    // Body with dot-stuffing undone.
    let body: String = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT b.text_body FROM messages m JOIN message_bodies b ON b.message_id=m.id
                 WHERE m.subject='Two'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        body.contains(".dot line") && body.contains("..two"),
        "{body:?}"
    );
}

#[tokio::test]
async fn deletes_from_the_server_only_after_storing_when_not_leaving_mail() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |c| {
            c.pop3_leave_on_server = false;
        }))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(subjects(&core).await.len(), 3);
    assert!(
        shared.lock().unwrap().messages.is_empty(),
        "deleted after download"
    );
}

#[tokio::test]
async fn works_without_uidl_only_when_not_leaving_mail() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    shared.lock().unwrap().no_uidl = true;
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |_| {}))
        .await
        .unwrap();
    // Leave-on-server needs UIDL: the sync reports it and downloads nothing.
    assert!(core.sync_now(Some(account.id)).await.is_err());
    assert!(subjects(&core).await.is_empty());
    assert_eq!(shared.lock().unwrap().messages.len(), 3);

    let temp2 = tempfile::tempdir().unwrap();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp2.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |c| {
            c.pop3_leave_on_server = false;
        }))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(subjects(&core).await.len(), 3);
    assert!(shared.lock().unwrap().messages.is_empty());
}

#[tokio::test]
async fn deletes_old_messages_from_the_server_after_n_days() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |c| {
            c.pop3_delete_after_days = 7;
        }))
        .await
        .unwrap();
    settle(&core, account.id).await;
    settle(&core, account.id).await;
    assert_eq!(
        shared.lock().unwrap().messages.len(),
        3,
        "too young to delete"
    );

    // Age two of them past the limit.
    core.db
        .write(|conn| {
            conn.execute(
                "UPDATE pop3_seen SET downloaded_at = downloaded_at - 8*86400000
                 WHERE uidl IN ('u1','u3')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    settle(&core, account.id).await;
    let left: Vec<String> = shared
        .lock()
        .unwrap()
        .messages
        .iter()
        .map(|m| m.0.clone())
        .collect();
    assert_eq!(left, vec!["u2".to_owned()]);
    assert_eq!(subjects(&core).await.len(), 3, "local copies stay");
    // A later sync does not bring the deleted ones back.
    settle(&core, account.id).await;
    assert_eq!(subjects(&core).await.len(), 3);
}

#[tokio::test]
async fn messages_over_the_size_limit_are_imported_header_only_and_kept() {
    let temp = tempfile::tempdir().unwrap();
    let big = mail("Huge", "z@example.org", &"x".repeat(5000));
    let shared = server_with(&[("s1", mail("Small", "a@example.org", "tiny")), ("s2", big)]);
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |c| {
            c.pop3_leave_on_server = false;
            c.pop3_max_size_kb = 1;
        }))
        .await
        .unwrap();
    settle(&core, account.id).await;
    settle(&core, account.id).await;
    assert_eq!(subjects(&core).await.len(), 2);
    let left: Vec<String> = shared
        .lock()
        .unwrap()
        .messages
        .iter()
        .map(|m| m.0.clone())
        .collect();
    assert_eq!(
        left,
        vec!["s2".to_owned()],
        "the big one stays on the server"
    );
    let retrs: Vec<_> = log(&shared)
        .into_iter()
        .filter(|l| l.starts_with("RETR"))
        .collect();
    assert_eq!(
        retrs.len(),
        1,
        "the big message was never downloaded: {retrs:?}"
    );
    let body: String = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT b.text_body FROM messages m JOIN message_bodies b ON b.message_id=m.id
                 WHERE m.subject='Huge'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(body.contains("not downloaded"), "{body:?}");
}

#[tokio::test]
async fn implicit_tls_and_stls_connect_and_verify_the_certificate() {
    for (wire, security) in [
        (Wire::ImplicitTls, ConnectionSecurity::Tls),
        (Wire::Stls, ConnectionSecurity::Starttls),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let shared = three();
        let port = serve(wire, shared.clone()).await;
        let core = start(temp.path()).await;
        let account = core
            .add_account_password(args(port, security, |_| {}))
            .await
            .unwrap();
        settle(&core, account.id).await;
        assert_eq!(subjects(&core).await.len(), 3);
    }
}

#[tokio::test]
async fn rejected_stls_never_sends_the_password() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    shared.lock().unwrap().refuse_stls = true;
    let port = serve(Wire::Stls, shared.clone()).await;
    let core = start(temp.path()).await;
    let result = core
        .add_account_password(args(port, ConnectionSecurity::Starttls, |_| {}))
        .await;
    assert!(result.is_err());
    let log = log(&shared);
    assert!(log.iter().any(|l| l == "STLS"), "{log:?}");
    assert!(
        !log.iter()
            .any(|l| l.starts_with("USER") || l.starts_with("PASS")),
        "no credentials after a failed STLS: {log:?}"
    );
}

#[tokio::test]
async fn untrusted_certificates_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::ImplicitTls, shared.clone()).await;
    let core = start(temp.path()).await;
    let result = core
        .add_account_password(args(port, ConnectionSecurity::Tls, |c| {
            c.trusted_certificate_pem.clear();
        }))
        .await;
    assert!(result.is_err());
    assert!(log(&shared).is_empty(), "no command reached the server");
}

#[tokio::test]
async fn apop_is_used_on_plain_connections_and_a_wrong_password_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    shared.lock().unwrap().apop = true;
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |_| {}))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(subjects(&core).await.len(), 3);
    let log = log(&shared);
    assert!(log.iter().any(|l| l.starts_with("APOP me ")), "{log:?}");
    assert!(
        !log.iter().any(|l| l.starts_with("PASS")),
        "password never sent: {log:?}"
    );

    let mut bad = args(port, ConnectionSecurity::None, |_| {});
    bad.email = "other@example.org".into();
    bad.password = "wrong".into();
    assert!(core.add_account_password(bad).await.is_err());
}

#[tokio::test]
async fn forced_apop_is_refused_when_the_server_offers_none() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let result = core
        .add_account_password(args(port, ConnectionSecurity::None, |c| {
            c.pop3_auth = Pop3Auth::Apop;
        }))
        .await;
    assert!(result.is_err());
    assert!(!log(&shared).iter().any(|l| l.starts_with("PASS")));
}

#[tokio::test]
async fn wrong_password_is_an_auth_failure() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let mut bad = args(port, ConnectionSecurity::None, |_| {});
    bad.password = "nope".into();
    let result = core.add_account_password(bad).await;
    assert!(result.is_err());
    assert!(subjects(&core).await.is_empty());
}

#[tokio::test]
async fn an_over_long_server_line_is_cut_off() {
    let temp = tempfile::tempdir().unwrap();
    let shared = three();
    shared.lock().unwrap().long_line = true;
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let result = core
        .add_account_password(args(port, ConnectionSecurity::None, |_| {}))
        .await;
    let error = result.expect_err("must fail").to_string();
    assert!(error.contains("over-long"), "{error}");
}

#[tokio::test]
async fn new_mail_goes_through_the_filters() {
    let temp = tempfile::tempdir().unwrap();
    let shared = server_with(&[
        ("f1", mail("Boss note", "boss@example.org", "hi")),
        ("f2", mail("Lunch", "friend@example.org", "hi")),
    ]);
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    core.save_filter_rules(vec![FilterRule {
        id: String::new(),
        name: "Boss".into(),
        enabled: true,
        match_all: true,
        conditions: vec![FilterCondition {
            field: FilterField::From,
            op: FilterOp::Contains,
            value: "boss@".into(),
            name: String::new(),
        }],
        actions: vec![FilterAction {
            kind: "star".into(),
            value: String::new(),
        }],
        stop: false,
    }])
    .await
    .unwrap();
    let account = core
        .add_account_password(args(port, ConnectionSecurity::None, |_| {}))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let mut starred = Vec::new();
    for _ in 0..50 {
        starred = core
            .db
            .read(|conn| {
                let mut stmt = conn.prepare("SELECT subject FROM messages WHERE is_starred=1")?;
                Ok(stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap();
        if !starred.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(starred, vec!["Boss note".to_owned()]);
}
