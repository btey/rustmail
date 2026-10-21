//! Local mail accounts: Maildir and MH (read/write) and mbox (read only) folders on
//! this computer, with optional SMTP for sending.
//!
//! The mail files stay the source of truth. Sync mirrors them into the same
//! database tables the network backends use; user actions are written back to
//! the files (Maildir only) by a durable queue, exactly like IMAP/JMAP.

pub mod maildir;
pub mod mbox;
pub mod mh;
pub(crate) mod scan;

use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::models::{AccountConfig, LocalFormat, MailProtocol, now_ms, roles};
use crate::sync::engine::{PriorityFetchCmd, SyncCmd, SyncCtx, set_state, set_state_error};
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Local files change without any notification, so poll often; an unchanged
/// folder costs two `stat` calls.
const POLL_INTERVAL: Duration = Duration::from_secs(20);
const MAX_ACTION_ATTEMPTS: i64 = 8;

pub(crate) fn spawn(
    ctx: SyncCtx,
    config: AccountConfig,
    rx: mpsc::Receiver<SyncCmd>,
    body_rx: mpsc::Receiver<PriorityFetchCmd>,
    settings_rx: watch::Receiver<crate::models::AccountSettings>,
) -> Vec<tokio::task::JoinHandle<()>> {
    debug_assert_eq!(config.mail_protocol, MailProtocol::Local);
    vec![
        tokio::spawn(actor(ctx.clone(), config.clone(), rx, settings_rx)),
        tokio::spawn(body_actor(ctx, config, body_rx)),
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
        match cycle(&ctx, &config).await {
            Ok(()) => {
                set_state(&ctx, config.id, "idle").await;
                finish_waiters(&mut waiters, Ok(()));
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(account_id = config.id, %message, "local mail sync failed");
                set_state_error(&ctx, config.id, "offline", &message).await;
                finish_waiters(&mut waiters, Err(message));
            }
        }
        tokio::select! {
            command = rx.recv() => match command {
                None | Some(SyncCmd::Shutdown) => return,
                Some(SyncCmd::SyncNow { complete: Some(done) }) => waiters.push(done),
                _ => {}
            },
            _ = settings.changed() => {},
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
        }
    }
}

async fn cycle(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
    let root = scan::root_of(config)?;
    // Apply queued user actions first so a rescan never undoes them.
    run_due_actions(ctx, config, &root).await?;
    let folders = scan::sync_folders(ctx, config, &root).await?;
    let mut changed = std::collections::HashSet::new();
    for folder in folders.iter().filter(|folder| folder.selectable) {
        changed.extend(scan::sync_one_folder(ctx, config, &root, folder).await?);
    }
    if !changed.is_empty() {
        ctx.bus.emit(CoreEvent::MailUpdated {
            thread_ids: changed.into_iter().collect(),
        });
    }
    Ok(())
}

/// Run a one-off scan (used by tests and by account creation).
pub async fn sync_once(ctx: &SyncCtx, config: &AccountConfig) -> Result<()> {
    cycle(ctx, config).await
}

async fn run_due_actions(ctx: &SyncCtx, config: &AccountConfig, root: &Path) -> Result<()> {
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
        match execute(ctx, config, root, &action).await {
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
                if attempts >= MAX_ACTION_ATTEMPTS {
                    let saved = message.clone();
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
                    let saved = message.clone();
                    ctx.db
                        .write(move |conn| {
                            repo::actions::bump_attempt(conn, action_id, retry_at, &saved)
                        })
                        .await?;
                    tracing::warn!(account_id, action_id, kind = %action.kind, error = %message, "local action retry scheduled");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FileRef {
    folder_id: i64,
    key: String,
}

fn file_ref(conn: &rusqlite::Connection, message_id: i64) -> Result<Option<FileRef>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT folder_id, unique_key FROM local_messages WHERE message_id=?1",
            params![message_id],
            |row| {
                Ok(FileRef {
                    folder_id: row.get(0)?,
                    key: row.get(1)?,
                })
            },
        )
        .optional()?)
}

fn folder_location(conn: &rusqlite::Connection, root: &Path, folder_id: i64) -> Result<PathBuf> {
    let rel: String = conn
        .query_row(
            "SELECT rel_path FROM local_folders WHERE folder_id=?1",
            params![folder_id],
            |row| row.get(0),
        )
        .map_err(|_| CoreError::NotFound("local folder".into()))?;
    let rel_path = Path::new(&rel);
    if !maildir::is_safe_relative(rel_path) {
        return Err(CoreError::Other("unsafe local folder path".into()));
    }
    Ok(root.join(rel_path))
}

fn io_error(error: std::io::Error) -> CoreError {
    CoreError::Other(format!("mail folder: {error}"))
}

fn read_only(config: &AccountConfig) -> bool {
    config.settings.connection.local_format == LocalFormat::Mbox
}

