//! The Exchange Online sync engine: folder tree, per-folder delta sync, queued
//! actions and body/attachment access. Mail is stored like any cached mail:
//! every message is downloaded once as MIME (`/messages/{id}/$value`) and read
//! from the local copy afterwards.
//!
//! Server changes win for read state and flags; a message with a local change
//! still waiting in the action queue keeps its local state until the action
//! has been sent. Messages moved on the server arrive as a removal in the old
//! folder and an upsert in the new one (Graph ids are immutable), so a removal
//! only deletes when the message still sits in that folder here.

use super::client::{BatchRequest, DeltaItem, DeltaResult, GraphMail, GraphMessage};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::local::scan::{FlagSet, ParsedMessage, insert_parsed};
use crate::models::{AccountConfig, MailProtocol, now_ms, roles};
use crate::sync::engine::{PriorityFetchCmd, SyncCmd, SyncCtx, set_state, set_state_error};
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// A first sync only reads mail received in this window.
/// Days of one month of the account's mail history setting.
const MONTH_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
/// New messages downloaded per sync cycle; the rest follows in the next one.
const CYCLE_NEW_MESSAGES: usize = 200;
const MAX_ACTION_ATTEMPTS: i64 = 8;

const WELL_KNOWN: [(&str, &str); 6] = [
    ("inbox", roles::INBOX),
    ("sentitems", roles::SENT),
    ("drafts", roles::DRAFTS),
    ("deleteditems", roles::TRASH),
    ("junkemail", roles::SPAM),
    ("archive", roles::ARCHIVE),
];

/// How often Microsoft 365 mail is read while the mail view is open in a
/// focused window. Graph change notifications need a public endpoint, which
/// Rustmail does not have, so the app asks for the (cheap) Inbox delta itself.
pub(crate) const MAIL_WATCH_POLL_SECS: u64 = 60;
/// The longest wait after repeated failures while watching.
const MAIL_WATCH_MAX_BACKOFF_SECS: u64 = 10 * 60;

/// Replaces the short poll's interval (milliseconds, no backoff) so tests can
/// watch the poll work without waiting a minute. 0: the normal interval.
static POLL_OVERRIDE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[doc(hidden)]
pub fn set_watch_poll_millis_for_tests(millis: u64) {
    POLL_OVERRIDE_MS.store(millis, std::sync::atomic::Ordering::Relaxed);
}

/// Seconds to wait before the next cycle: the configured cadence, or while the
/// mail view is watched the short poll (doubled after each failed cycle, never
/// beyond ten minutes or the configured cadence).
pub(crate) fn mail_poll_secs(configured_secs: u64, watching: bool, failures: u32) -> u64 {
    if !watching {
        return configured_secs;
    }
    MAIL_WATCH_POLL_SECS
        .saturating_mul(1u64 << failures.min(10))
        .min(MAIL_WATCH_MAX_BACKOFF_SECS)
        .min(configured_secs.max(MAIL_WATCH_POLL_SECS))
        .max(MAIL_WATCH_POLL_SECS)
}

pub(crate) fn spawn(
    ctx: SyncCtx,
    config: AccountConfig,
    rx: mpsc::Receiver<SyncCmd>,
    body_rx: mpsc::Receiver<PriorityFetchCmd>,
    settings_rx: watch::Receiver<crate::models::AccountSettings>,
) -> Vec<tokio::task::JoinHandle<()>> {
    debug_assert_eq!(config.mail_protocol, MailProtocol::Exchange);
    vec![
        tokio::spawn(actor(ctx.clone(), config.clone(), rx, settings_rx)),
        tokio::spawn(body_actor(ctx, body_rx)),
    ]
}

fn finish_waiters(
    waiters: &mut Vec<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    result: std::result::Result<(), String>,
) {
    for waiter in waiters.drain(..) {
        let _ = waiter.send(result.clone());
    }
}

async fn actor(
    ctx: SyncCtx,
    mut config: AccountConfig,
    mut rx: mpsc::Receiver<SyncCmd>,
    mut settings: watch::Receiver<crate::models::AccountSettings>,
) {
    let mut waiters = Vec::new();
    let mut failures: u32 = 0;
    let mut last_full: Option<std::time::Instant> = None;
    // Set when the timer (not a command) woke the loop while watching: only
    // the Inbox is read then, unless a full cycle is due.
    let mut light = false;
    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                SyncCmd::Shutdown => return,
                SyncCmd::SyncNow {
                    complete: Some(done),
                } => waiters.push(done),
                _ => {}
            }
        }
        // Always take the current value: `settings.changed()` in the wait
        // below marks a change as seen, so `has_changed()` would miss it.
        config.settings = settings.borrow_and_update().clone();
        set_state(&ctx, config.id, "syncing").await;
        let mut more = false;
        let ran_light = light;
        let outcome = match super::graph_for(&ctx, &config) {
            Ok(client) if ran_light => light_cycle(&ctx, &config, &client).await,
            Ok(client) => cycle(&ctx, &config, &client).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(pass) => {
                if !ran_light {
                    last_full = Some(std::time::Instant::now());
                }
                more = pass.more;
                match pass.failure {
                    None => {
                        failures = 0;
                        set_state(&ctx, config.id, "idle").await;
                        finish_waiters(&mut waiters, Ok(()));
                    }
                    // The rest of the mailbox synced; the folder that did not
                    // stays in the account list until it reads again.
                    Some(message) => {
                        failures = failures.saturating_add(1);
                        more = false;
                        set_state_error(&ctx, config.id, "idle", &message).await;
                        finish_waiters(&mut waiters, Err(message));
                    }
                }
            }
            Err(CoreError::Offline)
                if super::is_work_offline(&config.settings.connection.exchange) =>
            {
                // Offline on purpose: changes wait in the queue.
                set_state(&ctx, config.id, "offline").await;
                finish_waiters(&mut waiters, Err("working offline".into()));
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                let message = error.to_string();
                tracing::warn!(account_id = config.id, %message, "exchange sync failed");
                let state = if matches!(error, CoreError::Auth(_) | CoreError::NeedsReauth) {
                    "needs_reauth"
                } else {
                    "offline"
                };
                set_state_error(&ctx, config.id, state, &message).await;
                finish_waiters(&mut waiters, Err(message));
            }
        }
        let configured = crate::sync::engine::configured_sync_interval(&ctx.db).await;
        let watching = ctx.mail_watching.load(std::sync::atomic::Ordering::Relaxed);
        let override_ms = POLL_OVERRIDE_MS.load(std::sync::atomic::Ordering::Relaxed);
        let wait = if more {
            Duration::from_secs(1)
        } else if watching && override_ms > 0 {
            Duration::from_millis(override_ms)
        } else {
            Duration::from_secs(mail_poll_secs(configured.as_secs(), watching, failures))
        };
        light = false;
        tokio::select! {
            command = rx.recv() => match command {
                None | Some(SyncCmd::Shutdown) => return,
                Some(SyncCmd::SyncNow { complete: Some(done) }) => waiters.push(done),
                _ => {}
            },
            _ = settings.changed() => {},
            _ = tokio::time::sleep(wait) => {
                // A short poll only reads the Inbox; the whole mailbox is
                // still read once per configured interval.
                light = !more
                    && watching
                    && last_full.is_some_and(|at| at.elapsed() < configured);
            },
        }
    }
}

