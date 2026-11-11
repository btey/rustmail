//! Exchange Online (Microsoft Graph) accounts against an in-process fake
//! login + Graph server: sign-in verification, folder tree, delta sync
//! (add, change, remove, move), queued actions, throttling and sending.
//! Nothing here contacts Microsoft.

use rustmail_core::Core;
use rustmail_core::accounts::secrets::DevelopmentFileCredentialStore;
use rustmail_core::config::Paths;
use rustmail_core::error::CoreError;
use rustmail_core::exchange::auth::TokenSet;
use rustmail_core::exchange::{AddExchangeAccountArgs, ExchangeCloud, ExchangeSettings};
use rustmail_core::models::*;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct Msg {
    id: String,
    folder: &'static str,
    read: bool,
    flagged: bool,
    /// Comma-separated category names.
    categories: &'static str,
    raw: String,
}

#[derive(Default)]
struct State {
    messages: Vec<Msg>,
    /// Requests seen: "METHOD path?query" plus the body.
    log: Vec<(String, String)>,
    /// Changes the next delta of a folder will report: (folder, item json).
    pending: Vec<(&'static str, String)>,
    /// Answer the next N Graph requests with 429.
    throttle: usize,
    /// Number of times the token endpoint was called.
    token_calls: usize,
    /// Reject Graph requests (simulates a revoked grant) once set.
    expired_delta: bool,
    /// Folder ids the listing leaves out (a folder deleted on the server).
    hidden_folders: Vec<&'static str>,
    /// Draft ids the server holds, and counter for new ids.
    drafts: Vec<String>,
    next_id: usize,
    /// Message ids a `$search` returns.
    search_hits: Vec<&'static str>,
    /// Contacts and events the first delta returns, then the changes.
    contacts: Vec<String>,
    contact_changes: Vec<String>,
    events: Vec<String>,
    event_changes: Vec<String>,
    /// Bytes received by the upload session (Content-Range header values).
    uploads: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

fn mail(subject: &str, from: &str) -> String {
    format!(
        "From: {from}\r\nTo: me@example.org\r\nSubject: {subject}\r\nMessage-ID: <{}@example.org>\r\nDate: Mon, 01 Jan 2026 10:00:00 +0000\r\n\r\nbody of {subject}\r\n",
        subject.replace(' ', "-")
    )
}

fn message_json(m: &Msg) -> String {
    let subject = m
        .raw
        .lines()
        .find_map(|l| l.strip_prefix("Subject: "))
        .unwrap_or("");
    format!(
        r#"{{"id":"{}","subject":"{}","isRead":{},"isDraft":false,"flag":{{"flagStatus":"{}"}},"categories":[{}],"receivedDateTime":"2026-01-01T10:00:00Z","hasAttachments":false,"parentFolderId":"{}"}}"#,
        m.id,
        subject,
        m.read,
        if m.flagged { "flagged" } else { "notFlagged" },
        m.categories
            .split(',')
            .filter(|name| !name.is_empty())
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(","),
        m.folder
    )
}

const FOLDERS: [(&str, &str, &str); 4] = [
    ("inbox", "Inbox", "inbox"),
    ("sent", "Sent Items", "sentitems"),
    ("trash", "Deleted Items", "deleteditems"),
    ("junk", "Junk Email", "junkemail"),
];

async fn serve(shared: Shared) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let root = base.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let shared = shared.clone();
            let root = root.clone();
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p;
                    }
                };
                let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let target = first.next().unwrap().to_owned();
                let length = lines
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = data[head_end + 4..].to_vec();
                while body.len() < length {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                let body = String::from_utf8_lossy(&body).into_owned();
                let range = head
                    .split("\r\n")
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .starts_with("content-range:")
                            .then(|| line.split_once(':').unwrap().1.trim().to_owned())
                    })
                    .unwrap_or_default();
                if method == "PUT" && target.starts_with("/upload/") {
                    shared.lock().unwrap().uploads.push(range.clone());
                    let done = range
                        .split_once('-')
                        .and_then(|(_, rest)| rest.split_once('/'))
                        .is_some_and(|(end, total)| {
                            end.parse::<u64>().ok().map(|e| e + 1) == total.parse::<u64>().ok()
                        });
                    let status = if done { 201 } else { 200 };
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    return;
                }
                let (status, extra, payload, content_type) =
                    route(&shared, &root, &method, &target, &body);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n{extra}Connection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&payload).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    base
}

