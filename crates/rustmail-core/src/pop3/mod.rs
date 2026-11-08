//! POP3 accounts.
//!
//! Mail is downloaded into the account's Inbox. With "leave on server" the
//! server copy is kept (optionally deleted after N days) and a table of UIDLs
//! (`pop3_seen`) stops a message from being downloaded twice; otherwise a
//! message is deleted from the server only after it has been stored locally.
//! Messages over the size limit are imported header-only and stay on the
//! server. New Inbox mail goes through the normal filter-on-receive pass,
//! because it is stored like any other Inbox mail and announced with
//! `MailUpdated`. Sending uses the account's SMTP settings, optionally after a
//! POP login (POP-before-SMTP).
//!
//! POP3 has no folders and no flags, so read state, stars, moves and labels
//! live only in the app database.

pub mod client;
#[allow(clippy::chunks_exact_to_as_chunks, clippy::needless_range_loop)]
mod md5;

use crate::accounts::secrets::{self, Slot};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::local::scan::{FlagSet, ParsedMessage, insert_parsed};
use crate::models::{AccountConfig, MailProtocol, now_ms, roles};
use crate::sync::engine::{PriorityFetchCmd, SyncCmd, SyncCtx, set_state, set_state_error};
use client::{Login, Pop3Client};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Messages downloaded per sync cycle; the rest follows in the next cycle.
const CYCLE_MESSAGES: usize = 200;
const FLUSH_MESSAGES: usize = 25;
const FLUSH_BYTES: usize = 8 * 1024 * 1024;
const MAX_ACTION_ATTEMPTS: i64 = 8;
const DAY_MS: i64 = 86_400_000;

pub(crate) fn spawn(
    ctx: SyncCtx,
    config: AccountConfig,
    rx: mpsc::Receiver<SyncCmd>,
    body_rx: mpsc::Receiver<PriorityFetchCmd>,
    settings_rx: watch::Receiver<crate::models::AccountSettings>,
) -> Vec<tokio::task::JoinHandle<()>> {
    debug_assert_eq!(config.mail_protocol, MailProtocol::Pop3);
    vec![
        tokio::spawn(actor(ctx.clone(), config.clone(), rx, settings_rx)),
        tokio::spawn(body_actor(ctx, config, body_rx)),
    ]
}

/// Open an authenticated session to the account's POP3 server.
pub(crate) async fn open(ctx: &SyncCtx, config: &AccountConfig) -> Result<Pop3Client> {
    let hydrated = secrets::hydrate_account(&ctx.credentials, config).await;
    let config = &hydrated;
    let password = secrets::load_async(ctx.credentials.clone(), config.id, Slot::Password).await?;
    let login = Login {
        user: config.username.trim().to_owned(),
        password,
    };
    Pop3Client::connect(
        &config.imap_host,
        config.imap_port,
        &config.settings.connection,
        &login,
    )
    .await
}