/// The short poll while the mail view is watched: queued actions and the
/// Inbox's delta. No folder tree, contacts or other folders (the next full
/// cycle reads those). Falls back to a full cycle when the Inbox is unknown.
pub(crate) async fn light_cycle(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
) -> Result<Pass> {
    let account_id = config.id;
    let inbox = ctx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT ef.graph_id, ef.folder_id FROM exchange_folders ef
                       JOIN folders f ON f.id = ef.folder_id
                      WHERE ef.account_id = ?1 AND f.role = ?2",
                    params![account_id, roles::INBOX],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?)
        })
        .await?;
    let Some((graph_id, folder_id)) = inbox else {
        return cycle(ctx, config, client).await;
    };
    run_due_actions(ctx, config, client).await?;
    let folder = FolderMap {
        graph_id,
        folder_id,
        role: Some(roles::INBOX),
        name: "Inbox".into(),
    };
    let mut budget = CYCLE_NEW_MESSAGES;
    let mut more = in_step(
        "reading folder inbox",
        sync_folder(ctx, config, client, &folder, &mut budget).await,
    )?;
    // The folder the open view shows (when it is not the Inbox) is read too.
    for extra in watched_folders_of(ctx, account_id, folder_id).await? {
        let name = extra.name.clone();
        more |= in_step(
            &format!("reading folder {name}"),
            sync_folder(ctx, config, client, &extra, &mut budget).await,
        )?;
    }
    Ok(Pass {
        more,
        failure: None,
    })
}

/// A folder besides the Inbox that the open mail view shows. The app sets it
/// when the scope changes, so the short poll also reads that folder (every
/// scope maps to folders: one folder, one account's mailbox of a role, or the
/// same role of every account; labels, tasks and split inboxes show the
/// Inbox's mail and need nothing more). One list per process, like the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchedFolder {
    /// A folder by its local id.
    Folder(i64),
    /// An account's folder with this role (`models::roles`).
    Role(i64, String),
}

static WATCHED_FOLDERS: std::sync::Mutex<Vec<WatchedFolder>> = std::sync::Mutex::new(Vec::new());

/// How many extra folders one short poll reads (a scope never needs more
/// than one folder per account; this bounds a bad caller).
const MAX_WATCHED_FOLDERS: usize = 16;

pub fn set_watched_folders(mut folders: Vec<WatchedFolder>) {
    folders.truncate(MAX_WATCHED_FOLDERS);
    if let Ok(mut current) = WATCHED_FOLDERS.lock() {
        *current = folders;
    }
}

fn watched_folders() -> Vec<WatchedFolder> {
    WATCHED_FOLDERS
        .lock()
        .map(|folders| folders.clone())
        .unwrap_or_default()
}

/// The watched folders that belong to this account and are mapped to Graph
/// folders, except the Inbox (already read).
async fn watched_folders_of(
    ctx: &SyncCtx,
    account_id: i64,
    inbox_folder_id: i64,
) -> Result<Vec<FolderMap>> {
    let watched = watched_folders();
    if watched.is_empty() {
        return Ok(Vec::new());
    }
    ctx.db
        .read(move |conn| {
            let mut out: Vec<FolderMap> = Vec::new();
            for entry in watched {
                let (clause, key): (&str, rusqlite::types::Value) = match &entry {
                    WatchedFolder::Folder(id) => ("f.id = ?2", (*id).into()),
                    WatchedFolder::Role(account, role) if *account == account_id => {
                        ("f.role = ?2", role.clone().into())
                    }
                    WatchedFolder::Role(..) => continue,
                };
                let row = conn
                    .query_row(
                        &format!(
                            "SELECT x.graph_id, x.folder_id, f.role, f.imap_name
                               FROM exchange_folders x
                               JOIN folders f ON f.id = x.folder_id
                              WHERE x.account_id = ?1 AND {clause}"
                        ),
                        params![account_id, key],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((graph_id, id, role, name)) = row else {
                    continue;
                };
                if id == inbox_folder_id || out.iter().any(|known| known.folder_id == id) {
                    continue;
                }
                out.push(FolderMap {
                    graph_id,
                    folder_id: id,
                    role: WELL_KNOWN
                        .iter()
                        .map(|(_, known)| *known)
                        .find(|known| Some(*known) == role.as_deref()),
                    name,
                });
            }
            Ok(out)
        })
        .await
}

/// Whether Microsoft 365 refused a request for lack of a permission.
fn is_denied(error: &CoreError) -> bool {
    matches!(error, CoreError::Other(message) if message.starts_with("Microsoft 365 denied this request"))
}

/// Name the step that failed in the error the account list shows. Sign-in and
/// offline errors keep their kind, because the sync loop acts on it.
fn in_step<T>(step: &str, result: Result<T>) -> Result<T> {
    result.map_err(|error| match error {
        CoreError::Auth(_) | CoreError::NeedsReauth | CoreError::Offline => error,
        other => CoreError::Other(format!("{step}: {other}")),
    })
}

struct FolderMap {
    graph_id: String,
    folder_id: i64,
    role: Option<&'static str>,
    /// The local folder path, for error messages.
    name: String,
}

/// What one sync pass found: whether more mail waits on the server, and the
/// first folder that could not be read (the others were still read).
pub(crate) struct Pass {
    more: bool,
    failure: Option<String>,
}

/// One sync cycle. A folder that cannot be read does not stop the others: the
/// first such failure comes back in the pass, named by folder, and is shown in
/// the account list while everything else is still synced.
pub(crate) async fn cycle(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
) -> Result<Pass> {
    let folders = in_step(
        "reading the folder list",
        sync_folders(ctx, config, client).await,
    )?;
    run_due_actions(ctx, config, client).await?;
    let mut failure: Option<String> = None;
    // Contacts have their own permission; a failure there never stops mail.
    if let Err(error) = super::contacts::sync_contacts(ctx, config, client).await {
        if matches!(error, CoreError::NeedsReauth | CoreError::Auth(_)) {
            return Err(error);
        }
        tracing::warn!(account_id = config.id, %error, "exchange contacts not synced");
        // A sign-in without the contacts permission is a choice, not a
        // fault; anything else (a contact edit that cannot be sent, a folder
        // that cannot be read) was invisible until now.
        if !is_denied(&error) {
            failure = Some(format!("syncing contacts: {error}"));
        }
    }
    let mut budget = CYCLE_NEW_MESSAGES;
    let mut more = false;
    // Inbox first so new mail shows up before a large archive is read.
    let mut ordered: Vec<&FolderMap> = folders.iter().collect();
    ordered.sort_by_key(|folder| folder.role != Some(roles::INBOX));
    for folder in ordered {
        if budget == 0 {
            more = true;
            break;
        }
        match sync_folder(ctx, config, client, folder, &mut budget).await {
            Ok(rest) => more |= rest,
            // Sign-in and connection problems concern the whole account.
            Err(error @ (CoreError::Auth(_) | CoreError::NeedsReauth | CoreError::Offline)) => {
                return Err(error);
            }
            // One folder Microsoft 365 cannot give us (a damaged or
            // just-deleted folder) must not keep every later folder from
            // syncing on every cycle.
            Err(error) => {
                tracing::warn!(account_id = config.id, folder = %folder.name, %error, "exchange folder not read");
                failure
                    .get_or_insert_with(|| format!("reading folder \"{}\": {error}", folder.name));
            }
        }
    }
    Ok(Pass { more, failure })
}

/// The Graph id of a local folder.
async fn graph_id_of(ctx: &SyncCtx, account_id: i64, folder_id: i64) -> Result<String> {
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT graph_id FROM exchange_folders WHERE account_id=?1 AND folder_id=?2",
                    params![account_id, folder_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await?
        .ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))
}

/// The Graph id of the folder an Exchange account maps to `role`, if any.
pub(crate) async fn graph_id_of_role(
    ctx: &SyncCtx,
    account_id: i64,
    role: &str,
) -> Result<Option<String>> {
    let role = role.to_owned();
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT x.graph_id FROM exchange_folders x
                     JOIN folders f ON f.id = x.folder_id
                     WHERE x.account_id=?1 AND f.role=?2",
                    params![account_id, role],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await
}

/// The Graph id of a local folder.
pub(crate) async fn graph_id_of_folder(
    ctx: &SyncCtx,
    account_id: i64,
    folder_id: i64,
) -> Result<String> {
    graph_id_of(ctx, account_id, folder_id).await
}

