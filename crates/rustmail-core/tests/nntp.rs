//! NNTP accounts against an in-process fake news server: import, incremental
//! sync, read state, TLS/STARTTLS, login, posting, expiry and safety limits.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use rustmail_core::Core;
use rustmail_core::accounts::secrets::DevelopmentFileCredentialStore;
use rustmail_core::config::Paths;
use rustmail_core::models::*;
use rustmail_core::nntp::client::Io;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const CERT: &str = include_str!("fixtures/tls/server.pem");
const KEY: &str = include_str!("fixtures/tls/server-key.pem");

// ---- fake server --------------------------------------------------------

#[derive(Default)]
struct Group {
    articles: BTreeMap<u64, String>,
    low: u64,
}

#[derive(Default)]
struct Server {
    groups: HashMap<String, Group>,
    posted: Vec<String>,
    credentials: Option<(String, String)>,
    no_over: bool,
    junk_overview: bool,
    refuse_starttls: bool,
    log: Vec<String>,
}

type Shared = Arc<Mutex<Server>>;

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Plain,
    ImplicitTls,
    StartTls,
}

fn article(subject: &str, id: &str, references: &str, body: &str) -> String {
    let refs = if references.is_empty() {
        String::new()
    } else {
        format!("References: {references}\r\n")
    };
    format!(
        "From: Poster <poster@example.org>\r\nNewsgroups: test.group\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n{refs}\r\n{body}\r\n"
    )
}

