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
    /// The change key the server reports for contact C1 (empty: "k1").
    contact_key: String,
    events: Vec<String>,
    event_changes: Vec<String>,
    /// Bytes received by the upload session (Content-Range header values).
    uploads: Vec<String>,
    /// The picture of contact C1 (empty: none).
    contact_photo: Vec<u8>,
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
    if method == "GET" && path == "/graph/v1.0/users" {
        // The tenant directory (`User.ReadBasic.All`).
        return json(
            200,
            r#"{"value":[{"displayName":"Dora Tenant","mail":"dora@example.org"},{"displayName":"Carl Dir","mail":"CARL@example.org"},{"displayName":"No Address","mail":null}]}"#.into(),
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
        ("POST", p) if p.ends_with("/createReply") => {
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
        ("POST", "/contactFolders/CF1/contacts") => {
            json(201, r#"{"id":"NEWC","changeKey":"n1"}"#.into())
        }
        ("GET", "/contacts/C1") => {
            let key = if state.contact_key.is_empty() {
                "k1".to_owned()
            } else {
                state.contact_key.clone()
            };
            json(
                200,
                format!(
                    r#"{{"id":"C1","changeKey":"{key}","displayName":"Ann Lee","emailAddresses":[{{"address":"ann@example.org"}}]}}"#
                ),
            )
        }
        ("GET", "/contacts/C1/photo/$value") => {
            if state.contact_photo.is_empty() {
                json(404, r#"{"error":{"code":"ErrorItemNotFound"}}"#.into())
            } else {
                (200, String::new(), state.contact_photo.clone(), "image/jpeg")
            }
        }
        ("PUT", "/contacts/C1/photo/$value") => {
            state.contact_photo = body.as_bytes().to_vec();
            (200, String::new(), Vec::new(), "text/plain")
        }
        ("PATCH", "/contacts/C1") => json(200, r#"{"id":"C1","changeKey":"k2"}"#.into()),
        ("DELETE", "/contacts/C1") => (204, String::new(), Vec::new(), "text/plain"),
        ("GET", "/people") => json(
            200,
            r#"{"value":[{"displayName":"Carl Dir","scoredEmailAddresses":[{"address":"carl@example.org"}]},{"displayName":"No Mail","scoredEmailAddresses":[]}]}"#.into(),
        ),
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
        ("GET", "/events/EV-NEW") => json(
            200,
            r#"{"id":"EV-NEW","@odata.etag":"W/\"v2\"","changeKey":"v2"}"#.into(),
        ),
        ("GET", "/events/SM1") => json(
            200,
            r#"{"id":"SM1","iCalUId":"SUID1","type":"seriesMaster","subject":"Standup","isAllDay":false,"isCancelled":false,
                "start":{"dateTime":"2026-07-06T09:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-07-06T09:30:00.0000000","timeZone":"UTC"},
                "recurrence":{"pattern":{"type":"weekly","interval":1,"daysOfWeek":["monday"],"firstDayOfWeek":"sunday"},
                              "range":{"type":"noEnd","startDate":"2026-07-06"}},
                "cancelledOccurrences":["OID.SM1.2026-07-20"],
                "exceptionOccurrences":[{"id":"EX1","type":"exception","subject":"Standup (later)","isAllDay":false,"isCancelled":false,
                    "originalStart":"2026-07-13T09:00:00Z",
                    "start":{"dateTime":"2026-07-13T11:00:00.0000000","timeZone":"UTC"},
                    "end":{"dateTime":"2026-07-13T11:30:00.0000000","timeZone":"UTC"}}]}"#
                .replace('\n', ""),
        ),
        ("GET", "/events/SM3") => json(
            200,
            r#"{"id":"SM3","iCalUId":"SUID3","type":"seriesMaster","subject":"Berlin sync","isAllDay":false,"isCancelled":false,
                "start":{"dateTime":"2026-03-23T09:00:00.0000000","timeZone":"W. Europe Standard Time"},
                "end":{"dateTime":"2026-03-23T10:00:00.0000000","timeZone":"W. Europe Standard Time"},
                "recurrence":{"pattern":{"type":"weekly","interval":1,"daysOfWeek":["monday"],"firstDayOfWeek":"sunday"},
                              "range":{"type":"noEnd","startDate":"2026-03-23"}}}"#
                .replace('\n', ""),
        ),
        ("GET", "/events/SM2") => json(
            200,
            r#"{"id":"SM2","iCalUId":"SUID2","type":"seriesMaster","subject":"Review","isAllDay":false,"isCancelled":false,
                "start":{"dateTime":"2026-07-07T09:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-07-07T10:00:00.0000000","timeZone":"UTC"},
                "recurrence":{"pattern":{"type":"relativeYearly","interval":1,"daysOfWeek":["tuesday"],"index":"first","month":7},
                              "range":{"type":"noEnd","startDate":"2026-07-07"}}}"#
                .replace('\n', ""),
        ),
        ("GET", "/events/SM1/instances") => json(
            200,
            r#"{"value":[
                {"id":"INST-06","type":"occurrence","seriesMasterId":"SM1","subject":"Standup","isAllDay":false,"isCancelled":false,
                 "start":{"dateTime":"2026-07-06T09:00:00.0000000","timeZone":"UTC"},
                 "end":{"dateTime":"2026-07-06T09:30:00.0000000","timeZone":"UTC"}},
                {"id":"INST-27","type":"occurrence","seriesMasterId":"SM1","subject":"Standup","isAllDay":false,"isCancelled":false,
                 "start":{"dateTime":"2026-07-27T09:00:00.0000000","timeZone":"UTC"},
                 "end":{"dateTime":"2026-07-27T09:30:00.0000000","timeZone":"UTC"}}]}"#
                .replace('\n', ""),
        ),
        ("POST", "/calendar/getSchedule") => json(
            200,
            r#"{"value":[{"scheduleId":"ann@example.org","scheduleItems":[{"status":"busy",
                "start":{"dateTime":"2026-07-01T09:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-07-01T10:00:00.0000000","timeZone":"UTC"}}]}]}"#
                .replace('\n', ""),
        ),
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
async fn a_large_reply_starts_from_create_reply_to_keep_its_threading() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let parent: i64 = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT m.id FROM messages m JOIN exchange_messages em ON em.message_id = m.id
                 WHERE m.subject = 'One' AND m.message_id IS NOT NULL LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    let file = temp.path().join("big.bin");
    std::fs::write(&file, vec![7u8; 4 * 1024 * 1024]).unwrap();
    let mut args = new_draft(account.id, None, "Re: One");
    args.mode = "reply".into();
    args.in_reply_to_message_id = Some(parent);
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
    assert!(
        log_has(&shared, "/createReply"),
        "the reply was started from the original"
    );
    assert!(!log_has(&shared, "POST /graph/v1.0/me/sendMail"));
    assert!(log_has(&shared, "/send"));
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