/// Create a folder on the server (under `parent`, a local folder id) and list
/// the mailbox's folders again so it shows up.
pub(crate) async fn create_folder(
    ctx: &SyncCtx,
    config: &AccountConfig,
    parent: Option<i64>,
    name: &str,
) -> Result<()> {
    let client = super::graph_for(ctx, config)?;
    let parent = match parent {
        Some(folder_id) => Some(graph_id_of(ctx, config.id, folder_id).await?),
        None => None,
    };
    client.create_folder(parent.as_deref(), name).await?;
    sync_folders(ctx, config, &client).await.map(|_| ())
}

/// Rename a folder on the server, then the local folder and its children.
pub(crate) async fn rename_folder(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder_id: i64,
    old_path: &str,
    name: &str,
) -> Result<()> {
    let client = super::graph_for(ctx, config)?;
    let graph_id = graph_id_of(ctx, config.id, folder_id).await?;
    client.rename_folder(&graph_id, name).await?;
    let new_path = match old_path.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{}", sanitize_name(name)),
        None => sanitize_name(name),
    };
    let (account_id, old_path) = (config.id, old_path.to_owned());
    ctx.db
        .write(move |conn| repo::folders::rename_tree(conn, account_id, &old_path, &new_path, "/"))
        .await?;
    sync_folders(ctx, config, &client).await.map(|_| ())
}

/// Delete a folder (with its sub-folders and messages) on the server, then
/// the local copies.
pub(crate) async fn delete_folder(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder_id: i64,
    path: &str,
) -> Result<()> {
    let client = super::graph_for(ctx, config)?;
    let graph_id = graph_id_of(ctx, config.id, folder_id).await?;
    client.delete_folder(&graph_id).await?;
    let (account_id, path) = (config.id, path.to_owned());
    ctx.db
        .write(move |conn| repo::folders::delete_tree(conn, account_id, &path, "/"))
        .await?;
    sync_folders(ctx, config, &client).await.map(|_| ())
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch == '/' || ch.is_control() {
                '-'
            } else {
                ch
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Folder".to_owned()
    } else {
        trimmed.to_owned()
    }
}

async fn sync_folders(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
) -> Result<Vec<FolderMap>> {
    let tree = client.folders().await?;
    let mut roles_by_id: HashMap<String, &'static str> = HashMap::new();
    for (name, role) in WELL_KNOWN {
        if let Some(id) = client.well_known_folder(name).await? {
            roles_by_id.insert(id, role);
        }
    }
    // Parents come before children, so a child's path can be built in one pass.
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<(String, String, Option<&'static str>)> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for folder in &tree {
        let name = sanitize_name(&folder.name);
        let mut path = match folder
            .parent_id
            .as_ref()
            .and_then(|parent| paths.get(parent))
        {
            Some(parent) => format!("{parent}/{name}"),
            None => name,
        };
        // Two folders may share a display name; the local name must be unique.
        if !used.insert(path.clone()) {
            path = format!("{path} ({})", entries.len());
            used.insert(path.clone());
        }
        paths.insert(folder.id.clone(), path.clone());
        entries.push((
            folder.id.clone(),
            path,
            roles_by_id.get(&folder.id).copied(),
        ));
    }
    let account_id = config.id;
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            // The mailbox owner may manage every folder except the system ones.
            let system = repo::folders::FolderPermissions {
                can_create_children: false,
                can_rename: false,
                can_delete: false,
            };
            let user = repo::folders::FolderPermissions {
                can_create_children: true,
                can_rename: true,
                can_delete: true,
            };
            let mut out = Vec::new();
            for (graph_id, path, role) in entries {
                let folder_id = repo::folders::upsert_discovered(
                    &tx,
                    account_id,
                    &path,
                    Some("/"),
                    role,
                    true,
                    if role.is_some() { system } else { user },
                )?;
                tx.execute(
                    "INSERT INTO exchange_folders(account_id, graph_id, folder_id) VALUES (?1,?2,?3)
                     ON CONFLICT(account_id, graph_id) DO UPDATE SET folder_id = excluded.folder_id",
                    params![account_id, graph_id, folder_id],
                )?;
                out.push(FolderMap {
                    graph_id,
                    folder_id,
                    role,
                    name: path,
                });
            }
            // A folder deleted on the server goes with its cached messages. An
            // empty listing is never taken as "everything was deleted".
            if !out.is_empty() {
                remove_vanished_folders(&tx, account_id, &out)?;
            }
            tx.commit()?;
            Ok(out)
        })
        .await
}

/// Most categories read from one message, and the longest category name kept.
const MAX_CATEGORIES: usize = 50;
const MAX_CATEGORY_CHARS: usize = 100;
const CATEGORY_COLOR: &str = "#4a86e8";

/// Show the categories of a message as the account's labels: a label is made
/// for a category seen for the first time (named like it), and the message's
/// memberships follow the server.
fn sync_categories(
    tx: &rusqlite::Transaction<'_>,
    account_id: i64,
    message_id: i64,
    categories: &[String],
) -> Result<()> {
    let mut keywords: Vec<String> = Vec::new();
    for name in categories.iter().take(MAX_CATEGORIES) {
        let name = name.trim();
        if name.is_empty()
            || name.chars().count() > MAX_CATEGORY_CHARS
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let keyword = repo::labels::keyword_for(name);
        let known = repo::labels::list(tx)?
            .into_iter()
            .any(|label| label.owner_account_id == Some(account_id) && label.keyword == keyword);
        if !known {
            // A label of this name that belongs elsewhere is left alone.
            if repo::labels::save(tx, None, name, CATEGORY_COLOR, 0, Some(account_id)).is_err() {
                continue;
            }
        }
        keywords.push(keyword);
    }
    repo::labels::reconcile_keywords(tx, message_id, &keywords)?;
    Ok(())
}

/// Drop the local folders (and their cached messages) of Graph folders that
/// the server no longer lists.
fn remove_vanished_folders(
    tx: &rusqlite::Transaction<'_>,
    account_id: i64,
    current: &[FolderMap],
) -> Result<()> {
    let known: HashSet<&str> = current.iter().map(|map| map.graph_id.as_str()).collect();
    let stored = {
        let mut stmt =
            tx.prepare("SELECT graph_id, folder_id FROM exchange_folders WHERE account_id = ?1")?;
        stmt.query_map(params![account_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (graph_id, folder_id) in stored {
        if known.contains(graph_id.as_str()) {
            continue;
        }
        let threads = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT thread_id FROM messages
                 WHERE folder_id = ?1 AND thread_id IS NOT NULL",
            )?;
            stmt.query_map(params![folder_id], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            params![folder_id],
        )?;
        tx.execute(
            "DELETE FROM exchange_folders WHERE account_id = ?1 AND graph_id = ?2",
            params![account_id, graph_id],
        )?;
        tx.execute("DELETE FROM folders WHERE id = ?1", params![folder_id])?;
        for thread in threads {
            repo::threads::recompute(tx, thread)?;
        }
    }
    Ok(())
}

struct NewMessage {
    message: GraphMessage,
    raw: Vec<u8>,
    parsed: crate::mime::ParsedBody,
}

enum Plan {
    Update {
        message: GraphMessage,
        local_id: i64,
        local_folder: i64,
    },
    New(Box<NewMessage>),
    Remove(String),
    /// A removal that is really a move to another folder of this mailbox.
    Moved {
        graph_id: String,
        local_id: i64,
        to_folder: i64,
    },
}

async fn saved_link(ctx: &SyncCtx, account_id: i64, graph_id: &str) -> Result<Option<String>> {
    let graph_id = graph_id.to_owned();
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT delta_link FROM exchange_folders WHERE account_id=?1 AND graph_id=?2",
                    params![account_id, graph_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten())
        })
        .await
}

async fn save_link(
    ctx: &SyncCtx,
    account_id: i64,
    graph_id: &str,
    link: Option<String>,
) -> Result<()> {
    let graph_id = graph_id.to_owned();
    ctx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE exchange_folders SET delta_link=?3 WHERE account_id=?1 AND graph_id=?2",
                params![account_id, graph_id, link],
            )?;
            Ok(())
        })
        .await
}