fn header(raw: &str, name: &str) -> String {
    raw.lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
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
    let mut tls_active = wire == Wire::ImplicitTls;
    let mut group: Option<String> = None;
    let mut authed = shared.lock().unwrap().credentials.is_none();
    let mut pending_user: Option<String> = None;
    reader.write_all(b"200 fake news ready\r\n").await.unwrap();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end().to_owned();
        let upper = line.to_ascii_uppercase();
        let verb = upper.split_whitespace().next().unwrap_or("").to_owned();
        {
            let mut server = shared.lock().unwrap();
            // Passwords stay out of the log.
            if verb == "AUTHINFO" && upper.contains(" PASS") {
                server.log.push("AUTHINFO PASS".into());
            } else {
                server.log.push(line.clone());
            }
        }
        let needs_auth = matches!(
            verb.as_str(),
            "GROUP" | "LIST" | "OVER" | "XOVER" | "ARTICLE" | "HEAD" | "POST"
        );
        if needs_auth && !authed {
            reader
                .write_all(b"480 authentication required\r\n")
                .await
                .unwrap();
            continue;
        }
        let reply: String = match verb.as_str() {
            "CAPABILITIES" => {
                let mut caps = String::from("101 capabilities\r\nVERSION 2\r\nREADER\r\nOVER\r\n");
                if wire == Wire::StartTls && !tls_active {
                    caps.push_str("STARTTLS\r\n");
                }
                caps.push_str("AUTHINFO USER\r\n.\r\n");
                caps
            }
            "STARTTLS" => {
                if wire != Wire::StartTls || shared.lock().unwrap().refuse_starttls {
                    "502 no TLS here\r\n".into()
                } else {
                    reader.write_all(b"382 go ahead\r\n").await.unwrap();
                    let inner = reader.into_inner();
                    let tls = acceptor().accept(inner).await.unwrap();
                    reader = BufReader::new(Box::new(tls));
                    tls_active = true;
                    continue;
                }
            }
            "AUTHINFO" => {
                let mut parts = line.splitn(3, ' ');
                parts.next();
                let sub = parts.next().unwrap_or("").to_ascii_uppercase();
                let value = parts.next().unwrap_or("").to_owned();
                let expected = shared.lock().unwrap().credentials.clone();
                match (sub.as_str(), expected) {
                    ("USER", Some(_)) => {
                        pending_user = Some(value);
                        "381 password please\r\n".into()
                    }
                    ("PASS", Some((user, password)))
                        if pending_user.as_deref() == Some(user.as_str()) && value == password =>
                    {
                        authed = true;
                        "281 welcome\r\n".into()
                    }
                    _ => "481 bad login\r\n".into(),
                }
            }
            "LIST" => {
                let server = shared.lock().unwrap();
                let mut out = String::from("215 list follows\r\n");
                if upper.contains("NEWSGROUPS") {
                    for name in server.groups.keys() {
                        out.push_str(&format!("{name}\tDescription of {name}\r\n"));
                    }
                } else {
                    for (name, group) in &server.groups {
                        let high = group.articles.keys().next_back().copied().unwrap_or(0);
                        out.push_str(&format!("{name} {high} {} y\r\n", group.low));
                    }
                }
                out.push_str(".\r\n");
                out
            }
            "GROUP" => {
                let name = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let server = shared.lock().unwrap();
                match server.groups.get(&name) {
                    Some(found) => {
                        let high = found.articles.keys().next_back().copied().unwrap_or(0);
                        let low = found
                            .articles
                            .keys()
                            .next()
                            .copied()
                            .unwrap_or(0)
                            .max(found.low);
                        group = Some(name.clone());
                        format!("211 {} {low} {high} {name}\r\n", found.articles.len())
                    }
                    None => "411 no such group\r\n".into(),
                }
            }
            "OVER" | "XOVER" => {
                let server = shared.lock().unwrap();
                if server.no_over {
                    "500 what?\r\n".into()
                } else {
                    let range = line.split_whitespace().nth(1).unwrap_or("");
                    let (a, b) = match range.split_once('-') {
                        Some((a, "")) => (a.parse().unwrap_or(0), u64::MAX),
                        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
                        None => (range.parse().unwrap_or(0), range.parse().unwrap_or(0)),
                    };
                    let mut out = String::from("224 overview follows\r\n");
                    if let Some(found) = group.as_ref().and_then(|g| server.groups.get(g)) {
                        for (number, raw) in found.articles.range(a..=b) {
                            out.push_str(&format!(
                                "{number}\t{}\t{}\t{}\t{}\t{}\t{}\t1\r\n",
                                header(raw, "Subject"),
                                header(raw, "From"),
                                header(raw, "Date"),
                                header(raw, "Message-ID"),
                                header(raw, "References"),
                                raw.len()
                            ));
                        }
                    }
                    if server.junk_overview {
                        out.push_str("garbage without tabs\r\nx\ty\r\n0\ta\tb\tc\t<d>\r\n");
                    }
                    out.push_str(".\r\n");
                    out
                }
            }
            "ARTICLE" | "HEAD" => {
                let number: u64 = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                let server = shared.lock().unwrap();
                match group
                    .as_ref()
                    .and_then(|g| server.groups.get(g))
                    .and_then(|g| g.articles.get(&number))
                {
                    Some(raw) if verb == "ARTICLE" => {
                        format!("220 {number} article\r\n{}.\r\n", stuffed(raw))
                    }
                    Some(raw) => {
                        let head = raw.split("\r\n\r\n").next().unwrap_or("");
                        format!(
                            "221 {number} head\r\n{}.\r\n",
                            stuffed(&format!("{head}\r\n"))
                        )
                    }
                    None => "423 no such article\r\n".into(),
                }
            }
            "POST" => {
                reader.write_all(b"340 send it\r\n").await.unwrap();
                let mut text = String::new();
                loop {
                    let mut part = String::new();
                    if reader.read_line(&mut part).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if part == ".\r\n" {
                        break;
                    }
                    text.push_str(part.strip_prefix('.').unwrap_or(&part));
                }
                shared.lock().unwrap().posted.push(text);
                "240 article posted\r\n".into()
            }
            "QUIT" => {
                let _ = reader.write_all(b"205 bye\r\n").await;
                return;
            }
            _ => "500 unknown command\r\n".into(),
        };
        reader.write_all(reply.as_bytes()).await.unwrap();
    }
}

