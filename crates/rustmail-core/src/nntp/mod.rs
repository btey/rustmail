//! Usenet news (NNTP) accounts.
//!
//! Each subscribed newsgroup is a folder. Sync imports new articles
//! incrementally (tracking the last article number per group), the app's own
//! database keeps read state, and posting goes through `POST` instead of SMTP.
//! The server holds no per-user state, so actions such as archive or delete
//! only change the local cache.

pub mod client;

use crate::accounts::secrets::{self, Slot};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::local::scan::{FlagSet, ParsedMessage, insert_parsed};
use crate::models::{AccountConfig, MailProtocol, now_ms, roles};
use crate::sync::engine::{PriorityFetchCmd, SyncCmd, SyncCtx, set_state, set_state_error};
use client::{Login, NntpClient, Overview, parse_group_list};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Articles imported the first time a group is read (the newest ones).
const INITIAL_ARTICLES: u64 = 300;
/// Articles examined per group per sync cycle; the rest follows next cycle.
const CYCLE_ARTICLES: u64 = 1000;
/// Articles larger than this are imported header-only and fetched when opened.
const PARTIAL_BYTES: u64 = 1024 * 1024;
const FLUSH_ARTICLES: usize = 25;
const FLUSH_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACTION_ATTEMPTS: i64 = 8;
/// Error messages starting with this are permanent: retrying cannot help.
pub(crate) const PERMANENT_PREFIX: &str = "cannot post: ";

pub(crate) fn permanent(message: impl std::fmt::Display) -> CoreError {
    CoreError::Other(format!("{PERMANENT_PREFIX}{message}"))
}

pub(crate) fn spawn(
    ctx: SyncCtx,
    config: AccountConfig,
    rx: mpsc::Receiver<SyncCmd>,
    body_rx: mpsc::Receiver<PriorityFetchCmd>,
    settings_rx: watch::Receiver<crate::models::AccountSettings>,
) -> Vec<tokio::task::JoinHandle<()>> {
    debug_assert_eq!(config.mail_protocol, MailProtocol::News);
    vec![
        tokio::spawn(actor(ctx.clone(), config.clone(), rx, settings_rx)),
        tokio::spawn(body_actor(ctx, config, body_rx)),
    ]
}

/// Open an authenticated session to the account's news server.
pub(crate) async fn open(ctx: &SyncCtx, config: &AccountConfig) -> Result<NntpClient> {
    let hydrated = secrets::hydrate_account(&ctx.credentials, config).await;
    let config = &hydrated;
    let login = if config.username.trim().is_empty() {
        None
    } else {
        let password =
            secrets::load_async(ctx.credentials.clone(), config.id, Slot::Password).await?;
        Some(Login {
            user: config.username.trim().to_owned(),
            password,
        })
    };
    NntpClient::connect(
        &config.imap_host,
        config.imap_port,
        &config.settings.connection,
        login,
    )
    .await
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
                tracing::warn!(account_id = config.id, %message, "news sync failed");
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

/// A group's folder and where it stands.
#[derive(Debug, Clone)]
struct GroupFolder {
    id: i64,
    name: String,
}

/// One sync cycle. Returns whether some group still has articles to fetch.
async fn cycle(ctx: &SyncCtx, config: &AccountConfig) -> Result<bool> {
    let (groups, invalid) = parse_group_list(&config.settings.connection.news_groups);
    if !invalid.is_empty() {
        return Err(CoreError::Other(format!(
            "invalid newsgroup name: {}",
            invalid.join(", ")
        )));
    }
    let folders = sync_folders(ctx, config, &groups).await?;
    run_due_actions(ctx, config).await?;
    if folders.is_empty() {
        return Ok(false);
    }
    let mut client = open(ctx, config).await?;
    let mut changed = HashSet::new();
    let mut problems = Vec::new();
    let mut more = false;
    for folder in &folders {
        match sync_group(ctx, config, &mut client, folder).await {
            Ok(outcome) => {
                changed.extend(outcome.threads);
                more |= outcome.more;
            }
            // A session-level failure ends the cycle; the error is retried.
            Err(error @ (CoreError::Auth(_) | CoreError::Network(_) | CoreError::Tls(_))) => {
                emit_changed(ctx, &changed);
                return Err(error);
            }
            Err(error) => problems.push(format!("{}: {error}", folder.name)),
        }
    }
    client.quit().await;
    emit_changed(ctx, &changed);
    if problems.is_empty() {
        Ok(more)
    } else {
        Err(CoreError::Other(problems.join("; ")))
    }
}

fn emit_changed(ctx: &SyncCtx, changed: &HashSet<i64>) {
    if !changed.is_empty() {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: changed.iter().copied().collect(),
        });
    }
}