/// Ask Microsoft 365 to search the whole mailbox and bring the hits that are
/// not cached yet into the local database, where the normal search then finds
/// them. Returns how many messages were added. `folder` limits the search to
/// one Graph folder. The hits are not ordered here: once cached they are
/// ranked with every other message by the local search (BM25 over subject,
/// sender, recipients and body), because Graph's `$search` order is by date,
/// not by relevance.
pub(crate) async fn search_on_server(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    folder: Option<&str>,
    query: &str,
) -> Result<usize> {
    let account_id = config.id;
    let hits = client.search_messages(folder, query, 25).await?;
    if hits.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = hits.iter().map(|hit| hit.id.clone()).collect();
    let cached: HashSet<String> = ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT 1 FROM exchange_messages WHERE account_id=?1 AND graph_id=?2",
            )?;
            let mut out = HashSet::new();
            for id in ids {
                if stmt.exists(params![account_id, id])? {
                    out.insert(id);
                }
            }
            Ok(out)
        })
        .await?;
    // Group the new hits by the folder they live in.
    let mut by_folder: HashMap<String, Vec<DeltaItem>> = HashMap::new();
    for hit in hits {
        if cached.contains(&hit.id) {
            continue;
        }
        if let Some(parent) = hit.parent_folder_id.clone() {
            by_folder
                .entry(parent)
                .or_default()
                .push(DeltaItem::Upsert(hit));
        }
    }
    let mut added = 0;
    let mut changed: HashSet<i64> = HashSet::new();
    for (parent, items) in by_folder {
        let mapped: Option<(i64, Option<String>)> = ctx
            .db
            .read({
                let parent = parent.clone();
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT x.folder_id, f.role FROM exchange_folders x
                             JOIN folders f ON f.id = x.folder_id
                             WHERE x.account_id=?1 AND x.graph_id=?2",
                            params![account_id, parent],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?)
                }
            })
            .await?;
        // A folder the sync has not mapped yet is picked up by the next sync.
        let Some((folder_id, role)) = mapped else {
            continue;
        };
        let folder = FolderMap {
            graph_id: parent,
            folder_id,
            role: WELL_KNOWN
                .iter()
                .map(|(_, known)| *known)
                .find(|known| Some(*known) == role.as_deref()),
            name: "search result".into(),
        };
        let mut budget = items.len();
        let plans = prepare(ctx, config, client, &folder, &items, &mut budget).await?;
        added += plans
            .iter()
            .filter(|plan| matches!(plan, Plan::New(_)))
            .count();
        changed.extend(apply(ctx, config, &folder, plans).await?);
    }
    if !changed.is_empty() {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: changed.into_iter().collect(),
        });
    }
    Ok(added)
}

/// Sync one folder. Returns whether the cycle stopped before the end of it.
async fn sync_folder(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    folder: &FolderMap,
    budget: &mut usize,
) -> Result<bool> {
    let account_id = config.id;
    let mut link = saved_link(ctx, account_id, &folder.graph_id).await?;
    let mut restarted = false;
    let mut changed: HashSet<i64> = HashSet::new();
    let mut more = false;
    loop {
        // The first read of a folder is limited by the account's mail history
        // setting (Settings > Accounts); "all" reads everything.
        let cutoff = link
            .is_none()
            .then(|| {
                config
                    .settings
                    .mail_history
                    .months()
                    .map(|months| now_ms() - i64::from(months) * MONTH_DAYS * DAY_MS)
            })
            .flatten();
        let page = match client
            .delta(&folder.graph_id, link.as_deref(), cutoff)
            .await?
        {
            DeltaResult::Expired => {
                // The saved link is too old: read the folder again. Messages
                // already stored are matched by their Graph id, not re-added.
                if restarted {
                    return Err(CoreError::Network(
                        "Microsoft 365 keeps rejecting the sync state for a folder".into(),
                    ));
                }
                restarted = true;
                save_link(ctx, account_id, &folder.graph_id, None).await?;
                link = None;
                continue;
            }
            DeltaResult::Page(page) => page,
        };
        let plans = prepare(ctx, config, client, folder, &page.items, budget).await?;
        changed.extend(apply(ctx, config, folder, plans).await?);
        let resume = page.next_link.clone().or(page.delta_link.clone());
        save_link(ctx, account_id, &folder.graph_id, resume).await?;
        match page.next_link {
            Some(next) => {
                link = Some(next);
                if *budget == 0 {
                    more = true;
                    break;
                }
            }
            None => break,
        }
    }
    if !changed.is_empty() {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: changed.into_iter().collect(),
        });
    }
    Ok(more)
}

/// Decide what each change needs and download the MIME of new messages (no
/// database lock is held while waiting on the network).
async fn prepare(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    folder: &FolderMap,
    items: &[DeltaItem],
    budget: &mut usize,
) -> Result<Vec<Plan>> {
    let account_id = config.id;
    let ids: Vec<String> = items
        .iter()
        .map(|item| match item {
            DeltaItem::Upsert(message) => message.id.clone(),
            DeltaItem::Removed(id) => id.clone(),
        })
        .collect();
    let known: HashMap<String, (i64, i64)> = ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, folder_id FROM exchange_messages WHERE account_id=?1 AND graph_id=?2",
            )?;
            let mut out = HashMap::new();
            for id in ids {
                if let Some(found) = stmt
                    .query_row(params![account_id, id], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()?
                {
                    out.insert(id, found);
                }
            }
            Ok(out)
        })
        .await?;
    let mut plans = Vec::with_capacity(items.len());
    for item in items {
        match item {
            DeltaItem::Removed(id) => {
                // Graph reports a move as a removal here; ask where it went.
                if let Some(&(local_id, mapped_folder)) = known.get(id)
                    && mapped_folder == folder.folder_id
                    && let Some(parent) = client.message_parent(id).await?
                    && parent != folder.graph_id
                {
                    let destination: Option<i64> = ctx
                        .db
                        .read(move |conn| {
                            Ok(conn
                                .query_row(
                                    "SELECT folder_id FROM exchange_folders WHERE account_id=?1 AND graph_id=?2",
                                    params![account_id, parent],
                                    |row| row.get(0),
                                )
                                .optional()?)
                        })
                        .await?;
                    if let Some(to_folder) = destination {
                        plans.push(Plan::Moved {
                            graph_id: id.clone(),
                            local_id,
                            to_folder,
                        });
                    }
                    continue;
                }
                plans.push(Plan::Remove(id.clone()));
            }
            DeltaItem::Upsert(message) => {
                if let Some(&(local_id, local_folder)) = known.get(&message.id) {
                    plans.push(Plan::Update {
                        message: message.clone(),
                        local_id,
                        local_folder,
                    });
                    continue;
                }
                let raw = match fetch_text(client, config, message).await {
                    Ok(raw) => raw,
                    // Gone again before it could be read.
                    Err(CoreError::NotFound(_)) => continue,
                    // Graph cannot make this one message's MIME: keep what it
                    // can say about it instead of stopping the folder (and
                    // every message after it) on every cycle.
                    Err(error) if super::client::is_mime_conversion_failure(&error) => {
                        tracing::warn!(
                            account_id,
                            "exchange: a message has no MIME on the server, keeping a copy made from its JSON"
                        );
                        rebuilt_text(client, message).await?
                    }
                    Err(error) => return Err(error),
                };
                let Ok(parsed) = crate::mime::parse_message(&raw) else {
                    tracing::warn!(account_id, "exchange: skipping an unparseable message");
                    continue;
                };
                *budget = budget.saturating_sub(1);
                plans.push(Plan::New(Box::new(NewMessage {
                    message: message.clone(),
                    raw,
                    parsed,
                })));
            }
        }
    }
    Ok(plans)
}

/// The start of each note a cached copy that is not the whole message carries.
/// The reading pane finds them to offer "Download message" (`stub_kind`).
const NOTE_AGE: &str = "[This message is older than ";
const NOTE_SIZE: &str = "[This message is larger than ";
const NOTE_REBUILT: &str = "[Microsoft 365 could not provide this message in its original form";