fn server_with(groups: &[(&str, Vec<(u64, String)>)]) -> Shared {
    let mut server = Server::default();
    for (name, items) in groups {
        server.groups.insert(
            (*name).to_owned(),
            Group {
                articles: items.iter().cloned().collect(),
                low: 0,
            },
        );
    }
    Arc::new(Mutex::new(server))
}

// ---- helpers ------------------------------------------------------------

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

fn args(port: u16, groups: &str, security: ConnectionSecurity) -> AddPasswordAccountArgs {
    AddPasswordAccountArgs {
        email: "me@example.org".into(),
        display_name: Some("Me".into()),
        username: String::new(),
        password: String::new(),
        mail_protocol: MailProtocol::News,
        jmap_url: String::new(),
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        smtp_host: String::new(),
        smtp_port: 0,
        connection: MailConnectionSettings {
            imap_security: security,
            news_groups: groups.into(),
            trusted_certificate_pem: CERT.into(),
            ..Default::default()
        },
        smtp_password: None,
        client_key_pem: None,
    }
}

async fn settle(core: &Core, account_id: i64) {
    core.sync_now(Some(account_id)).await.unwrap();
    core.sync_now(Some(account_id)).await.unwrap();
}

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    thread_id: i64,
    subject: String,
    is_read: bool,
    folder: String,
    body: Option<String>,
    body_state: String,
}

async fn rows(core: &Core) -> Vec<Row> {
    core.db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.thread_id, m.subject, m.is_read, f.imap_name, b.text_body, m.body_state
                 FROM messages m JOIN folders f ON f.id = m.folder_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 ORDER BY m.subject",
            )?;
            Ok(stmt
                .query_map([], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        thread_id: r.get(1)?,
                        subject: r.get(2)?,
                        is_read: r.get::<_, i64>(3)? != 0,
                        folder: r.get(4)?,
                        body: r.get(5)?,
                        body_state: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

fn log(shared: &Shared) -> Vec<String> {
    shared.lock().unwrap().log.clone()
}

fn basic_server() -> Shared {
    server_with(&[
        (
            "test.group",
            vec![
                (
                    1,
                    article("First post", "a1@example.org", "", "hello world"),
                ),
                (
                    2,
                    article(
                        "Re: First post",
                        "a2@example.org",
                        "<a1@example.org>",
                        "a reply",
                    ),
                ),
                (
                    3,
                    article("Dots", "a3@example.org", "", ".leading dot\r\n..two dots"),
                ),
            ],
        ),
        (
            "other.group",
            vec![(7, article("Elsewhere", "b1@example.org", "", "other body"))],
        ),
    ])
}

// ---- tests --------------------------------------------------------------

#[tokio::test]
async fn imports_articles_threads_and_keeps_read_state() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(
            port,
            "test.group, other.group",
            ConnectionSecurity::None,
        ))
        .await
        .unwrap();
    assert_eq!(account.mail_protocol, MailProtocol::News);
    settle(&core, account.id).await;

    let all = rows(&core).await;
    assert_eq!(all.len(), 4, "{all:?}");
    let first = all.iter().find(|r| r.subject == "First post").unwrap();
    let reply = all.iter().find(|r| r.subject == "Re: First post").unwrap();
    assert_eq!(
        first.thread_id, reply.thread_id,
        "references thread articles"
    );
    assert_eq!(first.folder, "test.group");
    assert!(!first.is_read);
    assert_eq!(first.body.as_deref().map(str::trim), Some("hello world"));
    let dots = all.iter().find(|r| r.subject == "Dots").unwrap();
    let text = dots.body.clone().unwrap();
    assert!(
        text.contains(".leading dot") && text.contains("..two dots"),
        "{text:?}"
    );
    assert_eq!(
        all.iter()
            .find(|r| r.subject == "Elsewhere")
            .unwrap()
            .folder,
        "other.group"
    );

    // Local Sent / Drafts / Trash folders exist for posting and deleting.
    let roles: Vec<String> = core
        .db
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT role FROM folders WHERE role IS NOT NULL ORDER BY role")?;
            Ok(stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(roles, ["drafts", "sent", "trash"]);

    // Read state is kept locally and survives a re-sync.
    core.perform_action(PerformActionArgs {
        kind: ActionKind::MarkRead,
        thread_ids: vec![first.thread_id],
        params: None,
    })
    .await
    .unwrap();
    settle(&core, account.id).await;
    assert!(
        rows(&core)
            .await
            .iter()
            .find(|r| r.subject == "First post")
            .unwrap()
            .is_read
    );
}

#[tokio::test]
async fn later_syncs_fetch_only_new_articles() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 3);
    let fetched_before = log(&shared)
        .iter()
        .filter(|c| c.starts_with("ARTICLE"))
        .count();
    assert_eq!(fetched_before, 3);

    shared
        .lock()
        .unwrap()
        .groups
        .get_mut("test.group")
        .unwrap()
        .articles
        .insert(4, article("Newest", "a4@example.org", "", "fresh"));
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 4);
    let articles: Vec<String> = log(&shared)
        .into_iter()
        .filter(|c| c.starts_with("ARTICLE"))
        .collect();
    assert_eq!(articles.len(), 4, "{articles:?}");
    assert_eq!(articles.last().unwrap(), "ARTICLE 4");
}