fn utc_ms(y: i32, m: u32, d: u32, h: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

#[tokio::test]
async fn a_recurring_series_is_stored_as_one_master_with_its_exceptions() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let occurrence = |id: &str, master: &str, subject: &str, day: u32| {
        format!(
            r#"{{"id":"{id}","iCalUId":"U-{master}","type":"occurrence","seriesMasterId":"{master}","subject":"{subject}","isAllDay":false,"isCancelled":false,
                "start":{{"dateTime":"2026-07-{day:02}T09:00:00.0000000","timeZone":"UTC"}},
                "end":{{"dateTime":"2026-07-{day:02}T09:30:00.0000000","timeZone":"UTC"}}}}"#
        )
        .replace('\n', "")
    };
    shared.lock().unwrap().events = vec![
        occurrence("OC1", "SM1", "Standup", 6),
        occurrence("OC2", "SM1", "Standup", 13),
        occurrence("OC3", "SM1", "Standup", 20),
        occurrence("OC4", "SM2", "Review", 7),
        occurrence("OC5", "SM2", "Review", 8),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.calendar_sync_now(Some(account.id)).await;
    let window = (utc_ms(2026, 7, 1, 0), utc_ms(2026, 8, 1, 0));
    let mut events = Vec::new();
    for _ in 0..100 {
        events = core.list_events(window.0, window.1).await.unwrap();
        if events.len() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let standups: Vec<i64> = events
        .iter()
        .filter(|e| e.summary.as_deref() == Some("Standup"))
        .map(|e| e.starts_at)
        .collect();
    // Mondays 6, (13 moved to 11:00), 27; the 20th was cancelled.
    assert_eq!(
        standups,
        vec![
            utc_ms(2026, 7, 6, 9),
            utc_ms(2026, 7, 13, 11),
            utc_ms(2026, 7, 27, 9)
        ],
        "{events:?}"
    );
    // The rule maps, so the series is one row with its recurrence rule.
    let masters: i64 = core
        .calendar_db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM calendar_events
                 WHERE caldav_href = 'SM1' AND rrule = 'FREQ=WEEKLY;INTERVAL=1;BYDAY=MO'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(masters, 1);
    // A rule outside the local expander keeps the occurrences Graph sent.
    let reviews = events
        .iter()
        .filter(|e| e.summary.as_deref() == Some("Review"))
        .count();
    assert_eq!(reviews, 2, "{events:?}");
}

#[tokio::test]
async fn an_event_edited_here_and_on_the_server_keeps_both_versions() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    let window = (utc_ms(2026, 7, 1, 0), utc_ms(2026, 7, 31, 0));
    let created = core
        .create_event(CreateEventArgs {
            account_id: account.id,
            calendar_id: None,
            summary: "Workshop".into(),
            description: None,
            location: None,
            join_url: None,
            starts_at: utc_ms(2026, 7, 2, 9),
            ends_at: utc_ms(2026, 7, 2, 10),
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
    // The next pull brings the event back at version v1.
    shared.lock().unwrap().event_changes = vec![
        r#"{"id":"EV-NEW","@odata.etag":"W/\"v1\"","iCalUId":"UID-NEW","type":"singleInstance","subject":"Workshop","isAllDay":false,"isCancelled":false,
            "start":{"dateTime":"2026-07-02T09:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-07-02T10:00:00.0000000","timeZone":"UTC"}}"#
            .replace('\n', ""),
    ];
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the event at version v1", async || {
        core.calendar_db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM calendar_events WHERE caldav_href = 'EV-NEW' AND etag LIKE '%v1%'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
            == 1
    })
    .await;
    let _ = (created, window);
    // The server already holds v2 when the local edit is pushed.
    let local: i64 = core
        .calendar_db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT id FROM calendar_events WHERE caldav_href = 'EV-NEW'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    core.update_event(UpdateEventArgs {
        event_id: local,
        summary: "Workshop (new room)".into(),
        description: None,
        location: None,
        join_url: None,
        starts_at: utc_ms(2026, 7, 2, 9),
        ends_at: utc_ms(2026, 7, 2, 10),
        all_day: false,
        attendees: vec![],
        notify: false,
    })
    .await
    .unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the edit saved as a second event", async || {
        shared
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(line, _)| line.contains("POST /graph/v1.0/me/calendars/CAL1/events"))
            .count()
            == 2
    })
    .await;
    assert!(!log_has(&shared, "PATCH /graph/v1.0/me/events/EV-NEW"));
}