/// Why a cached message is not the whole one, from its text or HTML: `age` and
/// `size` (past the account's offline limits: headers only) or `rebuilt`
/// (Graph could not produce its MIME: made from Graph's JSON). `None` for a
/// whole message.
pub fn stub_kind(text: Option<&str>, html: Option<&str>) -> Option<&'static str> {
    // The notes are the first thing in the text; looking only at its start
    // keeps a mail that merely quotes one from matching.
    for body in [text, html].into_iter().flatten() {
        let head: String = body.chars().take(400).collect();
        let head = head.trim_start().trim_start_matches("<p><i>");
        for (note, kind) in [
            (NOTE_AGE, "age"),
            (NOTE_SIZE, "size"),
            (NOTE_REBUILT, "rebuilt"),
        ] {
            if head.starts_with(note) {
                return Some(kind);
            }
        }
    }
    None
}

/// Download the whole of a message that was cached as headers only (or rebuilt)
/// and replace the cached copy: text, attachments, search entry. The account's
/// offline limits do not apply to a message asked for by name.
pub(crate) async fn download_message(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    message_id: i64,
) -> Result<()> {
    let account_id = config.id;
    let Some(graph_id) = graph_id_for(ctx, account_id, message_id).await? else {
        return Err(CoreError::NotFound("message".into()));
    };
    let raw = client.message_mime(&graph_id).await.map_err(|error| {
        if super::client::is_mime_conversion_failure(&error) {
            CoreError::Other(
                "Microsoft 365 cannot provide this message in its original form".into(),
            )
        } else {
            error
        }
    })?;
    let parsed = crate::mime::parse_message(&raw)?;
    let cache_dir = ctx.paths.mail_dir(account_id);
    let thread_id = ctx
        .db
        .write(move |conn| {
            std::fs::create_dir_all(&cache_dir)?;
            let tx = conn.transaction()?;
            let (thread_id, raw_path): (i64, Option<String>) = tx
                .query_row(
                    "SELECT thread_id, raw_path FROM messages WHERE id=?1 AND account_id=?2",
                    params![message_id, account_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| CoreError::NotFound("message".into()))?;
            let path = match raw_path {
                Some(path) => std::path::PathBuf::from(path),
                None => cache_dir.join(format!("{message_id}.local.eml")),
            };
            std::fs::write(&path, &raw)?;
            let has_attachments = parsed.attachments.iter().any(|a| !a.is_inline);
            repo::messages::store_body(
                &tx,
                message_id,
                parsed.text.as_deref(),
                parsed.html.as_deref(),
                Some(&path.to_string_lossy()),
                has_attachments,
                Some(&parsed.snippet),
            )?;
            tx.execute(
                "UPDATE messages SET size=?2 WHERE id=?1",
                params![message_id, raw.len() as i64],
            )?;
            let attachments = parsed
                .attachments
                .iter()
                .map(|attachment| repo::messages::NewAttachment {
                    message_id,
                    part_id: Some(&attachment.part_id),
                    filename: attachment.filename.as_deref(),
                    mime_type: attachment.mime_type.as_deref(),
                    size: Some(attachment.size),
                    content_id: attachment.content_id.as_deref(),
                    is_inline: attachment.is_inline,
                })
                .collect::<Vec<_>>();
            repo::messages::replace_attachments(&tx, message_id, &attachments)?;
            repo::search::index_message(&tx, message_id)?;
            repo::threads::recompute(&tx, thread_id)?;
            tx.commit()?;
            Ok(thread_id)
        })
        .await?;
    ctx.bus.emit(CoreEvent::MailUpdated {
        thread_ids: vec![thread_id],
    });
    Ok(())
}

/// The message as it is cached: whole, or (for a message past the account's
/// offline limits) its headers with a note that the text stayed on the server.
async fn fetch_text(
    client: &GraphMail,
    config: &AccountConfig,
    message: &GraphMessage,
) -> Result<Vec<u8>> {
    let settings = &config.settings.connection.exchange;
    let too_old = settings.body_days > 0
        && message
            .received_ms
            .is_some_and(|at| at < now_ms() - i64::from(settings.body_days) * DAY_MS);
    let limit = (settings.message_limit_kb > 0)
        .then(|| usize::try_from(settings.message_limit_kb).unwrap_or(usize::MAX) * 1024);
    let note = if too_old {
        Some(format!(
            "{NOTE_AGE}{} days, so its text was not downloaded. It is still in your Microsoft 365 mailbox.]",
            settings.body_days
        ))
    } else {
        match limit {
            Some(limit) => match client.message_mime_within(&message.id, limit).await? {
                Some(raw) => return Ok(raw),
                None => Some(format!(
                    "{NOTE_SIZE}{} KiB, so its text was not downloaded. It is still in your Microsoft 365 mailbox.]",
                    settings.message_limit_kb
                )),
            },
            None => None,
        }
    };
    let Some(note) = note else {
        return client.message_mime(&message.id).await;
    };
    let mut head = client.message_headers(&message.id).await?;
    if head.is_empty() {
        // Graph sent no headers: keep what the listing knows.
        head = format!(
            "Subject: {}\r\n",
            message.subject.replace(['\r', '\n'], " ")
        )
        .into_bytes();
    }
    // The Message-ID keeps the cached copy matched with the server's.
    if let Some(id) = &message.internet_message_id
        && !String::from_utf8_lossy(&head)
            .to_ascii_lowercase()
            .contains("message-id:")
    {
        head.extend_from_slice(format!("Message-ID: {id}\r\n").as_bytes());
    }
    Ok(crate::nntp::placeholder_with_note(&head, &note))
}

/// A copy of a message whose MIME Graph cannot produce: its headers and body
/// as Graph reports them, or (when even that fails) just its subject and a note.
async fn rebuilt_text(client: &GraphMail, message: &GraphMessage) -> Result<Vec<u8>> {
    let note = format!(
        "{NOTE_REBUILT}, so this copy was rebuilt from the information it gave. Attachments and some formatting may be missing. The original is still in your Microsoft 365 mailbox.]"
    );
    let note = note.as_str();
    match client.message_json_copy(&message.id).await {
        Ok(value) => Ok(mime_from_json(message, &value, note)),
        Err(error @ (CoreError::Auth(_) | CoreError::NeedsReauth | CoreError::Offline)) => {
            Err(error)
        }
        Err(_) => {
            let mut head =
                format!("Subject: {}\r\n", encode_header_text(&message.subject)).into_bytes();
            if let Some(id) = &message.internet_message_id {
                head.extend_from_slice(format!("Message-ID: {id}\r\n").as_bytes());
            }
            Ok(crate::nntp::placeholder_with_note(&head, note))
        }
    }
}

/// A header value that is safe on one line: plain ASCII stays, anything else
/// becomes one encoded word.
fn encode_header_text(text: &str) -> String {
    use base64::Engine as _;
    let clean: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if clean.is_ascii() {
        return clean;
    }
    format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(clean.as_bytes())
    )
}

fn json_address(value: &serde_json::Value) -> Option<String> {
    let address = value["emailAddress"]["address"].as_str()?.trim();
    if address.is_empty()
        || address
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    let name = value["emailAddress"]["name"].as_str().unwrap_or("").trim();
    Some(if name.is_empty() {
        format!("<{address}>")
    } else {
        let encoded = encode_header_text(name);
        if encoded.is_ascii() && !encoded.starts_with("=?") {
            format!("\"{}\" <{address}>", encoded.replace(['\\', '"'], ""))
        } else {
            format!("{encoded} <{address}>")
        }
    })
}