fn route(
    shared: &Shared,
    root: &str,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, String, Vec<u8>, &'static str) {
    let json =
        |status: u16, text: String| (status, String::new(), text.into_bytes(), "application/json");
    let mut state = shared.lock().unwrap();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    state
        .log
        .push((format!("{method} {target}"), body.to_owned()));
    if path.ends_with("/oauth2/token") {
        // The classic (v1) endpoint: numbers as strings, a space-separated scope.
        state.token_calls += 1;
        let n = state.token_calls;
        return json(
            200,
            format!(
                r#"{{"access_token":"ACCESS{n}","refresh_token":"REFRESH{n}","expires_in":"3600","scope":"Mail.ReadWrite Mail.Send","resource":"graph"}}"#
            ),
        );
    }
    if path.ends_with("/oauth2/v2.0/token") {
        state.token_calls += 1;
        let n = state.token_calls;
        return json(
            200,
            format!(
                r#"{{"access_token":"ACCESS{n}","refresh_token":"REFRESH{n}","expires_in":3600}}"#
            ),
        );
    }
    if state.throttle > 0 {
        state.throttle -= 1;
        return (
            429,
            "Retry-After: 0\r\n".into(),
            b"{}".to_vec(),
            "application/json",
        );
    }
    let Some(rest) = path
        .strip_prefix("/graph/v1.0/me")
        .or_else(|| path.strip_prefix("/graph/v1.0/users/shared@example.org"))
    else {
        return json(404, r#"{"error":{"code":"NotFound","message":"x"}}"#.into());
    };
    match (method, rest) {
        ("GET", "/mailFolders") => {
            let hidden = state.hidden_folders.clone();
            let items: Vec<String> = FOLDERS
                .iter()
                .filter(|(id, _, _)| !hidden.contains(id))
                .map(|(id, name, _)| {
                    format!(r#"{{"id":"{id}","displayName":"{name}","childFolderCount":0}}"#)
                })
                .collect();
            json(200, format!(r#"{{"value":[{}]}}"#, items.join(",")))
        }
        ("GET", p) if p.starts_with("/mailFolders/") && !p.contains("/messages") => {
            let name = p.trim_start_matches("/mailFolders/");
            match FOLDERS
                .iter()
                .find(|(_, _, well_known)| *well_known == name)
            {
                Some((id, _, _)) => json(200, format!(r#"{{"id":"{id}"}}"#)),
                None => json(
                    404,
                    r#"{"error":{"code":"ErrorItemNotFound","message":"none"}}"#.into(),
                ),
            }
        }
        ("GET", p) if p.ends_with("/messages/delta") => {
            let folder = p
                .trim_start_matches("/mailFolders/")
                .trim_end_matches("/messages/delta")
                .to_owned();
            if state.expired_delta && query.contains("deltatoken") {
                state.expired_delta = false;
                return json(
                    410,
                    r#"{"error":{"code":"syncStateNotFound","message":"gone"}}"#.into(),
                );
            }
            let mut items: Vec<String> = Vec::new();
            if query.contains("deltatoken") {
                let (mine, rest): (Vec<_>, Vec<_>) =
                    state.pending.drain(..).partition(|(f, _)| *f == folder);
                state.pending = rest;
                items.extend(mine.into_iter().map(|(_, json)| json));
            } else {
                items.extend(
                    state
                        .messages
                        .iter()
                        .filter(|m| m.folder == folder)
                        .map(message_json),
                );
            }
            json(
                200,
                format!(
                    r#"{{"value":[{}],"@odata.deltaLink":"{root}/graph/v1.0/me/mailFolders/{folder}/messages/delta?$deltatoken=T"}}"#,
                    items.join(",")
                ),
            )
        }
        ("GET", p) if p.starts_with("/messages/") && !p.contains("/$value") => {
            let id = p.trim_start_matches("/messages/");
            match state.messages.iter().find(|m| m.id == id) {
                Some(m) => json(
                    200,
                    format!(r#"{{"id":"{id}","parentFolderId":"{}"}}"#, m.folder),
                ),
                None => json(
                    404,
                    r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#.into(),
                ),
            }
        }
        ("GET", p) if p.starts_with("/messages/") && p.ends_with("/$value") => {
            let id = p
                .trim_start_matches("/messages/")
                .trim_end_matches("/$value");
            match state.messages.iter().find(|m| m.id == id) {
                Some(m) => (
                    200,
                    String::new(),
                    m.raw.clone().into_bytes(),
                    "message/rfc822",
                ),
                None => json(
                    404,
                    r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#.into(),
                ),
            }
        }
        ("PATCH", p) if p.starts_with("/messages/") => {
            let id = p.trim_start_matches("/messages/").to_owned();
            if let Some(m) = state.messages.iter_mut().find(|m| m.id == id) {
                if body.contains("\"isRead\":true") {
                    m.read = true;
                }
                if body.contains("\"isRead\":false") {
                    m.read = false;
                }
                if body.contains("\"flagged\"") {
                    m.flagged = true;
                }
            }
            json(200, "{}".into())
        }
        ("POST", p) if p.ends_with("/move") => {
            let id = p
                .trim_start_matches("/messages/")
                .trim_end_matches("/move")
                .to_owned();
            let dest = body
                .split("\"destinationId\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_owned();
            let dest: &'static str = FOLDERS
                .iter()
                .find(|(f, _, _)| *f == dest)
                .map(|(f, _, _)| *f)
                .unwrap_or("inbox");
            if let Some(m) = state.messages.iter_mut().find(|m| m.id == id) {
                m.folder = dest;
            }
            json(201, format!(r#"{{"id":"{id}"}}"#))
        }
        ("POST", "/sendMail") => (202, String::new(), Vec::new(), "text/plain"),
        ("GET", "/messages") if query.contains("search") => {
            let hits = state.search_hits.clone();
            let items: Vec<String> = state
                .messages
                .iter()
                .filter(|m| hits.contains(&m.id.as_str()))
                .map(message_json)
                .collect();
            json(200, format!(r#"{{"value":[{}]}}"#, items.join(",")))
        }
        ("POST", "/messages") => {
            state.next_id += 1;
            let id = format!("D{}", state.next_id);
            state.drafts.push(id.clone());
            json(201, format!(r#"{{"id":"{id}"}}"#))
        }
        ("POST", p) if p.ends_with("/permanentDelete") => {
            let id = p
                .trim_start_matches("/messages/")
                .trim_end_matches("/permanentDelete")
                .to_owned();
            state.drafts.retain(|draft| *draft != id);
            (204, String::new(), Vec::new(), "text/plain")
        }
        ("POST", p) if p.ends_with("/attachments") => json(201, "{}".into()),
        ("POST", p) if p.ends_with("/createUploadSession") => {
            json(200, format!(r#"{{"uploadUrl":"{root}/upload/session1"}}"#))
        }
        ("POST", p) if p.starts_with("/messages/") && p.ends_with("/send") => {
            (202, String::new(), Vec::new(), "text/plain")
        }
        ("GET", "/contactFolders") => json(
            200,
            r#"{"value":[{"id":"CF1","displayName":"Contacts"}]}"#.into(),
        ),
        ("GET", "/contactFolders/CF1/contacts/delta") => {
            let items: Vec<String> = if query.contains("deltatoken") {
                std::mem::take(&mut state.contact_changes)
            } else {
                state.contacts.clone()
            };
            json(
                200,
                format!(
                    r#"{{"value":[{}],"@odata.deltaLink":"{root}/graph/v1.0/me/contactFolders/CF1/contacts/delta?$deltatoken=C"}}"#,
                    items.join(",")
                ),
            )
        }
        ("GET", "/calendars") => json(
            200,
            r#"{"value":[{"id":"CAL1","name":"Calendar","isDefaultCalendar":true,"canEdit":true}]}"#
                .into(),
        ),
        ("GET", "/calendars/CAL1/calendarView/delta") => {
            let items: Vec<String> = if query.contains("deltatoken") {
                std::mem::take(&mut state.event_changes)
            } else {
                state.events.clone()
            };
            json(
                200,
                format!(
                    r#"{{"value":[{}],"@odata.deltaLink":"{root}/graph/v1.0/me/calendars/CAL1/calendarView/delta?$deltatoken=E"}}"#,
                    items.join(",")
                ),
            )
        }
        ("POST", "/calendars/CAL1/events") => json(201, r#"{"id":"EV-NEW"}"#.into()),
        ("PATCH", p) if p.starts_with("/events/") => json(200, "{}".into()),
        ("DELETE", p) if p.starts_with("/events/") => {
            (204, String::new(), Vec::new(), "text/plain")
        }
        ("POST", p) if p.starts_with("/events/") => (202, String::new(), Vec::new(), "text/plain"),
        _ => json(
            404,
            r#"{"error":{"code":"NotFound","message":"unknown"}}"#.into(),
        ),
    }
}

fn settings(base: &str) -> ExchangeSettings {
    ExchangeSettings {
        enabled: true,
        client_id: "facd6cff-a294-4415-b59f-c5b01937d7bd".into(),
        cloud: ExchangeCloud::Custom,
        custom_login_url: base.into(),
        custom_graph_url: format!("{base}/graph"),
        ..ExchangeSettings::default()
    }
}

fn tokens() -> TokenSet {
    TokenSet {
        access_token: "ACCESS0".into(),
        refresh_token: Some("REFRESH0".into()),
        expires_in: Some(3600),
        scope: None,
    }
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

async fn add(core: &Core, base: &str) -> Account {
    core.add_exchange_account(
        AddExchangeAccountArgs {
            email: "me@example.org".into(),
            display_name: Some("Me".into()),
            exchange: settings(base),
        },
        tokens(),
    )
    .await
    .unwrap()
}

async fn rows(core: &Core) -> Vec<(String, String, bool, bool)> {
    core.db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.subject, COALESCE(f.role, f.imap_name), m.is_read, m.is_starred
                 FROM messages m JOIN folders f ON f.id = m.folder_id ORDER BY m.subject",
            )?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

fn seeded() -> Shared {
    let state = State {
        messages: vec![
            Msg {
                id: "M1".into(),
                folder: "inbox",
                read: false,
                flagged: false,
                categories: "Red,Blue team",
                raw: mail("One", "a@example.org"),
            },
            Msg {
                id: "M2".into(),
                folder: "inbox",
                read: true,
                flagged: true,
                categories: "",
                raw: mail("Two", "b@example.org"),
            },
            Msg {
                id: "M3".into(),
                folder: "sent",
                read: true,
                flagged: false,
                categories: "",
                raw: mail("Three", "me@example.org"),
            },
        ],
        ..State::default()
    };
    Arc::new(Mutex::new(state))
}

fn log_has(shared: &Shared, needle: &str) -> bool {
    shared
        .lock()
        .unwrap()
        .log
        .iter()
        .any(|(line, _)| line.contains(needle))
}

#[tokio::test]
async fn first_sync_reads_folders_roles_flags_and_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    assert_eq!(account.mail_protocol, MailProtocol::Exchange);
    core.sync_now(Some(account.id)).await.unwrap();

    let all = rows(&core).await;
    assert_eq!(
        all,
        vec![
            ("One".into(), "inbox".into(), false, false),
            ("Three".into(), "sent".into(), true, false),
            ("Two".into(), "inbox".into(), true, true),
        ],
        "{all:?}"
    );
    // Folders carry the well-known roles and are not editable here.
    let folders = core
        .db
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT imap_name, role, can_rename FROM folders ORDER BY imap_name")?;
            Ok(stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap();
    assert!(
        folders.contains(&("Deleted Items".into(), Some("trash".into()), false)),
        "{folders:?}"
    );
    assert!(
        folders.contains(&("Junk Email".into(), Some("spam".into()), false)),
        "{folders:?}"
    );
    // The cached body is readable without another request.
    let body: String = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT b.text_body FROM messages m JOIN message_bodies b ON b.message_id=m.id WHERE m.subject='One'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(body.contains("body of One"));
    // The first sync is limited to a recent window.
    assert!(
        shared
            .lock()
            .unwrap()
            .log
            .iter()
            .any(|(l, _)| l.contains("delta") && l.contains("receivedDateTime"))
    );
    // The sign-in was verified, then the refresh token was used.
    assert!(log_has(&shared, "GET /graph/v1.0/me/mailFolders/inbox?"));
}

#[tokio::test]
async fn later_syncs_use_the_delta_link_for_adds_flags_removals_and_moves() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let fetched_before = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(l, _)| l.contains("$value"))
        .count();
    assert_eq!(fetched_before, 3);

    let new_msg = Msg {
        id: "M4".into(),
        folder: "inbox",
        read: false,
        flagged: false,
        categories: "",
        raw: mail("Four", "d@example.org"),
    };
    {
        let mut state = shared.lock().unwrap();
        state.messages.push(new_msg.clone());
        state.pending.push(("inbox", message_json(&new_msg)));
        // One was read elsewhere; Two was deleted; Three moved to Deleted Items.
        state.messages[0].read = true;
        state.messages.retain(|m| m.id != "M2");
        let mut read = state.messages[0].clone();
        read.read = true;
        state.pending.push(("inbox", message_json(&read)));
        state.pending.push((
            "inbox",
            r#"{"id":"M2","@removed":{"reason":"deleted"}}"#.into(),
        ));
        let moved_index = state.messages.iter().position(|m| m.id == "M3").unwrap();
        state.messages[moved_index].folder = "trash";
        let moved = state.messages[moved_index].clone();
        state.pending.push(("trash", message_json(&moved)));
        state.pending.push((
            "sent",
            r#"{"id":"M3","@removed":{"reason":"changed"}}"#.into(),
        ));
    }
    core.sync_now(Some(account.id)).await.unwrap();
    let all = rows(&core).await;
    assert_eq!(
        all,
        vec![
            ("Four".into(), "inbox".into(), false, false),
            ("One".into(), "inbox".into(), true, false),
            ("Three".into(), "trash".into(), true, false),
        ],
        "{all:?}"
    );
    // Only the genuinely new message was downloaded again.
    let fetched = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(l, _)| l.contains("$value"))
        .count();
    assert_eq!(fetched, fetched_before + 1);
    assert!(log_has(&shared, "$deltatoken=T"));
}

#[tokio::test]
async fn categories_become_labels_and_follow_the_server() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let labelled = |core: &Core| {
        let db = core.db.clone();
        async move {
            db.read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT l.name FROM message_labels ml
                     JOIN labels l ON l.id = ml.label_id
                     JOIN messages m ON m.id = ml.message_id
                     WHERE m.subject = 'One' ORDER BY l.name",
                )?;
                Ok(stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(labelled(&core).await, ["Blue team", "Red"]);
    // One category is removed on the server.
    {
        let mut state = shared.lock().unwrap();
        state.messages[0].categories = "Red";
        let changed = state.messages[0].clone();
        state.pending.push(("inbox", message_json(&changed)));
    }
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(labelled(&core).await, ["Red"]);
}

#[tokio::test]
async fn a_folder_deleted_on_the_server_is_removed_with_its_cached_mail() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(rows(&core).await.len(), 3);
    let folders = |core: &Core| {
        let id = account.id;
        let db = core.db.clone();
        async move {
            db.read(move |conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM folders WHERE account_id = ?1",
                    [id],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
        }
    };
    let before = folders(&core).await;
    shared.lock().unwrap().hidden_folders.push("sent");
    core.sync_now(Some(account.id)).await.unwrap();
    let after = rows(&core).await;
    assert_eq!(after.len(), 2, "{after:?}");
    assert!(after.iter().all(|row| row.0 != "Three"), "{after:?}");
    assert_eq!(folders(&core).await, before - 1);
    // The next sync still works and nothing comes back.
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(rows(&core).await.len(), 2);
}

#[tokio::test]
async fn an_expired_delta_link_restarts_without_duplicating_messages() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    shared.lock().unwrap().expired_delta = true;
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(rows(&core).await.len(), 3);
}

#[tokio::test]
async fn the_resource_sign_in_refreshes_through_the_classic_endpoint() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = core
        .add_exchange_account(
            AddExchangeAccountArgs {
                email: "me@example.org".into(),
                display_name: None,
                exchange: ExchangeSettings {
                    endpoint: rustmail_core::exchange::settings::ExchangeEndpoint::Resource,
                    ..settings(&base)
                },
            },
            // Expires at once, so the first sync must refresh.
            TokenSet {
                expires_in: Some(1),
                ..tokens()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(rows(&core).await.len(), 3);
    let log = shared.lock().unwrap().log.clone();
    let refresh = log
        .iter()
        .find(|(line, _)| line.contains("/oauth2/token"))
        .expect("a refresh through the classic token endpoint");
    assert!(
        refresh.1.contains("grant_type=refresh_token"),
        "{}",
        refresh.1
    );
    assert!(refresh.1.contains("resource="), "{}", refresh.1);
    assert!(!refresh.1.contains("scope="), "{}", refresh.1);
    assert!(!log_has(&shared, "/oauth2/v2.0/token"));
}

#[tokio::test]
async fn throttled_requests_are_retried() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    shared.lock().unwrap().throttle = 3;
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(rows(&core).await.len(), 3);
}

#[tokio::test]
async fn read_star_and_move_actions_reach_graph() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let thread_of = |subject: &'static str| {
        let core = &core;
        async move {
            core.db
                .read(move |conn| {
                    Ok(conn.query_row(
                        "SELECT thread_id FROM messages WHERE subject=?1",
                        [subject],
                        |r| r.get::<_, i64>(0),
                    )?)
                })
                .await
                .unwrap()
        }
    };
    let one = thread_of("One").await;
    core.perform_action(PerformActionArgs {
        kind: ActionKind::MarkRead,
        thread_ids: vec![one],
        params: None,
    })
    .await
    .unwrap();
    core.perform_action(PerformActionArgs {
        kind: ActionKind::Trash,
        thread_ids: vec![one],
        params: None,
    })
    .await
    .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    {
        let state = shared.lock().unwrap();
        let m1 = state.messages.iter().find(|m| m.id == "M1").unwrap();
        assert!(m1.read, "read state reached the server");
        assert_eq!(m1.folder, "trash", "the move reached the server");
    }
    assert_eq!(
        rows(&core).await.iter().find(|r| r.0 == "One").unwrap().1,
        "trash"
    );
}

#[tokio::test]
async fn a_sent_message_goes_through_send_mail_and_keeps_no_imap_copy() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let draft = core
        .save_draft(SaveDraftArgs {
            draft_id: None,
            account_id: account.id,
            from: None,
            to: vec![Address {
                name: None,
                email: "to@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: "Outgoing".into(),
            body_text: "hello".into(),
            body_html: None,
            mode: "new".into(),
            in_reply_to_message_id: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    let queued = core
        .queue_send(QueueSendArgs {
            draft_id: draft,
            send_at: None,
        })
        .await
        .unwrap();
    core.wait_for_send(queued.action_id).await.unwrap();
    let sent = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .find(|(l, _)| l.starts_with("POST /graph/v1.0/me/sendMail"))
        .map(|(_, body)| body.clone())
        .expect("sendMail was called");
    use base64::Engine;
    let raw = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(sent.trim())
            .unwrap(),
    )
    .unwrap();
    assert!(
        raw.contains("Subject: Outgoing") && raw.contains("to@example.com"),
        "{raw}"
    );
    // No append-to-Sent action is queued: Microsoft 365 files its own copy.
    let appends: i64 = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM pending_actions WHERE kind='append_sent'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(appends, 0);
}

#[tokio::test]
async fn a_sign_in_that_graph_rejects_creates_no_account() {
    let temp = tempfile::tempdir().unwrap();
    let shared: Shared = Arc::default();
    shared.lock().unwrap().throttle = 0;
    // A server that answers every Graph call with 403.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"error":{"code":"ErrorAccessDenied","message":"denied"}}"#;
                let _ = stream
                    .write_all(format!("HTTP/1.1 403 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
            });
        }
    });
    let core = start(temp.path()).await;
    let result = core
        .add_exchange_account(
            AddExchangeAccountArgs {
                email: "me@example.org".into(),
                display_name: None,
                exchange: settings(&base),
            },
            tokens(),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("denied"));
    assert!(core.list_accounts().await.unwrap().is_empty());
    // Invalid settings never reach the network.
    let mut bad = settings(&base);
    bad.client_id = "nope".into();
    assert!(
        core.add_exchange_account(
            AddExchangeAccountArgs {
                email: "me@example.org".into(),
                display_name: None,
                exchange: bad
            },
            tokens()
        )
        .await
        .is_err()
    );
}

async fn wait_until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn new_draft(account_id: i64, draft_id: Option<i64>, subject: &str) -> SaveDraftArgs {
    SaveDraftArgs {
        draft_id,
        account_id,
        from: None,
        to: vec![Address {
            name: Some("Ann".into()),
            email: "ann@example.com".into(),
        }],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        body_text: "hello".into(),
        body_html: None,
        mode: "new".into(),
        in_reply_to_message_id: None,
        attachments: vec![],
    }
}

#[tokio::test]
async fn a_server_search_caches_the_hits_that_are_not_stored_yet() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    // A message the sync has not seen (it is not in any delta yet).
    shared.lock().unwrap().messages.push(Msg {
        id: "M9".into(),
        folder: "inbox",
        read: true,
        flagged: false,
        categories: "",
        raw: mail("Old invoice", "billing@example.org"),
    });
    shared.lock().unwrap().search_hits = vec!["M1", "M9"];
    let added = core.exchange_search(account.id, "invoice").await.unwrap();
    assert_eq!(added, 1, "only the hit that was not cached is added");
    let all = rows(&core).await;
    assert!(
        all.iter()
            .any(|row| row.0 == "Old invoice" && row.1 == "inbox"),
        "{all:?}"
    );
    assert!(
        log_has(&shared, "search="),
        "the server was asked to search"
    );
    // The same search again adds nothing.
    assert_eq!(
        core.exchange_search(account.id, "invoice").await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_draft_is_pushed_rebuilt_on_every_save_and_removed_when_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();

    let draft = core
        .save_draft(new_draft(account.id, None, "Plans v1"))
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    wait_until("the draft on the server", async || {
        shared.lock().unwrap().drafts == vec!["D1".to_string()]
    })
    .await;
    // The draft went up as MIME (so its Message-ID and headers are kept).
    use base64::Engine;
    let body = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .find(|(l, _)| l.starts_with("POST /graph/v1.0/me/messages"))
        .map(|(_, body)| body.clone())
        .unwrap();
    let raw = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap(),
    )
    .unwrap();
    assert!(
        raw.contains("Subject: Plans v1") && raw.contains("Message-ID:"),
        "{raw}"
    );

    // Saving again replaces the server copy.
    core.save_draft(new_draft(account.id, Some(draft), "Plans v2"))
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    wait_until("the replaced draft", async || {
        shared.lock().unwrap().drafts == vec!["D2".to_string()]
    })
    .await;
    assert!(log_has(
        &shared,
        "POST /graph/v1.0/me/messages/D1/permanentDelete"
    ));
    let mapped: String = core
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT graph_id FROM exchange_messages WHERE message_id=?1",
                [draft],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mapped, "D2");

    // Deleting the local draft deletes the server copy.
    core.delete_draft(draft).await.unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    wait_until("the server copy to go", async || {
        shared.lock().unwrap().drafts.is_empty()
    })
    .await;
    assert!(log_has(
        &shared,
        "POST /graph/v1.0/me/messages/D2/permanentDelete"
    ));
}

#[tokio::test]
async fn a_big_attachment_goes_through_an_upload_session_and_the_draft_is_sent() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    // 5 MiB: over the one-request limit, and 2 upload chunks.
    let file = temp.path().join("big.bin");
    std::fs::write(&file, vec![7u8; 5 * 1024 * 1024]).unwrap();
    let mut args = new_draft(account.id, None, "With a big file");
    args.attachments = vec![DraftAttachmentIn {
        file_path: file.to_string_lossy().into_owned(),
        filename: "big.bin".into(),
    }];
    let draft = core.save_draft(args).await.unwrap();
    let queued = core
        .queue_send(QueueSendArgs {
            draft_id: draft,
            send_at: None,
        })
        .await
        .unwrap();
    core.wait_for_send(queued.action_id).await.unwrap();
    // JSON draft, upload session, ranges, then the send; never sendMail.
    assert!(!log_has(&shared, "POST /graph/v1.0/me/sendMail"));
    assert!(log_has(&shared, "createUploadSession"));
    assert!(log_has(&shared, "/send"));
    let uploads = shared.lock().unwrap().uploads.clone();
    let total = 5 * 1024 * 1024;
    assert_eq!(uploads.len(), 2, "{uploads:?}");
    assert!(uploads[0].starts_with("bytes 0-") && uploads[0].ends_with(&format!("/{total}")));
    assert!(uploads[1].ends_with(&format!("-{}/{total}", total - 1)));
    let created = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .find(|(l, _)| l == "POST /graph/v1.0/me/messages")
        .map(|(_, body)| body.clone())
        .expect("a JSON draft was created");
    assert!(
        created.contains("\"subject\":\"With a big file\""),
        "{created}"
    );
    assert!(created.contains("ann@example.com"));
}

#[tokio::test]
async fn contacts_are_read_through_delta_and_removed_with_the_server_copy() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().contacts = vec![
        r#"{"id":"C1","changeKey":"k1","displayName":"Ann Lee","emailAddresses":[{"address":"Ann@Example.org"}],"companyName":"Acme","mobilePhone":"+41 79 000","jobTitle":"CTO"}"#.into(),
        r#"{"id":"C2","displayName":"Nobody","emailAddresses":[]}"#.into(),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let ann = |core: &Core| {
        let db = core.db.clone();
        async move {
            db.read(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT name, company, phone, job_title FROM contacts WHERE email='ann@example.org'",
                        [],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .ok())
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(
        ann(&core).await,
        Some((
            "Ann Lee".into(),
            "Acme".into(),
            "+41 79 000".into(),
            "CTO".into()
        ))
    );
    // It is available to the composer's address completion.
    let suggestions = core
        .list_contacts("ann".into(), Some(account.id), 5)
        .await
        .unwrap();
    assert!(suggestions.iter().any(|c| c.email == "ann@example.org"));

    shared.lock().unwrap().contact_changes =
        vec![r#"{"id":"C1","@removed":{"reason":"deleted"}}"#.into()];
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(ann(&core).await, None);
}

#[tokio::test]
async fn calendar_events_arrive_with_zones_applied_and_local_changes_and_answers_go_back() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().events = vec![
        r#"{"id":"EV1","iCalUId":"UID1","type":"singleInstance","subject":"Planning","isAllDay":false,"isCancelled":false,
            "organizer":{"emailAddress":{"address":"boss@example.org","name":"Boss"}},
            "attendees":[{"emailAddress":{"address":"me@example.org","name":"Me"},"status":{"response":"none"}}],
            "start":{"dateTime":"2026-07-01T12:00:00.0000000","timeZone":"W. Europe Standard Time"},
            "end":{"dateTime":"2026-07-01T13:00:00.0000000","timeZone":"W. Europe Standard Time"}}"#
            .replace('\n', ""),
        r#"{"id":"EV2","iCalUId":"UID2","type":"singleInstance","subject":"Holiday","isAllDay":true,"isCancelled":false,
            "start":{"dateTime":"2026-08-03T00:00:00.0000000","timeZone":"W. Europe Standard Time"},
            "end":{"dateTime":"2026-08-04T00:00:00.0000000","timeZone":"W. Europe Standard Time"}}"#
            .replace('\n', ""),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    let calendars = core.list_calendars(Some(account.id)).await.unwrap();
    assert_eq!(calendars.len(), 1, "the account's calendar was connected");
    assert!(calendars[0].is_default);

    let window = (
        chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
        chrono::NaiveDate::from_ymd_opt(2026, 9, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
    );
    core.calendar_sync_now(Some(account.id)).await;
    let mut events = Vec::new();
    for _ in 0..100 {
        events = core.list_events(window.0, window.1).await.unwrap();
        if events.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(events.len(), 2, "{events:?}");
    let planning = events
        .iter()
        .find(|e| e.summary.as_deref() == Some("Planning"))
        .unwrap();
    // 12:00 in Berlin in July is 10:00 UTC.
    assert_eq!(
        planning.starts_at,
        chrono::NaiveDate::from_ymd_opt(2026, 7, 1)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
    );
    let holiday = events
        .iter()
        .find(|e| e.summary.as_deref() == Some("Holiday"))
        .unwrap();
    assert!(holiday.all_day);
    assert_eq!(
        holiday.starts_at,
        chrono::NaiveDate::from_ymd_opt(2026, 8, 3)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
        "an all-day date is not moved by the zone"
    );

    // Answering the invitation goes to Microsoft 365, not by mail.
    core.rsvp_event(RsvpEventArgs {
        event_id: planning.id,
        response: "accepted".into(),
    })
    .await
    .unwrap();
    assert!(log_has(&shared, "POST /graph/v1.0/me/events/EV1/accept"));
    assert!(!log_has(&shared, "sendMail"));

    // A new local event is created on the server.
    core.create_event(CreateEventArgs {
        account_id: account.id,
        calendar_id: None,
        summary: "Lunch".into(),
        description: None,
        location: None,
        join_url: None,
        starts_at: window.0 + 86_400_000 * 40,
        ends_at: window.0 + 86_400_000 * 40 + 3_600_000,
        all_day: false,
        attendees: vec![],
    })
    .await
    .unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the event on the server", async || {
        log_has(&shared, "POST /graph/v1.0/me/calendars/CAL1/events")
    })
    .await;

    // Deleting a synced event deletes it there.
    core.delete_event(holiday.id, false).await.unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the delete on the server", async || {
        log_has(&shared, "DELETE /graph/v1.0/me/events/EV2")
    })
    .await;
}

#[tokio::test]
async fn an_account_is_edited_and_reconnected_and_a_new_mailbox_clears_the_cache() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let current = core.exchange_account_settings(account.id).await.unwrap();
    assert_eq!(current.client_id, "facd6cff-a294-4415-b59f-c5b01937d7bd");

    // A different tenant needs a new sign-in.
    let mut other_tenant = current.clone();
    other_tenant.tenant = "organizations".into();
    let refused = core
        .update_exchange_account(account.id, None, other_tenant.clone(), None)
        .await;
    assert!(
        matches!(&refused, Err(CoreError::Auth(text)) if text.contains("new sign-in")),
        "{refused:?}"
    );
    // ... and with one it is saved, and the old delta state is dropped.
    core.update_exchange_account(
        account.id,
        Some("Renamed".into()),
        other_tenant,
        Some(tokens()),
    )
    .await
    .unwrap();
    let saved = core.exchange_account_settings(account.id).await.unwrap();
    assert_eq!(saved.tenant, "organizations");
    let name: String = core
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT display_name FROM accounts WHERE id=?1",
                [account.id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(name, "Renamed");

    // Reconnecting after a revoked refresh token: same settings, new tokens.
    core.update_exchange_account(account.id, None, saved.clone(), Some(tokens()))
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();

    // A shared mailbox needs no new sign-in, but its mail is another set.
    let fetched_before = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(l, _)| l.contains("$value"))
        .count();
    let mut shared_box = saved;
    shared_box.shared_mailbox = "shared@example.org".into();
    // The shared-mailbox scopes were not consented to before.
    assert!(
        core.update_exchange_account(account.id, None, shared_box.clone(), None)
            .await
            .is_err()
    );
    core.update_exchange_account(account.id, None, shared_box, Some(tokens()))
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    assert!(log_has(
        &shared,
        "/graph/v1.0/users/shared@example.org/mailFolders"
    ));
    let fetched_after = shared
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(l, _)| l.contains("$value"))
        .count();
    assert!(
        fetched_after > fetched_before,
        "the cache was cleared and read again ({fetched_before} -> {fetched_after})"
    );
}