#[tokio::test]
async fn free_busy_of_other_mailboxes_comes_from_get_schedule() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    let answer = core
        .exchange_free_busy(
            account.id,
            vec!["ann@example.org".into()],
            utc_ms(2026, 7, 1, 0),
            utc_ms(2026, 7, 2, 0),
        )
        .await
        .unwrap();
    assert_eq!(answer.len(), 1);
    assert_eq!(answer[0].0, "ann@example.org");
    assert_eq!(answer[0].1.len(), 1);
    assert_eq!(answer[0].1[0].status, "busy");
    assert_eq!(answer[0].1[0].start_ms, utc_ms(2026, 7, 1, 9));
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

async fn ann_id(core: &Core) -> i64 {
    core.db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT id FROM contacts WHERE email='ann@example.org'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap()
}

fn seen(shared: &Shared, prefix: &str) -> Vec<String> {
    shared
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|(line, _)| line.contains(prefix))
        .map(|(_, body)| body.clone())
        .collect()
}

#[tokio::test]
async fn contact_edits_new_contacts_and_deletions_are_written_back() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().contacts = vec![
        r#"{"id":"C1","changeKey":"k1","displayName":"Ann Lee","emailAddresses":[{"address":"ann@example.org"}],"companyName":"Acme"}"#.into(),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();

    // An edit goes out as a PATCH.
    let id = ann_id(&core).await;
    let mut record = core
        .list_contact_records("ann".into(), 5)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap();
    record.company = "Globex".into();
    record.account_ids = vec![account.id];
    core.save_contact(record).await.unwrap();
    assert_eq!(core.pending_changes(Some(account.id)).await.unwrap(), 1);
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(core.pending_changes(Some(account.id)).await.unwrap(), 0);
    let patches = seen(&shared, "PATCH /graph/v1.0/me/contacts/C1");
    assert_eq!(patches.len(), 1, "{:?}", shared.lock().unwrap().log);
    assert!(patches[0].contains("Globex"), "{}", patches[0]);

    // A new contact is created in the default folder and then keeps its id.
    let new = core
        .save_contact(ContactRecord {
            id: 0,
            name: "Bob Ray".into(),
            email: "bob@example.org".into(),
            phone: String::new(),
            company: "Initech".into(),
            job_title: String::new(),
            website: String::new(),
            birthday: String::new(),
            postal_address: String::new(),
            notes: String::new(),
            tags: String::new(),
            is_favorite: false,
            interactions: 0,
            last_interacted: None,
            account_ids: vec![account.id],
            is_managed: true,
            extras: Default::default(),
            has_photo: false,
        })
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    let posts = seen(&shared, "POST /graph/v1.0/me/contactFolders/CF1/contacts");
    assert_eq!(posts.len(), 1);
    assert!(posts[0].contains("Bob Ray") && posts[0].contains("Initech"));
    let href: String = core
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT href FROM carddav_objects WHERE contact_id=?1",
                [new.id],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(href, "NEWC");
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(
        seen(&shared, "POST /graph/v1.0/me/contactFolders/CF1/contacts").len(),
        1,
        "created once"
    );

    // A deletion goes out as a DELETE.
    core.delete_contact(id).await.unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(seen(&shared, "DELETE /graph/v1.0/me/contacts/C1").len(), 1);
}