fn json_addresses(value: &serde_json::Value) -> String {
    value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(json_address)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// MIME made from the JSON form of a message (see [`GraphMail::message_json_copy`]).
fn mime_from_json(message: &GraphMessage, value: &serde_json::Value, note: &str) -> Vec<u8> {
    use base64::Engine as _;
    let subject = value["subject"].as_str().unwrap_or(&message.subject);
    let mut out = format!("Subject: {}\r\n", encode_header_text(subject));
    if let Some(from) = json_address(&value["from"]) {
        out.push_str(&format!("From: {from}\r\n"));
    }
    let to = json_addresses(&value["toRecipients"]);
    if !to.is_empty() {
        out.push_str(&format!("To: {to}\r\n"));
    }
    let cc = json_addresses(&value["ccRecipients"]);
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {cc}\r\n"));
    }
    let when = value["sentDateTime"]
        .as_str()
        .or_else(|| value["receivedDateTime"].as_str())
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
    if let Some(when) = when {
        out.push_str(&format!("Date: {}\r\n", when.to_rfc2822()));
    }
    let id = value["internetMessageId"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| message.internet_message_id.clone());
    if let Some(id) = id.filter(|id| !id.contains(['\r', '\n'])) {
        out.push_str(&format!("Message-ID: {id}\r\n"));
    }
    let content = value["body"]["content"].as_str().unwrap_or_default();
    let html = value["body"]["contentType"]
        .as_str()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("html"));
    let body = if html {
        format!("<p><i>{note}</i></p>{content}")
    } else {
        format!("{note}\r\n\r\n{content}")
    };
    out.push_str(&format!(
        "MIME-Version: 1.0\r\nContent-Type: text/{}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        if html { "html" } else { "plain" }
    ));
    let encoded = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
    for line in encoded.as_bytes().chunks(76) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push_str("\r\n");
    }
    out.into_bytes()
}

fn has_pending_action(conn: &rusqlite::Connection, message_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_actions
         WHERE message_id=?1 AND state IN ('pending','inflight'))",
        params![message_id],
        |row| row.get(0),
    )?)
}

async fn apply(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder: &FolderMap,
    plans: Vec<Plan>,
) -> Result<HashSet<i64>> {
    if plans.is_empty() {
        return Ok(HashSet::new());
    }
    let account_id = config.id;
    let folder_id = folder.folder_id;
    let role = folder.role;
    let cache_dir = ctx.paths.mail_dir(account_id);
    let remind = config.settings.connection.exchange.flag_reminders;
    let (threads, reminders, settled) = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            std::fs::create_dir_all(&cache_dir)?;
            let mut threads: HashSet<i64> = HashSet::new();
            // Flagged messages with a due date: (thread, due, subject).
            let mut reminders: Vec<(i64, i64)> = Vec::new();
            // Threads whose flag changed: (thread, completed, due now).
            let mut settled: Vec<(i64, bool, Option<i64>)> = Vec::new();
            let now = now_ms();
            for plan in &plans {
                match plan {
                    Plan::Remove(graph_id) => {
                        let mapped: Option<(i64, i64)> = tx
                            .query_row(
                                "SELECT message_id, folder_id FROM exchange_messages
                                 WHERE account_id=?1 AND graph_id=?2",
                                params![account_id, graph_id],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?;
                        // A move shows up as a removal here and an upsert in
                        // the new folder; only delete what still lives here.
                        let Some((message_id, mapped_folder)) = mapped else {
                            continue;
                        };
                        if mapped_folder != folder_id || has_pending_action(&tx, message_id)? {
                            continue;
                        }
                        if let Some(row) = repo::messages::get_row(&tx, message_id)? {
                            repo::messages::delete(&tx, message_id)?;
                            if let Some(thread_id) = row.thread_id {
                                repo::threads::recompute(&tx, thread_id)?;
                                threads.insert(thread_id);
                            }
                        }
                    }
                    Plan::Moved {
                        graph_id,
                        local_id,
                        to_folder,
                    } => {
                        if has_pending_action(&tx, *local_id)? {
                            continue;
                        }
                        repo::messages::set_uid_and_folder(&tx, *local_id, *to_folder, None)?;
                        tx.execute(
                            "UPDATE exchange_messages SET folder_id=?3 WHERE account_id=?1 AND graph_id=?2",
                            params![account_id, graph_id, to_folder],
                        )?;
                        if let Some(row) = repo::messages::get_row(&tx, *local_id)?
                            && let Some(thread_id) = row.thread_id
                        {
                            repo::threads::recompute(&tx, thread_id)?;
                            threads.insert(thread_id);
                        }
                    }
                    Plan::Update {
                        message,
                        local_id,
                        local_folder,
                    } => {
                        if has_pending_action(&tx, *local_id)? {
                            continue;
                        }
                        repo::messages::set_flags(&tx, *local_id, message.is_read, message.flagged)?;
                        sync_categories(&tx, account_id, *local_id, &message.categories)?;
                        if *local_folder != folder_id {
                            repo::messages::set_uid_and_folder(&tx, *local_id, folder_id, None)?;
                            tx.execute(
                                "UPDATE exchange_messages SET folder_id=?3 WHERE account_id=?1 AND graph_id=?2",
                                params![account_id, message.id, folder_id],
                            )?;
                        }
                        if let Some(row) = repo::messages::get_row(&tx, *local_id)?
                            && let Some(thread_id) = row.thread_id
                        {
                            repo::threads::recompute(&tx, thread_id)?;
                            threads.insert(thread_id);
                            if let Some(due) = reminder_due(remind, message, now) {
                                reminders.push((thread_id, due));
                            }
                            if remind && (message.flag_complete || message.flagged) {
                                settled.push((
                                    thread_id,
                                    message.flag_complete,
                                    message.flag_due_ms,
                                ));
                            }
                        }
                    }
                    Plan::New(new) => {
                        let NewMessage {
                            message,
                            raw,
                            parsed,
                        } = &**new;
                        let flags = FlagSet {
                            seen: message.is_read,
                            flagged: message.flagged,
                            draft: message.is_draft,
                            trashed: false,
                        };
                        // A message we sent ourselves is already stored (by
                        // Message-ID) without a server link: adopt it.
                        let adoptable = match parsed.headers.message_id.as_deref() {
                            Some(mid) => repo::messages::by_message_id(&tx, account_id, mid)?
                                .filter(|row| {
                                    tx.query_row(
                                        "SELECT NOT EXISTS(SELECT 1 FROM exchange_messages WHERE message_id=?1)",
                                        params![row.id],
                                        |r| r.get::<_, bool>(0),
                                    )
                                    .unwrap_or(false)
                                }),
                            None => None,
                        };
                        let (local_id, thread_id) = if let Some(existing) = adoptable {
                            repo::messages::set_uid_and_folder(&tx, existing.id, folder_id, None)?;
                            repo::messages::set_flags(&tx, existing.id, message.is_read, message.flagged)?;
                            (existing.id, existing.thread_id.unwrap_or(0))
                        } else {
                            insert_parsed(
                                &tx,
                                &ParsedMessage {
                                    account_id,
                                    folder_id,
                                    role,
                                    cache_dir: &cache_dir,
                                    parsed,
                                    raw,
                                    flags,
                                    fallback_ms: message.received_ms.unwrap_or_else(now_ms),
                                },
                            )?
                        };
                        tx.execute(
                            "INSERT OR REPLACE INTO exchange_messages(account_id, graph_id, message_id, folder_id)
                             VALUES (?1,?2,?3,?4)",
                            params![account_id, message.id, local_id, folder_id],
                        )?;
                        sync_categories(&tx, account_id, local_id, &message.categories)?;
                        if thread_id != 0 {
                            repo::threads::recompute(&tx, thread_id)?;
                            threads.insert(thread_id);
                            if let Some(due) = reminder_due(remind, message, now) {
                                reminders.push((thread_id, due));
                            }
                        }
                    }
                }
            }
            tx.commit()?;
            Ok((threads, reminders, settled))
        })
        .await?;
    if !settled.is_empty() {
        settle_reminder_tasks(ctx, settled).await;
    }
    if !reminders.is_empty() {
        make_reminder_tasks(ctx, reminders).await;
    }
    Ok(threads)
}