/// Run a one-off sync (used by tests).
pub async fn sync_once(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
    cycle(ctx, config).await.map(|_| ())
}

/// Reconcile folder rows with the subscription list: one folder per group plus
/// local Sent, Drafts and Trash folders so posting and deleting have somewhere
/// to go.
async fn sync_folders(
    ctx: &SyncCtx,
    config: &AccountConfig,
    groups: &[String],
) -> Result<Vec<GroupFolder>> {
    let account_id = config.id;
    let settings = config.settings.connection.clone();
    let groups = groups.to_vec();
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let none = repo::folders::FolderPermissions {
                can_create_children: false,
                can_rename: false,
                can_delete: false,
            };
            let mut folders = Vec::new();
            let mut wanted: HashSet<String> = HashSet::new();
            for name in &groups {
                let id = repo::folders::upsert_discovered(
                    &tx, account_id, name, None, None, true, none,
                )?;
                tx.execute(
                    "INSERT OR IGNORE INTO nntp_groups(folder_id) VALUES (?1)",
                    params![id],
                )?;
                wanted.insert(name.clone());
                folders.push(GroupFolder {
                    id,
                    name: name.clone(),
                });
            }
            for (role, default_name) in [
                (roles::SENT, "Sent"),
                (roles::DRAFTS, "Drafts"),
                (roles::TRASH, "Trash"),
            ] {
                if let Some(existing) = repo::folders::by_role(&tx, account_id, role)? {
                    wanted.insert(existing.imap_name);
                    continue;
                }
                let name = settings.folder_override(role).unwrap_or(default_name);
                repo::folders::upsert_discovered(
                    &tx,
                    account_id,
                    name,
                    None,
                    Some(role),
                    true,
                    none,
                )?;
                wanted.insert(name.to_owned());
            }
            // Unsubscribed groups: drop their cached articles and the folder.
            let existing: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, imap_name FROM folders
                     WHERE account_id=?1 AND jmap_id IS NULL AND role IS NULL",
                )?;
                stmt.query_map(params![account_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (id, name) in existing {
                if wanted.contains(&name) {
                    continue;
                }
                let ids: Vec<(i64, Option<i64>)> = {
                    let mut stmt = tx.prepare(
                        "SELECT id, thread_id FROM messages WHERE account_id=?1 AND folder_id=?2",
                    )?;
                    stmt.query_map(params![account_id, id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                let mut threads = HashSet::new();
                for (message_id, thread_id) in ids {
                    repo::messages::delete(&tx, message_id)?;
                    threads.extend(thread_id);
                }
                for thread_id in threads {
                    repo::threads::recompute(&tx, thread_id)?;
                }
                tx.execute("DELETE FROM folders WHERE id=?1", params![id])?;
            }
            tx.commit()?;
            Ok(folders)
        })
        .await
}

#[derive(Default)]
struct GroupOutcome {
    threads: HashSet<i64>,
    more: bool,
}

/// An article waiting to be written.
struct Fetched {
    number: u64,
    raw: Vec<u8>,
    parsed: crate::mime::ParsedBody,
    partial: bool,
}

async fn sync_group(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &mut NntpClient,
    folder: &GroupFolder,
) -> Result<GroupOutcome> {
    let Some(stats) = client.group(&folder.name).await? else {
        return Err(CoreError::NotFound(format!(
            "newsgroup {} is not carried by this server",
            folder.name
        )));
    };
    let folder_id = folder.id;
    let mut last_seen: u64 = ctx
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT last_seen FROM nntp_groups WHERE folder_id=?1",
                params![folder_id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        })
        .await?;
    // The server renumbered the group: start over (duplicates are skipped).
    if stats.high < last_seen {
        last_seen = 0;
    }
    let posting = client.posting_allowed();
    let (low, high) = (stats.low, stats.high);
    ctx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE nntp_groups SET low=?2, high=?3, posting_allowed=?4 WHERE folder_id=?1",
                params![folder_id, low as i64, high as i64, i64::from(posting)],
            )?;
            Ok(())
        })
        .await?;

    let mut outcome = GroupOutcome::default();
    if stats.count == 0 || high == 0 {
        return Ok(outcome);
    }
    let first = if last_seen == 0 {
        stats
            .low
            .max(high.saturating_sub(INITIAL_ARTICLES - 1))
            .max(1)
    } else {
        stats.low.max(last_seen + 1)
    };
    if first <= high {
        let last = high.min(first + CYCLE_ARTICLES - 1);
        outcome.more = last < high;
        let entries: Vec<(u64, Option<u64>)> = match client.overview(first, last).await? {
            Some(list) => dedupe_overview(list),
            // No OVER/XOVER: try every number in the window.
            None => (first..=last).map(|n| (n, None)).collect(),
        };
        let mut pending: Vec<Fetched> = Vec::new();
        let mut pending_bytes = 0usize;
        for (number, bytes) in entries {
            let partial = bytes.is_some_and(|size| size > PARTIAL_BYTES);
            let fetched = if partial {
                client
                    .head(number)
                    .await?
                    .map(|head| placeholder_article(&head, bytes.unwrap_or(0)))
            } else {
                client.article(number).await?
            };
            let Some(raw) = fetched else { continue };
            let Ok(parsed) = crate::mime::parse_message(&raw) else {
                tracing::debug!(number, group = %folder.name, "skipping unparseable article");
                continue;
            };
            pending_bytes += raw.len();
            pending.push(Fetched {
                number,
                raw,
                parsed,
                partial,
            });
            if pending.len() >= FLUSH_ARTICLES || pending_bytes >= FLUSH_BYTES {
                let upto = pending.last().map_or(last_seen, |item| item.number);
                outcome.threads.extend(
                    store(ctx, config, folder.id, std::mem::take(&mut pending), upto).await?,
                );
                pending_bytes = 0;
            }
        }
        outcome
            .threads
            .extend(store(ctx, config, folder.id, pending, last).await?);
    }
    outcome
        .threads
        .extend(expire(ctx, folder.id, stats.low).await?);
    Ok(outcome)
}