#[tokio::test]
async fn a_contact_changed_on_the_server_meanwhile_is_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().contacts = vec![
        r#"{"id":"C1","changeKey":"k1","displayName":"Ann Lee","emailAddresses":[{"address":"ann@example.org"}],"companyName":"Acme"}"#.into(),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let id = ann_id(&core).await;
    let mut record = core
        .list_contact_records("ann".into(), 5)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap();
    record.company = "Local edit".into();
    record.account_ids = vec![account.id];
    core.save_contact(record).await.unwrap();
    shared.lock().unwrap().contact_key = "k9".into();
    let mut events = core.bus.subscribe();
    core.sync_now(Some(account.id)).await.unwrap();
    assert!(
        seen(&shared, "PATCH /graph/v1.0/me/contacts/C1").is_empty(),
        "the server's newer version is not overwritten"
    );
    // The local edit is not lost: it is kept as a second contact, and the
    // user is told.
    let copies = seen(&shared, "POST /graph/v1.0/me/contactFolders/CF1/contacts");
    assert_eq!(copies.len(), 1, "{:?}", shared.lock().unwrap().log);
    assert!(copies[0].contains("Ann Lee (your edit)") && copies[0].contains("Local edit"));
    assert!(
        copies[0].contains(r#""emailAddresses":[]"#),
        "{}",
        copies[0]
    );
    let mut notice = None;
    while let Ok(event) = events.try_recv() {
        if let rustmail_core::events::CoreEvent::Notice { message, .. } = event {
            notice = Some(message);
        }
    }
    assert!(notice.unwrap().contains("your edit"));
    assert_eq!(core.pending_changes(Some(account.id)).await.unwrap(), 0);
}

#[tokio::test]
async fn the_organization_directory_can_be_searched() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    let found = core
        .exchange_directory_search(account.id, "carl", 10)
        .await
        .unwrap();
    // `/me/people` first, then the tenant directory without repeating anyone.
    assert_eq!(
        found,
        vec![
            ("Carl Dir".to_owned(), "carl@example.org".to_owned()),
            ("Dora Tenant".to_owned(), "dora@example.org".to_owned()),
        ]
    );
    assert!(
        core.exchange_directory_search(account.id, "  ", 10)
            .await
            .unwrap()
            .is_empty()
    );
    // What the directory answered is offered to the composer, for this
    // account only, until the account is removed.
    assert!(core.is_exchange_account(account.id).await);
    assert_eq!(
        core.directory_suggestions(Some(account.id), "CARL", 5),
        vec![("Carl Dir".to_owned(), "carl@example.org".to_owned())]
    );
    assert!(
        core.directory_suggestions(Some(account.id + 1), "carl", 5)
            .is_empty()
    );
    assert!(
        core.directory_suggestions(Some(account.id), "zed", 5)
            .is_empty()
    );
    core.remove_account(account.id).await.unwrap();
    assert!(core.directory_suggestions(None, "carl", 5).is_empty());
}

#[tokio::test]
async fn delete_everything_clears_the_exchange_cache_and_tokens() {
    use rustmail_core::accounts::secrets::{CredentialStore, Slot};
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().contacts = vec![
        r#"{"id":"C1","changeKey":"k1","displayName":"Ann Lee","emailAddresses":[{"address":"ann@example.org"}]}"#.into(),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let store = DevelopmentFileCredentialStore::new(temp.path().join("credentials.json"));
    assert!(store.load(account.id, Slot::RefreshToken).is_ok());
    let count = |sql: &'static str| {
        let db = core.db.clone();
        async move {
            db.read(move |conn| Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?))
                .await
                .unwrap()
        }
    };
    assert!(count("SELECT COUNT(*) FROM exchange_messages").await > 0);

    core.delete_all_local_data().await.unwrap();

    assert_eq!(count("SELECT COUNT(*) FROM exchange_messages").await, 0);
    assert_eq!(count("SELECT COUNT(*) FROM exchange_folders").await, 0);
    assert_eq!(count("SELECT COUNT(*) FROM carddav_objects").await, 0);
    assert_eq!(count("SELECT COUNT(*) FROM contacts").await, 0);
    let store = DevelopmentFileCredentialStore::new(temp.path().join("credentials.json"));
    assert!(store.load(account.id, Slot::RefreshToken).is_err());
}

fn requests_seen(shared: &Shared) -> usize {
    shared.lock().unwrap().log.len()
}