/// The due time of a flag that should become a task: the account asked for
/// reminders, the message is flagged, and the due date is not in the past
/// (mail that was flagged long ago is not turned into a pile of overdue tasks).
fn reminder_due(remind: bool, message: &GraphMessage, now: i64) -> Option<i64> {
    if !remind || !message.flagged {
        return None;
    }
    message.flag_due_ms.filter(|due| *due >= now)
}

/// One task per conversation (an existing one is left alone), due when the
/// flag says. A failure only means no task this time.
async fn make_reminder_tasks(ctx: &SyncCtx, reminders: Vec<(i64, i64)>) {
    use crate::tasks::{ColumnKind, SqliteTaskStore, TaskDraft, TaskStore};
    let made = ctx
        .db
        .write(move |conn| {
            let mut made = 0;
            // The tasks module seeds its first project when it is first used.
            SqliteTaskStore::new(conn).ensure_default_project()?;
            for (thread_id, due) in reminders {
                let mut store = SqliteTaskStore::new(conn);
                if !store.threads_with_tasks(Some(&[thread_id]))?.is_empty() {
                    continue;
                }
                let created = store.create_from_thread(
                    thread_id,
                    TaskDraft {
                        title: None,
                        project_id: None,
                        status: ColumnKind::Next,
                        due_at: Some(due),
                        context_id: None,
                        label_ids: Vec::new(),
                    },
                )?;
                // Remember where the task came from and which due date the
                // flag had, so a later change of the flag can follow it.
                conn.execute(
                    "INSERT INTO task_activity (task_id, kind, text, at) VALUES (?1, ?2, ?3, ?4)",
                    params![created.id, FLAG_MARKER, due.to_string(), now_ms()],
                )?;
                made += 1;
            }
            Ok(made)
        })
        .await;
    match made {
        Ok(made) if made > 0 => ctx.bus.emit(CoreEvent::TasksChanged),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "exchange: could not make a task from a flag"),
    }
}

/// Task activity kind that marks a task made from a follow-up flag; its text
/// is the flag's due time (UTC ms) the task last followed.
const FLAG_MARKER: &str = "flag_reminder";

/// A flag marked complete finishes the task that was made from it; a flag that
/// got a new due date moves the task's due date, unless the flag still has the
/// date the task already followed (then a date changed by hand stays). Tasks
/// made by hand, or from other mail, are never touched.
async fn settle_reminder_tasks(ctx: &SyncCtx, settled: Vec<(i64, bool, Option<i64>)>) {
    use crate::tasks::{ColumnKind, SqliteTaskStore, TaskPatch, TaskStore};
    let result = ctx
        .db
        .write(move |conn| {
            let mut changed = false;
            for (thread_id, complete, due) in settled {
                let tasks = SqliteTaskStore::new(conn).threads_with_tasks(Some(&[thread_id]))?;
                for task in tasks {
                    if task.status_kind == ColumnKind::Done {
                        continue;
                    }
                    let followed: Option<i64> = conn
                        .query_row(
                            "SELECT text FROM task_activity WHERE task_id=?1 AND kind=?2
                             ORDER BY id DESC LIMIT 1",
                            params![task.task_id, FLAG_MARKER],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                        .map(|text| text.parse().unwrap_or(0));
                    let Some(followed) = followed else {
                        continue;
                    };
                    let mut store = SqliteTaskStore::new(conn);
                    if complete {
                        store.complete_task(task.task_id)?;
                        changed = true;
                    } else if let Some(due) = due
                        && due != followed
                        && due >= now_ms()
                    {
                        store.update_task(
                            task.task_id,
                            TaskPatch {
                                due_at: Some(Some(due)),
                                ..Default::default()
                            },
                        )?;
                        conn.execute(
                            "INSERT INTO task_activity (task_id, kind, text, at)
                             VALUES (?1, ?2, ?3, ?4)",
                            params![task.task_id, FLAG_MARKER, due.to_string(), now_ms()],
                        )?;
                        changed = true;
                    }
                }
            }
            Ok(changed)
        })
        .await;
    match result {
        Ok(true) => ctx.bus.emit(CoreEvent::TasksChanged),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "exchange: could not follow a flag change"),
    }
}

async fn graph_id_for(ctx: &SyncCtx, account_id: i64, message_id: i64) -> Result<Option<String>> {
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT graph_id FROM exchange_messages WHERE account_id=?1 AND message_id=?2",
                    params![account_id, message_id],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
}

async fn run_due_actions(ctx: &SyncCtx, config: &AccountConfig, client: &GraphMail) -> Result<()> {
    let account_id = config.id;
    let actions = ctx
        .db
        .read(move |conn| repo::actions::due_except_send(conn, account_id, now_ms(), 50))
        .await?;
    for action in actions {
        let action_id = action.id;
        if !ctx
            .db
            .write(move |conn| repo::actions::try_claim(conn, action_id))
            .await?
        {
            continue;
        }
        match execute(ctx, config, client, &action).await {
            Ok(()) => {
                ctx.db
                    .write(move |conn| repo::actions::set_state(conn, action_id, "done", None))
                    .await?;
                ctx.bus.emit(CoreEvent::ActionState {
                    action_id,
                    state: "done".into(),
                    error: None,
                });
            }
            Err(error @ (CoreError::Auth(_) | CoreError::NeedsReauth)) => {
                let message = error.to_string();
                ctx.db
                    .write(move |conn| {
                        repo::actions::bump_attempt(conn, action_id, now_ms() + 60_000, &message)
                    })
                    .await?;
                return Err(error);
            }
            // No connection says nothing about the action: keep its retry
            // budget and wait for the network (or the offline switch).
            Err(CoreError::Offline) => {
                let retry_at = now_ms() + 60_000;
                ctx.db
                    .write(move |conn| {
                        repo::actions::defer_offline(conn, action_id, retry_at, "offline")
                    })
                    .await?;
                return Err(CoreError::Offline);
            }
            Err(error) => {
                let message = error.to_string();
                let attempts = action.attempts + 1;
                let saved = message.clone();
                if attempts >= MAX_ACTION_ATTEMPTS {
                    ctx.db
                        .write(move |conn| {
                            repo::actions::set_state(conn, action_id, "failed", Some(&saved))
                        })
                        .await?;
                    ctx.bus.emit(CoreEvent::ActionState {
                        action_id,
                        state: "failed".into(),
                        error: Some(message),
                    });
                } else {
                    let retry_at = now_ms() + (1_i64 << attempts.min(8)) * 1000 + action_id % 997;
                    ctx.db
                        .write(move |conn| {
                            repo::actions::bump_attempt(conn, action_id, retry_at, &saved)
                        })
                        .await?;
                    tracing::warn!(account_id, action_id, kind = %action.kind, error = %message, "exchange action retry scheduled");
                }
            }
        }
    }
    Ok(())
}