/// Overview data in article order, one entry per number.
fn dedupe_overview(list: Vec<Overview>) -> Vec<(u64, Option<u64>)> {
    let mut seen = HashMap::new();
    for item in list {
        seen.entry(item.number).or_insert(item.bytes);
    }
    let mut entries: Vec<_> = seen.into_iter().collect();
    entries.sort_unstable_by_key(|(number, _)| *number);
    entries
}

/// A header-only stand-in for an article too large to download during sync.
/// MIME headers are dropped so the note is shown as plain text.
fn placeholder_article(head: &[u8], bytes: u64) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut out = String::with_capacity(text.len() + 160);
    let mut skipping = false;
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if !skipping {
                out.push_str(line);
                out.push_str("\r\n");
            }
            continue;
        }
        let name = line.split(':').next().unwrap_or("").trim();
        skipping = name.eq_ignore_ascii_case("mime-version")
            || name.len() >= 8 && name[..8].eq_ignore_ascii_case("content-");
        if !skipping {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n");
    out.push_str(&format!(
        "[This article is large ({} KiB). Open it to download the full text.]\r\n",
        bytes.div_ceil(1024)
    ));
    out.into_bytes()
}

/// Write a batch and advance the group's `last_seen` to `upto`.
async fn store(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder_id: i64,
    batch: Vec<Fetched>,
    upto: u64,
) -> Result<HashSet<i64>> {
    let account_id = config.id;
    let cache_dir = ctx.paths.mail_dir(account_id);
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            std::fs::create_dir_all(&cache_dir)?;
            let mut threads = HashSet::new();
            for article in &batch {
                let known: bool = match &article.parsed.headers.message_id {
                    Some(id) => tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM nntp_articles a
                           JOIN messages m ON m.id = a.message_id
                          WHERE a.folder_id=?1 AND m.message_id=?2)",
                        params![folder_id, id],
                        |row| row.get(0),
                    )?,
                    None => false,
                };
                if known {
                    continue;
                }
                let (message_id, thread_id) = insert_parsed(
                    &tx,
                    &ParsedMessage {
                        account_id,
                        folder_id,
                        role: None,
                        cache_dir: &cache_dir,
                        parsed: &article.parsed,
                        raw: &article.raw,
                        flags: FlagSet::default(),
                        fallback_ms: now_ms(),
                    },
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO nntp_articles(message_id, folder_id, article_no, partial)
                     VALUES (?1,?2,?3,?4)",
                    params![
                        message_id,
                        folder_id,
                        article.number as i64,
                        i64::from(article.partial)
                    ],
                )?;
                if article.partial {
                    // Only headers so far: opening the message downloads the rest.
                    tx.execute(
                        "UPDATE messages SET body_state='none' WHERE id=?1",
                        params![message_id],
                    )?;
                }
                threads.insert(thread_id);
            }
            for thread_id in &threads {
                repo::threads::recompute(&tx, *thread_id)?;
            }
            tx.execute(
                "UPDATE nntp_groups SET last_seen = MAX(last_seen, ?2) WHERE folder_id=?1",
                params![folder_id, upto as i64],
            )?;
            tx.commit()?;
            Ok(threads)
        })
        .await
}