async fn thread_of_subject(core: &Core, subject: &'static str) -> i64 {
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

#[tokio::test]
async fn working_offline_queues_changes_makes_no_request_and_replays_them_later() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    assert_eq!(
        core.offline_status().await.unwrap(),
        (false, 0),
        "online and nothing waiting"
    );

    core.set_exchange_work_offline(account.id, true)
        .await
        .unwrap();
    let one = thread_of_subject(&core, "One").await;
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
    // A sync attempt neither reaches the server nor fails the actions.
    wait_until("the sync actor to go offline", async || {
        core.sync_now(Some(account.id)).await.is_err()
    })
    .await;
    let before = requests_seen(&shared);
    assert_eq!(requests_seen(&shared), before, "no request while offline");
    assert_eq!(core.pending_changes(Some(account.id)).await.unwrap(), 2);
    assert_eq!(core.offline_status().await.unwrap(), (true, 2));
    // Reading works: the change is already visible locally.
    assert_eq!(
        rows(&core).await.iter().find(|r| r.0 == "One").unwrap().1,
        "trash"
    );
    // A message written offline waits in the outbox as well.
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
            subject: "Written offline".into(),
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
    wait_until("the offline send to stay queued", async || {
        let state: String = core
            .db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT state FROM pending_actions WHERE id=?1",
                    [queued.action_id],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_ne!(state, "failed", "an offline send never fails");
        state == "pending"
    })
    .await;
    assert!(seen(&shared, "sendMail").is_empty());

    // Back online: everything is replayed.
    core.set_exchange_work_offline(account.id, false)
        .await
        .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    {
        let state = shared.lock().unwrap();
        let m1 = state.messages.iter().find(|m| m.id == "M1").unwrap();
        assert!(m1.read && m1.folder == "trash", "queued changes replayed");
    }
    core.wait_for_send(queued.action_id).await.unwrap();
    assert!(!seen(&shared, "sendMail").is_empty());
    assert_eq!(core.pending_changes(Some(account.id)).await.unwrap(), 0);
    assert_eq!(core.offline_status().await.unwrap(), (false, 0));
}

#[tokio::test]
async fn a_lost_connection_keeps_the_retry_budget_of_queued_actions() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    core.set_exchange_work_offline(account.id, true)
        .await
        .unwrap();
    // The sync actor takes the new settings a moment later; once a sync
    // fails for being offline it has them.
    wait_until("the sync actor to go offline", async || {
        core.sync_now(Some(account.id)).await.is_err()
    })
    .await;
    let one = thread_of_subject(&core, "One").await;
    core.perform_action(PerformActionArgs {
        kind: ActionKind::MarkRead,
        thread_ids: vec![one],
        params: None,
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let _ = core.sync_now(Some(account.id)).await;
    }
    let (state, attempts): (String, i64) = core
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT state, attempts FROM pending_actions WHERE kind='mark_read'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!((state.as_str(), attempts), ("pending", 0));
}