async fn execute(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    action: &repo::actions::PendingAction,
) -> Result<()> {
    // mbox files are never written; flag/label changes stay in the app only.
    if read_only(config) {
        return Ok(());
    }
    match action.kind.as_str() {
        "mark_read" | "mark_unread" | "star" | "unstar" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            set_flag(ctx, config, root, message_id, &action.kind).await
        }
        "archive" | "unarchive" | "trash" | "spam" | "not_spam" | "move" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            relocate(ctx, config, root, message_id).await
        }
        "delete_permanently" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            remove_file(ctx, config, root, message_id).await?;
            crate::sync::finish_permanent_delete(ctx, Some(message_id)).await
        }
        "empty_trash" => {
            let snapshot = crate::sync::snapshot_trash_ids(ctx, config.id).await?;
            for message_id in &snapshot {
                remove_file(ctx, config, root, *message_id).await?;
            }
            crate::sync::finish_empty_trash(ctx, config.id, snapshot).await
        }
        "append_sent" => {
            let Some(message_id) = action.message_id else {
                return Ok(());
            };
            file_sent_copy(ctx, config, root, message_id).await
        }
        // Labels, snoozes and similar are kept in the app's database.
        _ => Ok(()),
    }
}

fn is_mh(config: &AccountConfig) -> bool {
    config.settings.connection.local_format == LocalFormat::Mh
}