/// Drop cached articles the server has expired (below the group's low-water
/// mark). Starred articles are kept.
async fn expire(ctx: &SyncCtx, folder_id: i64, low: u64) -> Result<HashSet<i64>> {
    if low <= 1 {
        return Ok(HashSet::new());
    }
    let threads = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let rows: Vec<(i64, Option<i64>)> = {
                let mut stmt = tx.prepare(
                    "SELECT m.id, m.thread_id FROM nntp_articles a
                       JOIN messages m ON m.id = a.message_id
                      WHERE a.folder_id=?1 AND a.article_no < ?2 AND m.is_starred = 0",
                )?;
                stmt.query_map(params![folder_id, low as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<_>>()?
            };
            let mut threads = HashSet::new();
            for (id, thread_id) in rows {
                repo::messages::delete(&tx, id)?;
                threads.extend(thread_id);
            }
            for thread_id in &threads {
                repo::threads::recompute(&tx, *thread_id)?;
            }
            tx.commit()?;
            Ok(threads)
        })
        .await?;
    Ok(threads)
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
                    tracing::warn!(account_id, action_id, kind = %action.kind, error = %message, "news action retry scheduled");
                }
            }
        }
    }
    Ok(())
}

/// The news server keeps no per-user state, so only cache clean-up needs work.
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

async fn body_actor(ctx: SyncCtx, config: AccountConfig, mut rx: mpsc::Receiver<PriorityFetchCmd>) {
    let mut client: Option<NntpClient> = None;
    while let Some(command) = rx.recv().await {
        match command {
            PriorityFetchCmd::Body(message_id) => {
                // Most articles are cached whole when imported; only large ones
                // need a trip to the server.
                let mut fetched = false;
                for attempt in 0..2 {
                    match fetch_full(&ctx, &config, &mut client, message_id).await {
                        Ok(()) => {
                            fetched = true;
                            break;
                        }
                        Err(error) => {
                            tracing::warn!(account_id = config.id, attempt, %error, "news: could not fetch article");
                            client = None;
                        }
                    }
                }
                if !fetched {
                    // Let the next open try again.
                    let _ = ctx
                        .db
                        .write(move |conn| repo::messages::cancel_body_fetch(conn, message_id))
                        .await;
                }
            }
            PriorityFetchCmd::Attachment {
                attachment_id,
                complete,
            } => {
                let result = attachment_bytes(&ctx, attachment_id)
                    .await
                    .map_err(|error| error.to_string());
                let _ = complete.send(result);
            }
        }
    }
}

/// Download the whole article for a header-only message.
async fn fetch_full(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &mut Option<NntpClient>,
    message_id: i64,
) -> Result<()> {
    let job = ctx
        .db
        .read(move |conn| {
            use rusqlite::OptionalExtension;
            Ok(conn
                .query_row(
                    "SELECT a.article_no, f.imap_name, m.raw_path FROM nntp_articles a
                       JOIN folders f ON f.id = a.folder_id
                       JOIN messages m ON m.id = a.message_id
                      WHERE a.message_id=?1 AND a.partial=1",
                    params![message_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?)
        })
        .await?;
    let Some((number, group, raw_path)) = job else {
        return Ok(());
    };
    if client.is_none() {
        *client = Some(open(ctx, config).await?);
    }
    let session = client
        .as_mut()
        .ok_or_else(|| CoreError::Other("news session lost".into()))?;
    if session.group(&group).await?.is_none() {
        return Err(CoreError::NotFound(format!("newsgroup {group}")));
    }
    let Some(raw) = session.article(number as u64).await? else {
        return Err(CoreError::NotFound("the article has expired".into()));
    };
    let parsed = crate::mime::parse_message(&raw)?;
    let has_attachments = parsed.attachments.iter().any(|a| !a.is_inline);
    let path = match raw_path {
        Some(path) => path,
        None => ctx
            .paths
            .mail_dir(config.id)
            .join(format!("{message_id}.local.eml"))
            .to_string_lossy()
            .into_owned(),
    };
    tokio::fs::write(&path, &raw).await?;
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            repo::messages::store_body(
                &tx,
                message_id,
                parsed.text.as_deref(),
                parsed.html.as_deref(),
                Some(&path),
                has_attachments,
                Some(&parsed.snippet),
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
            tx.execute(
                "UPDATE messages SET size=?2 WHERE id=?1",
                params![message_id, raw.len() as i64],
            )?;
            tx.execute(
                "UPDATE nntp_articles SET partial=0 WHERE message_id=?1",
                params![message_id],
            )?;
            repo::search::index_message(&tx, message_id)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    if let Some(thread_id) = ctx
        .db
        .read(move |conn| Ok(repo::messages::get_row(conn, message_id)?.and_then(|r| r.thread_id)))
        .await?
    {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: vec![thread_id],
        });
    }
    Ok(())
}

async fn attachment_bytes(ctx: &SyncCtx, attachment_id: i64) -> Result<Vec<u8>> {
    let (part_id, raw_path) = ctx
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT a.part_id, m.raw_path FROM attachments a
                   JOIN messages m ON m.id=a.message_id WHERE a.id=?1",
                params![attachment_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .map_err(Into::into)
        })
        .await?;
    let part_id = part_id.ok_or_else(|| CoreError::NotFound("attachment MIME part".into()))?;
    let path = raw_path.ok_or_else(|| CoreError::NotFound("cached article".into()))?;
    let raw = tokio::fs::read(path).await?;
    crate::mime::extract_attachment(&raw, &part_id).map(|(bytes, _)| bytes)
}