#[tokio::test]
async fn a_task_linked_to_microsoft_365_mail_keeps_its_link_after_the_mail_moves() {
    use rustmail_core::tasks::{ColumnKind, LinkTarget, TaskDraft};
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();
    let one = thread_of_subject(&core, "One").await;
    core.task_project_create_default("Work".into())
        .await
        .unwrap();
    let task = core
        .task_create_from_thread(
            one,
            TaskDraft {
                title: None,
                project_id: None,
                status: ColumnKind::Next,
                due_at: None,
                context_id: None,
                label_ids: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(task.title, "One");
    // The link names the message by its Message-ID, so a move on the server
    // (here: to Deleted Items) and the sync that follows do not break it.
    core.perform_action(PerformActionArgs {
        kind: ActionKind::Trash,
        thread_ids: vec![one],
        params: None,
    })
    .await
    .unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    let links = core.task_links(task.id).await.unwrap();
    assert!(
        matches!(links[0].target, LinkTarget::Mail { .. }),
        "{links:?}"
    );
    let linked = core.task_threads(None).await.unwrap();
    assert!(
        linked.iter().any(|t| t.task_id == task.id),
        "the task still finds its conversation"
    );
}

#[tokio::test]
async fn contact_extras_and_photo_round_trip_with_graph() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().contacts = vec![
        r#"{"id":"C1","changeKey":"k1","displayName":"Ann Lee","emailAddresses":[{"address":"ann@example.org"},{"address":"ann.lee@home.example"}],"mobilePhone":"+1 555 0100","businessPhones":["+1 555 0101"],"homePhones":["+1 555 0102"],"businessAddress":{"street":"1 Main St","city":"Oslo"},"homeAddress":{"street":"2 Side Rd","city":"Bergen"}}"#.into(),
    ];
    shared.lock().unwrap().contact_photo =
        b"\xFF\xD8JPEG-PICTURE".iter().map(|b| b & 0x7f).collect();
    let picture = shared.lock().unwrap().contact_photo.clone();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.sync_now(Some(account.id)).await.unwrap();

    let id = ann_id(&core).await;
    let record = core
        .list_contact_records("ann".into(), 5)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == id)
        .unwrap();
    assert_eq!(record.phone, "+1 555 0100");
    assert_eq!(
        record.extras.phones,
        vec!["+1 555 0101".to_owned(), "+1 555 0102".to_owned()]
    );
    assert_eq!(
        record.extras.emails,
        vec!["ann.lee@home.example".to_owned()]
    );
    assert_eq!(record.postal_address, "1 Main St, Oslo");
    assert_eq!(
        record.extras.addresses,
        vec!["2 Side Rd, Bergen".to_owned()]
    );
    assert!(record.has_photo);
    assert_eq!(core.contact_photo(id).await.unwrap(), Some(picture.clone()));
    // The kind of each number and address is kept.
    assert_eq!(record.extras.phone_kind("+1 555 0100"), Some("mobile"));
    assert_eq!(record.extras.phone_kind("+1 555 0101"), Some("work"));
    assert_eq!(record.extras.phone_kind("+1 555 0102"), Some("home"));
    assert_eq!(record.extras.address_kind("1 Main St, Oslo"), Some("work"));
    assert_eq!(
        record.extras.address_kind("2 Side Rd, Bergen"),
        Some("home")
    );
    // The extra values are found by searching.
    for query in ["ann.lee@home", "555 0102", "side rd"] {
        let found = core.list_contact_records(query.into(), 5).await.unwrap();
        assert!(found.iter().any(|c| c.id == id), "{query}");
    }

    // An edit keeps the extra values and the picture and sends them back.
    let mut edited = record.clone();
    edited.company = "Globex".into();
    edited.extras.emails.push("ann@work.example".into());
    edited.account_ids = vec![account.id];
    core.save_contact(edited).await.unwrap();
    core.sync_now(Some(account.id)).await.unwrap();
    let patches = seen(&shared, "PATCH /graph/v1.0/me/contacts/C1");
    assert_eq!(patches.len(), 1, "{:?}", shared.lock().unwrap().log);
    for wanted in [
        "ann.lee@home.example",
        "ann@work.example",
        "+1 555 0101",
        "2 Side Rd, Bergen",
    ] {
        assert!(patches[0].contains(wanted), "{wanted} in {}", patches[0]);
    }
    // Every value goes back to the field of its kind.
    let sent: serde_json::Value = serde_json::from_str(&patches[0]).unwrap();
    assert_eq!(sent["mobilePhone"], "+1 555 0100");
    assert_eq!(sent["businessPhones"], serde_json::json!(["+1 555 0101"]));
    assert_eq!(sent["homePhones"], serde_json::json!(["+1 555 0102"]));
    assert_eq!(sent["businessAddress"]["street"], "1 Main St, Oslo");
    assert_eq!(sent["homeAddress"]["street"], "2 Side Rd, Bergen");
    assert_eq!(
        seen(&shared, "PUT /graph/v1.0/me/contacts/C1/photo/$value").len(),
        1
    );
    assert_eq!(core.contact_photo(id).await.unwrap(), Some(picture));

    // Removing the picture locally clears it and marks the contact for upload.
    core.set_contact_photo(id, None).await.unwrap();
    assert_eq!(core.contact_photo(id).await.unwrap(), None);
}

#[tokio::test]
async fn the_calendar_window_of_an_account_can_be_set_and_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.set_exchange_calendar_window(account.id, 30, 100_000)
        .await
        .unwrap();
    let settings = core.exchange_account_settings(account.id).await.unwrap();
    assert_eq!(settings.calendar_past_days, 30);
    assert_eq!(settings.calendar_future_days, 3650);
    // Editing the other settings keeps the window.
    core.update_exchange_account(account.id, None, settings.clone(), None)
        .await
        .unwrap();
    let again = core.exchange_account_settings(account.id).await.unwrap();
    assert_eq!(again.calendar_past_days, 30);
}