#[tokio::test]
async fn works_without_over_and_ignores_malformed_overview_lines() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    shared.lock().unwrap().no_over = true;
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    // Missing numbers (423) are skipped; the three real articles import.
    assert_eq!(rows(&core).await.len(), 3);

    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    shared.lock().unwrap().junk_overview = true;
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 3);
}

#[tokio::test]
async fn login_is_required_and_checked() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    shared.lock().unwrap().credentials = Some(("reader".into(), "s3cret".into()));
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;

    let mut wrong = args(port, "test.group", ConnectionSecurity::None);
    wrong.username = "reader".into();
    wrong.password = "nope".into();
    let result = core.test_connection(&wrong).await;
    assert!(!result.ok, "wrong password must fail");

    let anonymous = args(port, "test.group", ConnectionSecurity::None);
    assert!(!core.test_connection(&anonymous).await.ok);

    let mut right = args(port, "test.group", ConnectionSecurity::None);
    right.username = "reader".into();
    right.password = "s3cret".into();
    assert!(core.test_connection(&right).await.ok);
    let account = core.add_account_password(right).await.unwrap();
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 3);
}

#[tokio::test]
async fn implicit_tls_and_starttls_connect_and_verify_the_certificate() {
    for wire in [Wire::ImplicitTls, Wire::StartTls] {
        let temp = tempfile::tempdir().unwrap();
        let shared = basic_server();
        let port = serve(wire, shared.clone()).await;
        let security = if wire == Wire::ImplicitTls {
            ConnectionSecurity::Tls
        } else {
            ConnectionSecurity::Starttls
        };
        let core = start(temp.path()).await;
        let account = core
            .add_account_password(args(port, "test.group", security))
            .await
            .unwrap();
        settle(&core, account.id).await;
        assert_eq!(rows(&core).await.len(), 3);
        if wire == Wire::StartTls {
            assert!(log(&shared).iter().any(|c| c == "STARTTLS"));
        }

        // Without trusting the fixture certificate the handshake must fail.
        let mut untrusted = args(port, "test.group", security);
        untrusted.connection.trusted_certificate_pem.clear();
        assert!(!core.test_connection(&untrusted).await.ok);
        // ...unless the user accepts untrusted certificates for this account.
        untrusted.connection.accept_invalid_certs = true;
        assert!(core.test_connection(&untrusted).await.ok);
    }
}