async fn entry_for(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    message_id: i64,
) -> Result<Option<(FileRef, PathBuf, maildir::Entry)>> {
    let root = root.to_path_buf();
    let found = ctx
        .db
        .read(move |conn| {
            let Some(reference) = file_ref(conn, message_id)? else {
                return Ok(None);
            };
            let dir = folder_location(conn, &root, reference.folder_id)?;
            Ok(Some((reference, dir)))
        })
        .await?;
    let Some((reference, dir)) = found else {
        return Ok(None);
    };
    let key = reference.key.clone();
    let lookup_dir = dir.clone();
    let mh_format = is_mh(config);
    let entry = tokio::task::spawn_blocking(move || {
        if mh_format {
            mh::locate(&lookup_dir, &key)
        } else {
            maildir::locate(&lookup_dir, &key)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?;
    Ok(entry.map(|entry| (reference, dir, entry)))
}

async fn set_flag(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    message_id: i64,
    kind: &str,
) -> Result<()> {
    let Some((_, _, entry)) = entry_for(ctx, config, root, message_id).await? else {
        return Ok(()); // the file vanished; the next scan reconciles
    };
    let mut flags = entry.flags;
    match kind {
        "mark_read" => flags.seen = true,
        "mark_unread" => flags.seen = false,
        "star" => flags.flagged = true,
        _ => flags.flagged = false,
    }
    let root_owned = root.to_path_buf();
    let rel_root = root.to_path_buf();
    let mh_format = is_mh(config);
    let target = tokio::task::spawn_blocking(move || {
        if mh_format {
            mh::set_flags(&root_owned, &entry, flags)
        } else {
            maildir::set_flags(&root_owned, &entry, flags)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?
    .map_err(io_error)?;
    let rel = target
        .strip_prefix(&rel_root)
        .unwrap_or(&target)
        .to_string_lossy()
        .into_owned();
    let encoded = scan::FlagSet::from(flags);
    ctx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE local_messages SET rel_path=?2, flags=?3 WHERE message_id=?1",
                params![message_id, rel, encoded.encode()],
            )?;
            Ok(())
        })
        .await
}

/// Move the file to the folder the message now belongs to in the app.
async fn relocate(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    message_id: i64,
) -> Result<()> {
    let Some((reference, _dir, entry)) = entry_for(ctx, config, root, message_id).await? else {
        return Ok(());
    };
    let root_owned = root.to_path_buf();
    let target = ctx
        .db
        .read(move |conn| {
            let folder_id: Option<i64> = conn.query_row(
                "SELECT folder_id FROM messages WHERE id=?1",
                params![message_id],
                |row| row.get(0),
            )?;
            let Some(folder_id) = folder_id else {
                return Ok(None);
            };
            if folder_id == reference.folder_id {
                return Ok(None);
            }
            let dir = folder_location(conn, &root_owned, folder_id)?;
            Ok(Some((folder_id, dir)))
        })
        .await?;
    let Some((target_folder, target_dir)) = target else {
        return Ok(());
    };
    let root_owned = root.to_path_buf();
    let flags = entry.flags;
    let mh_format = is_mh(config);
    let (key, path) = tokio::task::spawn_blocking(move || {
        if mh_format {
            mh::move_to(&root_owned, &entry, &target_dir, flags)
        } else {
            maildir::move_to(&root_owned, &entry, &target_dir, flags)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?
    .map_err(io_error)?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    ctx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE local_messages SET folder_id=?2, unique_key=?3, rel_path=?4 WHERE message_id=?1",
                params![message_id, target_folder, key, rel],
            )?;
            Ok(())
        })
        .await
}

async fn remove_file(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    message_id: i64,
) -> Result<()> {
    let Some((_, _, entry)) = entry_for(ctx, config, root, message_id).await? else {
        return Ok(());
    };
    let root_owned = root.to_path_buf();
    let mh_format = is_mh(config);
    tokio::task::spawn_blocking(move || {
        if mh_format {
            mh::remove(&root_owned, &entry)
        } else {
            maildir::remove(&root_owned, &entry)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?
    .map_err(io_error)
}

/// File the just-sent message in the Sent Maildir.
async fn file_sent_copy(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    message_id: i64,
) -> Result<()> {
    let account_id = config.id;
    let root_owned = root.to_path_buf();
    let job = ctx
        .db
        .read(move |conn| {
            if file_ref(conn, message_id)?.is_some() {
                return Ok(None);
            }
            let Some(row) = repo::messages::get_row(conn, message_id)? else {
                return Ok(None);
            };
            let Some(raw_path) = row.raw_path else {
                return Ok(None);
            };
            let Some(folder) = repo::folders::by_role(conn, account_id, roles::SENT)? else {
                return Ok(None);
            };
            let dir = folder_location(conn, &root_owned, folder.id)?;
            Ok(Some((raw_path, folder.id, dir)))
        })
        .await?;
    let Some((raw_path, folder_id, dir)) = job else {
        return Ok(());
    };
    let raw = tokio::fs::read(&raw_path).await?;
    let root_owned = root.to_path_buf();
    let size = raw.len() as i64;
    let flags = maildir::Flags {
        seen: true,
        ..Default::default()
    };
    let mh_format = is_mh(config);
    let (key, path) = tokio::task::spawn_blocking(move || {
        if mh_format {
            mh::deliver(&root_owned, &dir, &raw, flags)
        } else {
            maildir::deliver(&root_owned, &dir, &raw, flags)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?
    .map_err(io_error)?;
    let rel = path
        .strip_prefix(root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            scan::register_file(
                &tx,
                account_id,
                message_id,
                &scan::FileLocation {
                    folder_id,
                    key: &key,
                    rel_path: &rel,
                    size,
                    flags: scan::FlagSet::from(flags),
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

async fn body_actor(ctx: SyncCtx, config: AccountConfig, mut rx: mpsc::Receiver<PriorityFetchCmd>) {
    while let Some(command) = rx.recv().await {
        match command {
            // Bodies are cached when the message is imported.
            PriorityFetchCmd::Body(_) => {}
            PriorityFetchCmd::Attachment {
                attachment_id,
                complete,
            } => {
                let result = attachment_bytes(&ctx, &config, attachment_id)
                    .await
                    .map_err(|error| error.to_string());
                let _ = complete.send(result);
            }
        }
    }
}

async fn attachment_bytes(
    ctx: &SyncCtx,
    config: &AccountConfig,
    attachment_id: i64,
) -> Result<Vec<u8>> {
    let (message_id, part_id, raw_path) = ctx
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT a.message_id, a.part_id, m.raw_path FROM attachments a
                 JOIN messages m ON m.id=a.message_id WHERE a.id=?1",
                params![attachment_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(Into::into)
        })
        .await?;
    let part_id = part_id.ok_or_else(|| CoreError::NotFound("attachment MIME part".into()))?;
    let cached = match raw_path {
        Some(path) => tokio::fs::read(path).await.ok(),
        None => None,
    };
    let raw = match cached {
        Some(raw) => raw,
        None => source_bytes(ctx, config, message_id).await?,
    };
    crate::mime::extract_attachment(&raw, &part_id).map(|(bytes, _)| bytes)
}

/// The message exactly as stored in the user's mail folder.
pub async fn source_bytes(
    ctx: &SyncCtx,
    config: &AccountConfig,
    message_id: i64,
) -> Result<Vec<u8>> {
    let root = scan::root_of(config)?;
    let mbox_format = config.settings.connection.local_format == LocalFormat::Mbox;
    let mh_format = is_mh(config);
    let (reference, dir) = ctx
        .db
        .read({
            let root = root.clone();
            move |conn| {
                let reference = file_ref(conn, message_id)?
                    .ok_or_else(|| CoreError::NotFound("local message file".into()))?;
                let dir = folder_location(conn, &root, reference.folder_id)?;
                Ok((reference, dir))
            }
        })
        .await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        if mbox_format {
            let bytes = mbox::read_file(&dir).map_err(io_error)?;
            mbox::parse(&bytes)
                .into_iter()
                .find(|message| message.key == reference.key)
                .map(|message| message.raw)
                .ok_or_else(|| CoreError::NotFound("message in mbox".into()))
        } else {
            let entry = if mh_format {
                mh::locate(&dir, &reference.key)
            } else {
                maildir::locate(&dir, &reference.key)
            }
            .ok_or_else(|| CoreError::NotFound("message file".into()))?;
            std::fs::read(entry.path).map_err(io_error)
        }
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))?
}