async fn execute(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    action: &repo::actions::PendingAction,
) -> Result<()> {
    let account_id = config.id;
    match action.kind.as_str() {
        "mark_read" | "mark_unread" | "star" | "unstar" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            // Not linked to the server (yet): nothing to change remotely.
            let Some(graph_id) = graph_id_for(ctx, account_id, message_id).await? else {
                return Ok(());
            };
            let result = match action.kind.as_str() {
                "mark_read" => client.set_read(&graph_id, true).await,
                "mark_unread" => client.set_read(&graph_id, false).await,
                "star" => client.set_flagged(&graph_id, true).await,
                _ => client.set_flagged(&graph_id, false).await,
            };
            match result {
                Err(CoreError::NotFound(_)) => Ok(()),
                other => other,
            }
        }
        "archive" | "unarchive" | "trash" | "spam" | "not_spam" | "move" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            let Some(graph_id) = graph_id_for(ctx, account_id, message_id).await? else {
                return Ok(());
            };
            let target = action.payload["targetFolderId"]
                .as_i64()
                .ok_or_else(|| CoreError::NotFound("target folder for a queued move".into()))?;
            let destination: Option<String> = ctx
                .db
                .read(move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT graph_id FROM exchange_folders WHERE account_id=?1 AND folder_id=?2",
                            params![account_id, target],
                            |row| row.get(0),
                        )
                        .optional()?)
                })
                .await?;
            let destination = destination.ok_or_else(|| {
                CoreError::Other("the destination folder is not a Microsoft 365 folder".into())
            })?;
            let moved = match client.move_message(&graph_id, &destination).await {
                Ok(id) => id,
                Err(CoreError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            ctx.db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute(
                        "DELETE FROM exchange_messages WHERE account_id=?1 AND graph_id=?2",
                        params![account_id, graph_id],
                    )?;
                    tx.execute(
                        "INSERT OR REPLACE INTO exchange_messages(account_id, graph_id, message_id, folder_id)
                         VALUES (?1,?2,?3,?4)",
                        params![account_id, moved, message_id, target],
                    )?;
                    tx.commit()?;
                    Ok(())
                })
                .await
        }
        "delete_permanently" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            if crate::sync::is_local_only_draft(ctx, message_id).await? {
                return crate::sync::finish_permanent_delete(ctx, Some(message_id)).await;
            }
            if let Some(graph_id) = graph_id_for(ctx, account_id, message_id).await? {
                client.permanent_delete(&graph_id).await?;
            }
            crate::sync::finish_permanent_delete(ctx, Some(message_id)).await
        }
        "empty_trash" => {
            let folder_id = action.payload["folderId"]
                .as_i64()
                .ok_or_else(|| CoreError::NotFound("Trash folder for a queued purge".into()))?;
            let folder = ctx
                .db
                .read(move |conn| repo::folders::get(conn, folder_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("Trash folder".into()))?;
            if folder.account_id != account_id || folder.role.as_deref() != Some(roles::TRASH) {
                return Err(CoreError::Other(
                    "the queued Trash folder belongs to another account".into(),
                ));
            }
            let graph_folder: Option<String> = ctx
                .db
                .read(move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT graph_id FROM exchange_folders WHERE account_id=?1 AND folder_id=?2",
                            params![account_id, folder_id],
                            |row| row.get(0),
                        )
                        .optional()?)
                })
                .await?;
            let graph_folder =
                graph_folder.ok_or_else(|| CoreError::NotFound("Deleted Items folder".into()))?;
            let snapshot = crate::sync::snapshot_trash_ids(ctx, account_id).await?;
            let ids = client.list_message_ids(&graph_folder).await?;
            let requests: Vec<BatchRequest> = ids
                .iter()
                .map(|id| BatchRequest {
                    method: "POST",
                    url: client.batch_message_url(id, Some("permanentDelete")),
                    body: Some(serde_json::json!({})),
                })
                .collect();
            for response in client.batch(requests).await? {
                if !(200..300).contains(&response.status) && response.status != 404 {
                    return Err(CoreError::Network(format!(
                        "Microsoft 365 refused to delete a message ({})",
                        response.status
                    )));
                }
            }
            crate::sync::finish_empty_trash(ctx, account_id, snapshot).await
        }
        // A label is a category of the message on the server.
        "add_label" | "remove_label" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            let Some(graph_id) = graph_id_for(ctx, account_id, message_id).await? else {
                return Ok(());
            };
            let Some(keyword) = action.payload["keyword"].as_str() else {
                return Ok(());
            };
            // The category carries the label's display name, not its keyword.
            let label_name = match action.payload["labelId"].as_i64() {
                Some(label_id) => ctx
                    .db
                    .read(move |conn| repo::labels::get(conn, label_id))
                    .await?
                    .map(|label| label.name),
                None => None,
            }
            .unwrap_or_else(|| keyword.to_owned());
            let mut categories = match client.categories(&graph_id).await {
                Ok(categories) => categories,
                Err(CoreError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let before = categories.len();
            categories.retain(|category| repo::labels::keyword_for(category) != keyword);
            if action.kind == "add_label" {
                categories.push(label_name);
            }
            if categories.len() == before && action.kind == "remove_label" {
                return Ok(());
            }
            match client.set_categories(&graph_id, &categories).await {
                Err(CoreError::NotFound(_)) => Ok(()),
                other => other,
            }
        }
        "save_draft" => {
            let Some(draft_id) = action.payload["draftId"].as_i64().or(action.message_id) else {
                return Ok(());
            };
            super::draft::push_draft(ctx, config, client, draft_id)
                .await
                .map_err(|error| super::client::during("saving a draft", error))
        }
        "delete_draft" => match action.payload["graphId"].as_str() {
            Some(graph_id) => {
                super::draft::delete_remote_draft(ctx, account_id, client, graph_id).await
            }
            None => Ok(()),
        },
        // Snoozes stay in the app database (see the roadmap).
        "snooze" | "unsnooze" => Ok(()),
        other => {
            tracing::warn!("exchange: unsupported queued action {other}");
            Ok(())
        }
    }
}

/// Every message is cached whole, so a body request has nothing to fetch.
async fn body_actor(ctx: SyncCtx, mut rx: mpsc::Receiver<PriorityFetchCmd>) {
    while let Some(command) = rx.recv().await {
        match command {
            PriorityFetchCmd::Body(message_id) => {
                let _ = ctx
                    .db
                    .write(move |conn| repo::messages::cancel_body_fetch(conn, message_id))
                    .await;
            }
            PriorityFetchCmd::Original { complete, .. } => {
                let _ = complete.send(Err(
                    "this kind of account cannot download the original message directly".into(),
                ));
            }
            PriorityFetchCmd::Attachment {
                attachment_id,
                complete,
            } => {
                let result = crate::nntp::attachment_bytes(&ctx, attachment_id)
                    .await
                    .map_err(|error| error.to_string());
                let _ = complete.send(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mail_poll_is_a_minute_backs_off_and_never_beats_the_normal_cadence_when_unwatched() {
        // Unwatched: the configured cadence, whatever failed.
        assert_eq!(mail_poll_secs(300, false, 0), 300);
        assert_eq!(mail_poll_secs(300, false, 5), 300);
        // Watched: a minute, doubled after each failure, capped by the cadence.
        assert_eq!(mail_poll_secs(300, true, 0), 60);
        assert_eq!(mail_poll_secs(300, true, 1), 120);
        assert_eq!(mail_poll_secs(300, true, 2), 240);
        assert_eq!(mail_poll_secs(300, true, 3), 300);
        assert_eq!(mail_poll_secs(900, true, 10), 600);
        // A cadence shorter than the poll (1 minute) is the poll.
        assert_eq!(mail_poll_secs(60, true, 4), 60);
    }

    #[test]
    fn a_failed_step_is_named_but_sign_in_and_offline_errors_keep_their_kind() {
        let named = in_step::<()>(
            "reading folder inbox",
            Err(CoreError::Other("Microsoft Graph returned 500".into())),
        )
        .unwrap_err();
        assert!(
            named
                .to_string()
                .starts_with("reading folder inbox: Microsoft Graph returned 500"),
            "{named}"
        );
        assert!(matches!(
            in_step::<()>("x", Err(CoreError::NeedsReauth)),
            Err(CoreError::NeedsReauth)
        ));
        assert!(matches!(
            in_step::<()>("x", Err(CoreError::Offline)),
            Err(CoreError::Offline)
        ));
        assert_eq!(in_step("x", Ok(3)).unwrap(), 3);
    }

    #[test]
    fn only_the_start_of_a_text_marks_a_message_as_not_whole() {
        let note = format!("{NOTE_SIZE}1 KiB, so its text was not downloaded.]");
        assert_eq!(stub_kind(Some(&note), None), Some("size"));
        assert_eq!(
            stub_kind(
                None,
                Some(&format!("<p><i>{NOTE_REBUILT}, so ...]</i></p>"))
            ),
            Some("rebuilt")
        );
        assert_eq!(
            stub_kind(Some(&format!("{NOTE_AGE}30 days")), None),
            Some("age")
        );
        assert_eq!(
            stub_kind(Some("hi\n> [This message is larger than 1 KiB"), None),
            None
        );
        assert_eq!(stub_kind(None, None), None);
    }
}