#[tokio::test]
async fn rejected_starttls_never_sends_the_password() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    {
        let mut server = shared.lock().unwrap();
        server.refuse_starttls = true;
        server.credentials = Some(("reader".into(), "s3cret".into()));
    }
    let port = serve(Wire::StartTls, shared.clone()).await;
    let core = start(temp.path()).await;
    let mut request = args(port, "test.group", ConnectionSecurity::Starttls);
    request.username = "reader".into();
    request.password = "s3cret".into();
    let result = core.test_connection(&request).await;
    assert!(!result.ok);
    assert!(!log(&shared).iter().any(|c| c.starts_with("AUTHINFO")));
}

#[tokio::test]
async fn missing_and_invalid_groups_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared).await;
    let core = start(temp.path()).await;
    let missing = core
        .test_connection(&args(
            port,
            "test.group no.such.group",
            ConnectionSecurity::None,
        ))
        .await;
    assert!(!missing.ok);
    assert!(missing.error.unwrap().contains("no.such.group"));
    let invalid = core
        .test_connection(&args(port, "bad/group", ConnectionSecurity::None))
        .await;
    assert!(invalid.error.unwrap().contains("bad/group"));
    let empty = core
        .test_connection(&args(port, "  ", ConnectionSecurity::None))
        .await;
    assert!(!empty.ok);
}

#[tokio::test]
async fn very_large_articles_are_header_only_until_opened() {
    let temp = tempfile::tempdir().unwrap();
    let big_body = format!("BIGMARK {}", "x".repeat(1_200_000));
    let shared = server_with(&[(
        "test.group",
        vec![
            (1, article("Small", "s1@example.org", "", "tiny")),
            (2, article("Huge", "h1@example.org", "", &big_body)),
        ],
    )]);
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let all = rows(&core).await;
    let huge = all.iter().find(|r| r.subject == "Huge").unwrap();
    assert_eq!(huge.body_state, "none");
    assert!(
        huge.body
            .as_deref()
            .unwrap()
            .contains("Open it to download")
    );
    let commands = log(&shared);
    assert!(commands.iter().any(|c| c == "HEAD 2"));
    assert!(!commands.iter().any(|c| c == "ARTICLE 2"));

    // Opening the message downloads the whole article.
    core.get_body(huge.id).await.unwrap();
    let mut loaded = false;
    for _ in 0..100 {
        let row = rows(&core)
            .await
            .into_iter()
            .find(|r| r.subject == "Huge")
            .unwrap();
        if row.body_state == "cached" && row.body.as_deref().is_some_and(|b| b.contains("BIGMARK"))
        {
            loaded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(loaded, "the full article should be downloaded when opened");
}

#[tokio::test]
async fn expired_articles_are_removed_but_starred_ones_stay() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let all = rows(&core).await;
    // Starring a thread stars its newest article.
    let reply = all.iter().find(|r| r.subject == "Re: First post").unwrap();
    core.perform_action(PerformActionArgs {
        kind: ActionKind::Star,
        thread_ids: vec![reply.thread_id],
        params: None,
    })
    .await
    .unwrap();
    {
        let mut server = shared.lock().unwrap();
        let group = server.groups.get_mut("test.group").unwrap();
        group.articles.remove(&1);
        group.articles.remove(&2);
    }
    settle(&core, account.id).await;
    let subjects: Vec<String> = rows(&core).await.into_iter().map(|r| r.subject).collect();
    assert_eq!(subjects, ["Dots", "Re: First post"], "{subjects:?}");
}

#[tokio::test]
async fn unsubscribing_removes_the_group_and_its_articles() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(
            port,
            "test.group other.group",
            ConnectionSecurity::None,
        ))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 4);
    let again = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    assert_eq!(again.id, account.id);
    settle(&core, account.id).await;
    let all = rows(&core).await;
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|r| r.folder == "test.group"));
}