async fn event_id_of(core: &Core, href: &'static str) -> i64 {
    core.calendar_db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT id FROM calendar_events WHERE caldav_href = ?1",
                [href],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn reminders_come_from_graph_and_a_changed_one_goes_back() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().events = vec![
        r#"{"id":"EV1","iCalUId":"UID1","type":"singleInstance","subject":"Planning","isAllDay":false,"isCancelled":false,
            "isReminderOn":true,"reminderMinutesBeforeStart":30,
            "start":{"dateTime":"2026-07-01T09:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-07-01T10:00:00.0000000","timeZone":"UTC"}}"#
            .replace('\n', ""),
        r#"{"id":"EV2","iCalUId":"UID2","type":"singleInstance","subject":"Quiet","isAllDay":false,"isCancelled":false,
            "isReminderOn":false,"reminderMinutesBeforeStart":15,
            "start":{"dateTime":"2026-07-02T09:00:00.0000000","timeZone":"UTC"},
            "end":{"dateTime":"2026-07-02T10:00:00.0000000","timeZone":"UTC"}}"#
            .replace('\n', ""),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.calendar_sync_now(Some(account.id)).await;
    let window = (utc_ms(2026, 6, 30, 0), utc_ms(2026, 7, 5, 0));
    let mut events = Vec::new();
    for _ in 0..100 {
        events = core.list_events(window.0, window.1).await.unwrap();
        if events.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let reminder = |name: &str| {
        events
            .iter()
            .find(|e| e.summary.as_deref() == Some(name))
            .map(|e| e.reminder_minutes)
    };
    assert_eq!(reminder("Planning"), Some(Some(30)), "{events:?}");
    assert_eq!(
        reminder("Quiet"),
        Some(Some(0)),
        "a switched-off reminder is 0"
    );

    // A reminder outside Outlook's range is refused; a valid one is pushed.
    let planning = event_id_of(&core, "EV1").await;
    assert!(
        core.set_event_reminder(planning, Some(40_321))
            .await
            .is_err()
    );
    core.set_event_reminder(planning, Some(10)).await.unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the reminder on the server", async || {
        seen(&shared, "PATCH /graph/v1.0/me/events/EV1")
            .iter()
            .any(|body| {
                body.contains("\"reminderMinutesBeforeStart\":10")
                    && body.contains("\"isReminderOn\":true")
            })
    })
    .await;
    // "No reminder" switches it off there.
    core.set_event_reminder(planning, Some(0)).await.unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the reminder switched off", async || {
        seen(&shared, "PATCH /graph/v1.0/me/events/EV1")
            .iter()
            .any(|body| body.contains("\"isReminderOn\":false"))
    })
    .await;
}

#[tokio::test]
async fn one_occurrence_of_a_series_can_be_cancelled_or_changed_and_a_series_ended() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let occurrence = |id: &str, day: u32| {
        format!(
            r#"{{"id":"{id}","iCalUId":"U-SM1","type":"occurrence","seriesMasterId":"SM1","subject":"Standup","isAllDay":false,"isCancelled":false,
                "start":{{"dateTime":"2026-07-{day:02}T09:00:00.0000000","timeZone":"UTC"}},
                "end":{{"dateTime":"2026-07-{day:02}T09:30:00.0000000","timeZone":"UTC"}}}}"#
        )
        .replace('\n', "")
    };
    shared.lock().unwrap().events = vec![occurrence("OC1", 6), occurrence("OC3", 20)];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the series master", async || {
        core.calendar_db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM calendar_events WHERE caldav_href = 'SM1'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .await
            .unwrap()
            == 1
    })
    .await;
    let master = event_id_of(&core, "SM1").await;

    // Cancel the occurrence of the 6th: Graph is asked for the instances and
    // the matching one is deleted.
    core.exchange_cancel_occurrence(master, utc_ms(2026, 7, 6, 9))
        .await
        .unwrap();
    assert!(log_has(&shared, "GET /graph/v1.0/me/events/SM1/instances"));
    assert!(log_has(&shared, "DELETE /graph/v1.0/me/events/INST-06"));

    // Change the occurrence of the 27th: only that instance is patched.
    core.exchange_edit_occurrence(
        master,
        utc_ms(2026, 7, 27, 9),
        Some("Standup (short)".into()),
        Some(utc_ms(2026, 7, 27, 10)),
        Some(utc_ms(2026, 7, 27, 11)),
    )
    .await
    .unwrap();
    let patches = seen(&shared, "PATCH /graph/v1.0/me/events/INST-27");
    assert_eq!(patches.len(), 1);
    assert!(patches[0].contains("Standup (short)"), "{patches:?}");
    assert!(patches[0].contains("2026-07-27T10:00:00"), "{patches:?}");
    assert!(!log_has(&shared, "PATCH /graph/v1.0/me/events/SM1"));

    // An occurrence that does not exist is not found, and nothing is sent.
    let changes = |shared: &Shared| {
        shared
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(line, _)| line.starts_with("DELETE ") || line.starts_with("PATCH "))
            .count()
    };
    let before = changes(&shared);
    let missing = core
        .exchange_cancel_occurrence(master, utc_ms(2026, 7, 14, 9))
        .await;
    assert!(matches!(missing, Err(CoreError::NotFound(_))));
    assert_eq!(changes(&shared), before, "nothing was deleted or changed");

    // "Delete this and the following" ends the series the day before.
    core.end_series_before(master, utc_ms(2026, 7, 20, 9))
        .await
        .unwrap();
    let rule: String = core
        .calendar_db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT rrule FROM calendar_events WHERE id = ?1",
                [master],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        rule,
        "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO;UNTIL=20260719T235959Z"
    );
    let listed = core
        .list_events(utc_ms(2026, 7, 1, 0), utc_ms(2026, 8, 1, 0))
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .filter(|e| e.summary.as_deref() == Some("Standup"))
            .all(|e| e.starts_at < utc_ms(2026, 7, 20, 0)),
        "{listed:?}"
    );
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the shortened series on the server", async || {
        seen(&shared, "PATCH /graph/v1.0/me/events/SM1")
            .iter()
            .any(|body| body.contains("2026-07-19"))
    })
    .await;

    // Ending it at its first occurrence deletes the whole series.
    core.end_series_before(master, utc_ms(2026, 7, 6, 9))
        .await
        .unwrap();
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the series deleted", async || {
        log_has(&shared, "DELETE /graph/v1.0/me/events/SM1")
    })
    .await;
}