// ---- posting ------------------------------------------------------------

/// Newsgroups a message is addressed to: recipients written as
/// `group.name@news`, otherwise the group(s) of the article being answered
/// (its `Followup-To`, falling back to `Newsgroups`).
pub(crate) async fn resolve_groups(
    ctx: &SyncCtx,
    draft_id: i64,
    recipients: &[crate::models::Address],
) -> Result<Vec<String>> {
    let mut groups: Vec<String> = Vec::new();
    for address in recipients {
        if let Some((local, domain)) = address.email.rsplit_once('@')
            && domain.eq_ignore_ascii_case("news")
            && client::valid_group_name(local)
            && !groups.iter().any(|g| g.eq_ignore_ascii_case(local))
        {
            groups.push(local.to_owned());
        }
    }
    if !groups.is_empty() {
        return Ok(groups);
    }
    let parent_raw = ctx
        .db
        .read(move |conn| {
            let parent: Option<i64> = conn
                .query_row(
                    "SELECT in_reply_to_message_id FROM drafts_meta WHERE message_id=?1",
                    params![draft_id],
                    |row| row.get(0),
                )
                .unwrap_or(None);
            Ok(match parent {
                Some(id) => repo::messages::get_row(conn, id)?.and_then(|row| row.raw_path),
                None => None,
            })
        })
        .await?;
    let Some(path) = parent_raw else {
        return Ok(groups);
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return Ok(groups);
    };
    let head = &bytes[..bytes.len().min(256 * 1024)];
    let mut followup = None;
    let mut newsgroups = None;
    for (name, value) in unfolded_headers(head) {
        match name.to_ascii_lowercase().as_str() {
            "followup-to" if followup.is_none() => followup = Some(value),
            "newsgroups" if newsgroups.is_none() => newsgroups = Some(value),
            _ => {}
        }
    }
    // `Followup-To: poster` asks for a private reply, not a posting.
    let chosen = followup
        .filter(|value| !value.trim().eq_ignore_ascii_case("poster"))
        .or(newsgroups);
    if let Some(value) = chosen {
        for token in value.split([',', ' ']) {
            let token = token.trim();
            if client::valid_group_name(token)
                && !groups.iter().any(|g| g.eq_ignore_ascii_case(token))
            {
                groups.push(token.to_owned());
            }
        }
    }
    Ok(groups)
}

/// Header `(name, value)` pairs of a raw message, with folded lines joined.
fn unfolded_headers(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    headers
}

/// Post `raw` (a complete message) to `groups` on the account's server.
pub(crate) async fn post_article(
    ctx: &SyncCtx,
    config: &AccountConfig,
    groups: &[String],
    raw: Vec<u8>,
) -> Result<Vec<u8>> {
    // Outgoing headers (Organization, custom headers) are already in `raw`.
    let raw = crate::mime::insert_headers(raw, &[("Newsgroups".to_owned(), groups.join(","))]);
    let mut session = open(ctx, config).await?;
    if !session.posting_allowed() {
        return Err(permanent("this news server does not allow posting"));
    }
    session.post(&raw).await?;
    session.quit().await;
    Ok(raw)
}