#[tokio::test]
async fn posts_new_articles_and_followups() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;

    let draft = |to: Vec<Address>, subject: &str, parent: Option<i64>| SaveDraftArgs {
        draft_id: None,
        account_id: account.id,
        from: None,
        to,
        cc: vec![],
        bcc: vec![Address {
            name: None,
            email: "hidden@example.org".into(),
        }],
        subject: subject.into(),
        body_text: ".starts with a dot\nsecond line".into(),
        body_html: Some("<p>html</p>".into()),
        mode: "new".into(),
        in_reply_to_message_id: parent,
        attachments: vec![],
    };
    let now = || chrono::Utc::now().timestamp_millis();

    // A new article addressed to group@news.
    let id = core
        .save_draft(draft(
            vec![Address {
                name: None,
                email: "test.group@news".into(),
            }],
            "Brand new topic",
            None,
        ))
        .await
        .unwrap();
    core.queue_send(QueueSendArgs {
        draft_id: id,
        send_at: Some(now()),
    })
    .await
    .unwrap();
    wait_posted(&shared, 1).await;
    let posted = shared.lock().unwrap().posted[0].clone();
    assert!(posted.contains("Newsgroups: test.group"), "{posted}");
    assert!(posted.contains("Subject: Brand new topic"));
    assert!(
        !posted.to_ascii_lowercase().contains("hidden@example.org"),
        "Bcc leaked"
    );
    assert!(
        !posted.contains("\r\nTo:"),
        "no To header on a news article"
    );
    assert!(
        posted.contains("\r\n.starts with a dot"),
        "dot-unstuffed on the wire"
    );
    assert!(!posted.contains("multipart/alternative"), "plain text only");

    // A follow-up with no addressee goes to the article's own group.
    let first = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "First post")
        .unwrap();
    let followup = core
        .save_draft(draft(vec![], "Re: First post", Some(first.id)))
        .await
        .unwrap();
    core.queue_send(QueueSendArgs {
        draft_id: followup,
        send_at: Some(now()),
    })
    .await
    .unwrap();
    wait_posted(&shared, 2).await;
    let posted = shared.lock().unwrap().posted[1].clone();
    assert!(posted.contains("Newsgroups: test.group"), "{posted}");
    assert!(posted.contains("References: <a1@example.org>"), "{posted}");

    // The posted articles end up in the local Sent folder.
    let mut sent = 0;
    for _ in 0..50 {
        sent = core
            .db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM messages m JOIN folders f ON f.id=m.folder_id
                     WHERE f.role='sent' AND m.is_draft=0",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap();
        if sent == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(sent, 2);
}

#[tokio::test]
async fn posting_without_a_newsgroup_fails_permanently() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(port, "test.group", ConnectionSecurity::None))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let id = core
        .save_draft(SaveDraftArgs {
            draft_id: None,
            account_id: account.id,
            from: None,
            to: vec![Address {
                name: None,
                email: "someone@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "Mail, not news".into(),
            body_text: "x".into(),
            body_html: None,
            mode: "new".into(),
            in_reply_to_message_id: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    let queued = core
        .queue_send(QueueSendArgs {
            draft_id: id,
            send_at: Some(chrono::Utc::now().timestamp_millis()),
        })
        .await
        .unwrap();
    let mut state = String::new();
    for _ in 0..100 {
        state = core
            .db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT state FROM pending_actions WHERE id=?1",
                    [queued.action_id],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        if state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(state, "failed");
    assert!(shared.lock().unwrap().posted.is_empty());
}

#[tokio::test]
async fn the_group_list_can_be_browsed() {
    let temp = tempfile::tempdir().unwrap();
    let shared = basic_server();
    let port = serve(Wire::Plain, shared).await;
    let core = start(temp.path()).await;
    let mut groups = core
        .list_news_groups(&args(port, "", ConnectionSecurity::None), None)
        .await
        .unwrap();
    groups.sort();
    assert_eq!(
        groups,
        [
            (
                "other.group".to_owned(),
                "Description of other.group".to_owned()
            ),
            (
                "test.group".to_owned(),
                "Description of test.group".to_owned()
            ),
        ]
    );
}

async fn wait_posted(shared: &Shared, count: usize) {
    for _ in 0..150 {
        if shared.lock().unwrap().posted.len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the article was never posted");
}