/// POP-before-SMTP: authenticate to the POP3 server so it admits this address
/// to the SMTP server, then log out.
pub(crate) async fn authorize_smtp(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
    let client = open(ctx, config).await?;
    client.quit_quietly().await;
    Ok(())
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
        match cycle(&ctx, &config).await {
            Ok(remaining) => {
                more = remaining;
                set_state(&ctx, config.id, "idle").await;
                finish_waiters(&mut waiters, Ok(()));
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(account_id = config.id, %message, "pop3 sync failed");
                let state = if matches!(error, CoreError::Auth(_)) {
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

/// Run a one-off sync (used by tests).
pub async fn sync_once(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
    cycle(ctx, config).await.map(|_| ())
}

/// A downloaded message waiting to be written.
struct Fetched {
    uidl: Option<String>,
    raw: Vec<u8>,
    parsed: crate::mime::ParsedBody,
    skipped: bool,
}

/// What the cycle learned about a stored UIDL.
struct Seen {
    skipped: bool,
    downloaded_at: i64,
}

/// One sync cycle. Returns whether more messages are waiting on the server.
async fn cycle(ctx: &SyncCtx, config: &AccountConfig) -> Result<bool> {
    let inbox_id = sync_folders(ctx, config).await?;
    run_due_actions(ctx, config).await?;
    let settings = &config.settings.connection;
    let leave = settings.pop3_leave_on_server;
    let max_bytes = u64::from(settings.pop3_max_size_kb) * 1024;
    let mut client = open(ctx, config).await?;

    let sizes: HashMap<u32, u64> = client.list().await?.into_iter().collect();
    let uidls = client.uidl().await?;
    if uidls.is_none() && leave {
        client.quit_quietly().await;
        return Err(CoreError::Other(
            "this POP3 server does not support UIDL, which \"leave messages on the server\" needs; turn that option off".into(),
        ));
    }
    // Without UIDL every listed message is new and is deleted after download.
    let entries: Vec<(u32, Option<String>)> = match uidls {
        Some(list) => {
            let mut seen_ids = HashSet::new();
            list.into_iter()
                // A server that repeats an id would otherwise be fetched twice.
                .filter(|(_, uidl)| seen_ids.insert(uidl.clone()))
                .filter(|(number, _)| sizes.contains_key(number))
                .map(|(number, uidl)| (number, Some(uidl)))
                .collect()
        }
        None => {
            let mut numbers: Vec<u32> = sizes.keys().copied().collect();
            numbers.sort_unstable();
            numbers.into_iter().map(|n| (n, None)).collect()
        }
    };
    let account_id = config.id;
    let seen: HashMap<String, Seen> = ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT uidl, skipped, downloaded_at FROM pop3_seen WHERE account_id=?1",
            )?;
            let rows = stmt
                .query_map(params![account_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Seen {
                            skipped: row.get::<_, i64>(1)? != 0,
                            downloaded_at: row.get(2)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await?;

    let mut changed = HashSet::new();
    let mut to_delete: Vec<u32> = Vec::new();
    let mut deleted_ids: HashSet<String> = HashSet::new();
    let mut pending: Vec<Fetched> = Vec::new();
    let mut pending_bytes = 0usize;
    // Messages whose deletion waits for their rows to be written.
    let mut pending_deletes: Vec<u32> = Vec::new();
    let mut fetched_count = 0usize;
    let mut more = false;
    let now = now_ms();
    let delete_after_ms = i64::from(settings.pop3_delete_after_days) * DAY_MS;

    for (number, uidl) in &entries {
        let number = *number;
        let size = sizes.get(&number).copied().unwrap_or(0);
        if let Some(known) = uidl.as_ref().and_then(|id| seen.get(id)) {
            // Already stored. Remove the server copy when the settings say so
            // (also finishes a delete a crash interrupted). Messages too large
            // to download are never deleted unseen.
            if !known.skipped {
                let expired = leave
                    && delete_after_ms > 0
                    && now.saturating_sub(known.downloaded_at) >= delete_after_ms;
                if !leave || expired {
                    to_delete.push(number);
                    deleted_ids.extend(uidl.clone());
                }
            }
            continue;
        }
        if fetched_count >= CYCLE_MESSAGES {
            more = true;
            continue;
        }
        fetched_count += 1;
        let too_big =
            (max_bytes > 0 && size > max_bytes) || size > client::MAX_MESSAGE_BYTES as u64;
        let (raw, skipped) = if too_big {
            let head = client.top_headers(number).await?.unwrap_or_default();
            let note = format!(
                "[This message is large ({} KiB) and was not downloaded. It is still on the mail server.]",
                size.div_ceil(1024)
            );
            (crate::nntp::placeholder_with_note(&head, &note), true)
        } else {
            (client.retr(number).await?, false)
        };
        let Ok(parsed) = crate::mime::parse_message(&raw) else {
            tracing::warn!(account_id, number, "pop3: skipping an unparseable message");
            // Remember it so it is not retried every cycle; it stays on the server.
            if let Some(id) = uidl {
                record_unparseable(ctx, account_id, id).await?;
            }
            continue;
        };
        pending_bytes += raw.len();
        if !skipped && !leave {
            pending_deletes.push(number);
        }
        pending.push(Fetched {
            uidl: uidl.clone(),
            raw,
            parsed,
            skipped,
        });
        if pending.len() >= FLUSH_MESSAGES || pending_bytes >= FLUSH_BYTES {
            changed.extend(store(ctx, config, inbox_id, std::mem::take(&mut pending)).await?);
            pending_bytes = 0;
            to_delete.append(&mut pending_deletes);
        }
    }
    changed.extend(store(ctx, config, inbox_id, pending).await?);
    to_delete.append(&mut pending_deletes);

    let mut delete_failed = false;
    for number in &to_delete {
        if client.dele(*number).await.is_err() {
            delete_failed = true;
            break;
        }
    }
    let quit = if to_delete.is_empty() {
        client.quit_quietly().await;
        Ok(())
    } else {
        client.quit().await
    };
    if !changed.is_empty() {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: changed.iter().copied().collect(),
        });
    }
    if quit.is_ok() && !delete_failed {
        // Forget ids the server no longer lists (deleted now or elsewhere).
        let live: HashSet<String> = entries
            .iter()
            .filter_map(|(_, uidl)| uidl.clone())
            .filter(|id| !deleted_ids.contains(id))
            .collect();
        prune_seen(ctx, account_id, live).await?;
    }
    if delete_failed {
        return Err(CoreError::Network(
            "pop3: could not delete a message on the server; it will be retried".into(),
        ));
    }
    quit?;
    Ok(more)
}

async fn record_unparseable(ctx: &SyncCtx, account_id: i64, uidl: &str) -> Result<()> {
    let uidl = uidl.to_owned();
    ctx.db
        .write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO pop3_seen(account_id, uidl, message_id, skipped, downloaded_at)
                 VALUES (?1,?2,NULL,1,?3)",
                params![account_id, uidl, now_ms()],
            )?;
            Ok(())
        })
        .await
}

/// Drop `pop3_seen` rows for messages the server no longer lists.
async fn prune_seen(ctx: &SyncCtx, account_id: i64, live: HashSet<String>) -> Result<()> {
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let stored: Vec<String> = {
                let mut stmt = tx.prepare("SELECT uidl FROM pop3_seen WHERE account_id=?1")?;
                stmt.query_map(params![account_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for uidl in stored {
                if !live.contains(&uidl) {
                    tx.execute(
                        "DELETE FROM pop3_seen WHERE account_id=?1 AND uidl=?2",
                        params![account_id, uidl],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// Create the Inbox and local Sent, Drafts and Trash folders. Returns the Inbox id.
async fn sync_folders(ctx: &SyncCtx, config: &AccountConfig) -> Result<i64> {
    let account_id = config.id;
    let settings = config.settings.connection.clone();
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let none = repo::folders::FolderPermissions {
                can_create_children: false,
                can_rename: false,
                can_delete: false,
            };
            let mut inbox = None;
            for (role, default_name) in [
                (roles::INBOX, "INBOX"),
                (roles::SENT, "Sent"),
                (roles::DRAFTS, "Drafts"),
                (roles::TRASH, "Trash"),
            ] {
                let id = if let Some(existing) = repo::folders::by_role(&tx, account_id, role)? {
                    existing.id
                } else {
                    let name = settings.folder_override(role).unwrap_or(default_name);
                    repo::folders::upsert_discovered(
                        &tx,
                        account_id,
                        name,
                        None,
                        Some(role),
                        true,
                        none,
                    )?
                };
                if role == roles::INBOX {
                    inbox = Some(id);
                }
            }
            tx.commit()?;
            inbox.ok_or_else(|| CoreError::NotFound("inbox".into()))
        })
        .await
}

/// Write a batch of messages and remember their UIDLs.
async fn store(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder_id: i64,
    batch: Vec<Fetched>,
) -> Result<HashSet<i64>> {
    if batch.is_empty() {
        return Ok(HashSet::new());
    }
    let account_id = config.id;
    let cache_dir = ctx.paths.mail_dir(account_id);
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            std::fs::create_dir_all(&cache_dir)?;
            let mut threads = HashSet::new();
            for item in &batch {
                let (message_id, thread_id) = insert_parsed(
                    &tx,
                    &ParsedMessage {
                        account_id,
                        folder_id,
                        role: Some(roles::INBOX),
                        cache_dir: &cache_dir,
                        parsed: &item.parsed,
                        raw: &item.raw,
                        flags: FlagSet::default(),
                        fallback_ms: now_ms(),
                    },
                )?;
                if let Some(uidl) = &item.uidl {
                    tx.execute(
                        "INSERT OR REPLACE INTO pop3_seen(account_id, uidl, message_id, skipped, downloaded_at)
                         VALUES (?1,?2,?3,?4,?5)",
                        params![account_id, uidl, message_id, i64::from(item.skipped), now_ms()],
                    )?;
                }
                threads.insert(thread_id);
            }
            for thread_id in &threads {
                repo::threads::recompute(&tx, *thread_id)?;
            }
            tx.commit()?;
            Ok(threads)
        })
        .await
}

async fn run_due_actions(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
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
        match execute(ctx, config, &action).await {
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
                    tracing::warn!(account_id, action_id, kind = %action.kind, error = %message, "pop3 action retry scheduled");
                }
            }
        }
    }
    Ok(())
}

/// POP3 keeps no per-user state, so only cache clean-up needs work.
async fn execute(
    ctx: &SyncCtx,
    config: &AccountConfig,
    action: &repo::actions::PendingAction,
) -> Result<()> {
    match action.kind.as_str() {
        "delete_permanently" => crate::sync::finish_permanent_delete(ctx, action.message_id).await,
        "empty_trash" => {
            let snapshot = crate::sync::snapshot_trash_ids(ctx, config.id).await?;
            crate::sync::finish_empty_trash(ctx, config.id, snapshot).await
        }
        // Read state, stars, moves, labels and snoozes live in the app database.
        _ => Ok(()),
    }
}

/// Every message is cached whole (or as a deliberate header-only note), so
/// there is nothing to fetch for a body request.
async fn body_actor(
    ctx: SyncCtx,
    _config: AccountConfig,
    mut rx: mpsc::Receiver<PriorityFetchCmd>,
) {
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
