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
const INITIAL_WINDOW_DAYS: i64 = 90;
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
        if settings.has_changed().unwrap_or(false) {
            config.settings = settings.borrow_and_update().clone();
        }
        set_state(&ctx, config.id, "syncing").await;
        let mut more = false;
        let outcome = match super::graph_for(&ctx, &config) {
            Ok(client) => cycle(&ctx, &config, &client).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(remaining) => {
                more = remaining;
                set_state(&ctx, config.id, "idle").await;
                finish_waiters(&mut waiters, Ok(()));
            }
            Err(error) => {
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
        let wait = if more {
            Duration::from_secs(1)
        } else {
            crate::sync::engine::configured_sync_interval(&ctx.db).await
        };
        tokio::select! {
            command = rx.recv() => match command {
                None | Some(SyncCmd::Shutdown) => return,
                Some(SyncCmd::SyncNow { complete: Some(done) }) => waiters.push(done),
                _ => {}
            },
            _ = settings.changed() => {},
            _ = tokio::time::sleep(wait) => {},
        }
    }
}

struct FolderMap {
    graph_id: String,
    folder_id: i64,
    role: Option<&'static str>,
}

/// One sync cycle. Returns whether more mail is waiting on the server.
pub(crate) async fn cycle(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
) -> Result<bool> {
    let folders = sync_folders(ctx, config, client).await?;
    run_due_actions(ctx, config, client).await?;
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
        more |= sync_folder(ctx, config, client, folder, &mut budget).await?;
    }
    Ok(more)
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
            let none = repo::folders::FolderPermissions {
                can_create_children: false,
                can_rename: false,
                can_delete: false,
            };
            let mut out = Vec::new();
            for (graph_id, path, role) in entries {
                let folder_id = repo::folders::upsert_discovered(
                    &tx, account_id, &path, Some("/"), role, true, none,
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
        let cutoff = link
            .is_none()
            .then(|| now_ms() - INITIAL_WINDOW_DAYS * DAY_MS);
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
                let raw = match client.message_mime(&message.id).await {
                    Ok(raw) => raw,
                    // Gone again before it could be read.
                    Err(CoreError::NotFound(_)) => continue,
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
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            std::fs::create_dir_all(&cache_dir)?;
            let mut threads: HashSet<i64> = HashSet::new();
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
                        }
                    }
                }
            }
            tx.commit()?;
            Ok(threads)
        })
        .await
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
        // Drafts and snoozes stay in the app database (see the roadmap).
        "save_draft" | "delete_draft" | "snooze" | "unsnooze" => Ok(()),
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