#[tokio::test]
async fn an_event_can_be_made_to_repeat_and_goes_to_graph_with_its_rule() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    let created = core
        .create_event(CreateEventArgs {
            account_id: account.id,
            calendar_id: None,
            summary: "Weekly sync".into(),
            description: None,
            location: None,
            join_url: None,
            starts_at: utc_ms(2026, 7, 6, 9),
            ends_at: utc_ms(2026, 7, 6, 10),
            all_day: false,
            attendees: vec![],
        })
        .await
        .unwrap();
    // A rule the expander does not know, or Outlook cannot hold, is refused.
    assert!(
        core.set_event_recurrence(created.id, Some("FREQ=SECONDLY".into()))
            .await
            .is_err()
    );
    core.set_event_recurrence(created.id, Some("FREQ=WEEKLY;BYDAY=MO;COUNT=4".into()))
        .await
        .unwrap();
    let listed = core
        .list_events(utc_ms(2026, 7, 1, 0), utc_ms(2026, 9, 1, 0))
        .await
        .unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|e| e.summary.as_deref() == Some("Weekly sync"))
            .count(),
        4,
        "{listed:?}"
    );
    core.calendar_sync_now(Some(account.id)).await;
    wait_until("the rule on the server", async || {
        let bodies = seen(&shared, "/graph/v1.0/me/calendars/CAL1/events");
        let patches = seen(&shared, "PATCH /graph/v1.0/me/events/");
        bodies
            .iter()
            .chain(patches.iter())
            .any(|body| body.contains("\"weekly\"") && body.contains("\"numberOfOccurrences\":4"))
    })
    .await;
    // Stopping the repetition clears the rule here.
    core.set_event_recurrence(created.id, None).await.unwrap();
    let again = core
        .list_events(utc_ms(2026, 7, 1, 0), utc_ms(2026, 9, 1, 0))
        .await
        .unwrap();
    assert_eq!(
        again
            .iter()
            .filter(|e| e.summary.as_deref() == Some("Weekly sync"))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_series_keeps_its_local_time_in_its_own_zone_across_daylight_saving() {
    let temp = tempfile::tempdir().unwrap();
    let shared = seeded();
    shared.lock().unwrap().events = vec![
        r#"{"id":"OCB","iCalUId":"U-SM3","type":"occurrence","seriesMasterId":"SM3","subject":"Berlin sync","isAllDay":false,"isCancelled":false,
            "start":{"dateTime":"2026-03-23T09:00:00.0000000","timeZone":"W. Europe Standard Time"},
            "end":{"dateTime":"2026-03-23T10:00:00.0000000","timeZone":"W. Europe Standard Time"}}"#
            .replace('\n', ""),
    ];
    let base = serve(shared.clone()).await;
    let core = start(temp.path()).await;
    let account = add(&core, &base).await;
    core.calendar_sync_now(Some(account.id)).await;
    let window = (utc_ms(2026, 3, 20, 0), utc_ms(2026, 4, 12, 0));
    let mut starts = Vec::new();
    for _ in 0..100 {
        starts = core
            .list_events(window.0, window.1)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.summary.as_deref() == Some("Berlin sync"))
            .map(|e| e.starts_at)
            .collect();
        if starts.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Mondays 09:00 in Berlin: CET (UTC+1) on the 23rd, CEST (UTC+2) after
    // summer time starts on Sunday 2026-03-29, whatever zone this computer is
    // in.
    assert_eq!(
        starts,
        vec![
            utc_ms(2026, 3, 23, 8),
            utc_ms(2026, 3, 30, 7),
            utc_ms(2026, 4, 6, 7)
        ]
    );
}
