//! Folder discovery and message import for local accounts.

use super::{maildir, mbox, mh};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::imap::RemoteFolder;
use crate::mime::ParsedBody;
use crate::models::{AccountConfig, LocalFormat, MailConnectionSettings, now_ms, roles};
use crate::sync::engine::{SyncCtx, arrival_eligible, chime_worthy};
use rusqlite::{Transaction, params};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const BATCH_MESSAGES: usize = 100;
const BATCH_BYTES: usize = 32 * 1024 * 1024;
const MAX_MESSAGE_BYTES: u64 = mbox::MAX_MESSAGE_BYTES as u64;

/// Where a discovered folder lives on disk.
#[derive(Debug, Clone)]
pub(super) enum Source {
    Maildir(PathBuf),
    Mbox(PathBuf),
    Mh(PathBuf),
}

#[derive(Debug, Clone)]
pub(super) struct FolderState {
    pub id: i64,
    pub role: Option<String>,
    pub source: Source,
    pub selectable: bool,
}

pub(crate) fn root_of(config: &AccountConfig) -> Result<PathBuf> {
    let configured = config.settings.connection.local_path.trim();
    if configured.is_empty() {
        return Err(CoreError::Other(
            "this local account has no mail folder configured".into(),
        ));
    }
    std::fs::canonicalize(configured).map_err(|error| {
        CoreError::Other(format!(
            "mail folder {configured} is not available: {error}"
        ))
    })
}

fn remote_folder(name: &str) -> RemoteFolder {
    RemoteFolder {
        name: name.to_owned(),
        delimiter: Some("/".into()),
        attributes: Vec::new(),
    }
}

fn discover_sources(root: &Path, format: LocalFormat) -> std::io::Result<Vec<(String, Source)>> {
    Ok(match format {
        LocalFormat::Maildir => maildir::discover(root)?
            .into_iter()
            .map(|folder| (folder.name, Source::Maildir(folder.dir)))
            .collect(),
        LocalFormat::Mh => mh::discover(root)?
            .into_iter()
            .map(|folder| (folder.name, Source::Mh(folder.dir)))
            .collect(),
        LocalFormat::Mbox => mbox::discover(root)?
            .into_iter()
            .map(|folder| (folder.name, Source::Mbox(folder.path)))
            .collect(),
    })
}

/// Make sure Trash, Sent and Drafts exist so moves and sent copies have a
/// destination. Only Maildir accounts get folders created; mbox is read-only.
fn ensure_standard_maildirs(
    root: &Path,
    found: &[(String, Source)],
    settings: &MailConnectionSettings,
    format: LocalFormat,
) -> std::io::Result<bool> {
    let mut created = false;
    for (role, default_name) in [
        (roles::TRASH, "Trash"),
        (roles::SENT, "Sent"),
        (roles::DRAFTS, "Drafts"),
    ] {
        let present = found.iter().any(|(name, _)| {
            crate::sync::folder_map::resolve_role(&remote_folder(name), settings) == Some(role)
        });
        if present {
            continue;
        }
        let name = settings.folder_override(role).unwrap_or(default_name);
        if format == LocalFormat::Mh {
            if let Some(dir) = mh::dir_for_new_folder(root, name) {
                mh::create_folder(root, &dir)?;
                created = true;
            }
        } else if let Some(dir) = maildir::dir_for_new_folder(root, name) {
            maildir::ensure_maildir(&dir)?;
            created = true;
        }
    }
    Ok(created)
}

/// Discover folders on disk and reconcile the folder rows.
pub(super) async fn sync_folders(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
) -> Result<Vec<FolderState>> {
    let format = config.settings.connection.local_format;
    let settings = config.settings.connection.clone();
    let root_owned = root.to_path_buf();
    let found = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<(String, Source)>> {
        let mut found = discover_sources(&root_owned, format)?;
        if format != LocalFormat::Mbox
            && ensure_standard_maildirs(&root_owned, &found, &settings, format)?
        {
            found = discover_sources(&root_owned, format)?;
        }
        Ok(found)
    })
    .await
    .map_err(|error| CoreError::Other(error.to_string()))??;

    let account_id = config.id;
    let settings = config.settings.connection.clone();
    // A single mbox file is one folder; every other root holds a tree.
    let can_nest = format != LocalFormat::Mbox
        || std::fs::symlink_metadata(root)
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
    let root_for_rel = root.to_path_buf();
    ctx.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut states: Vec<FolderState> = Vec::new();
            let mut names: HashSet<String> = HashSet::new();
            let mut ids: HashMap<String, i64> = HashMap::new();
            for (name, source) in &found {
                let remote = remote_folder(name);
                let role = crate::sync::folder_map::resolve_role(&remote, &settings);
                let ordinary = role.is_none() && name != "INBOX";
                let permissions = repo::folders::FolderPermissions {
                    can_create_children: can_nest,
                    can_rename: ordinary && can_nest,
                    can_delete: ordinary && can_nest,
                };
                let id = repo::folders::upsert_discovered(
                    &tx,
                    account_id,
                    name,
                    Some("/"),
                    role,
                    true,
                    permissions,
                )?;
                let location = match source {
                    Source::Maildir(dir) | Source::Mh(dir) => dir,
                    Source::Mbox(path) => path,
                };
                tx.execute(
                    "INSERT INTO local_folders(folder_id, rel_path) VALUES(?1,?2)
                     ON CONFLICT(folder_id) DO UPDATE SET rel_path=excluded.rel_path",
                    params![id, relative(&root_for_rel, location)],
                )?;
                names.insert(name.clone());
                ids.insert(name.clone(), id);
                states.push(FolderState {
                    id,
                    role: role.map(str::to_owned),
                    source: source.clone(),
                    selectable: true,
                });
            }
            // Hierarchy containers that exist only as path prefixes.
            let mut ancestors = Vec::new();
            for (name, _) in &found {
                for ancestor in crate::sync::folder_map::ancestor_names(&remote_folder(name)) {
                    if names.insert(ancestor.clone()) {
                        ancestors.push(ancestor);
                    }
                }
            }
            for name in ancestors {
                let id = repo::folders::upsert_discovered(
                    &tx,
                    account_id,
                    &name,
                    Some("/"),
                    None,
                    false,
                    repo::folders::FolderPermissions {
                        can_create_children: can_nest,
                        can_rename: false,
                        can_delete: false,
                    },
                )?;
                ids.insert(name, id);
            }
            for (name, id) in &ids {
                let parent = name
                    .rsplit_once('/')
                    .and_then(|(parent, _)| ids.get(parent).copied());
                repo::folders::set_parent(&tx, *id, parent)?;
            }
            // Folders removed from disk: drop their cached messages first.
            let existing: Vec<(i64, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, imap_name FROM folders WHERE account_id=?1 AND jmap_id IS NULL",
                )?;
                stmt.query_map(params![account_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (id, name) in existing {
                if names.contains(&name) {
                    continue;
                }
                let mut threads = HashSet::new();
                let message_ids: Vec<(i64, Option<i64>)> = {
                    let mut stmt = tx.prepare(
                        "SELECT id, thread_id FROM messages WHERE account_id=?1 AND folder_id=?2",
                    )?;
                    stmt.query_map(params![account_id, id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (message_id, thread_id) in message_ids {
                    repo::messages::delete(&tx, message_id)?;
                    threads.extend(thread_id);
                }
                for thread_id in threads {
                    repo::threads::recompute(&tx, thread_id)?;
                }
                tx.execute("DELETE FROM folders WHERE id=?1", params![id])?;
            }
            tx.commit()?;
            Ok(states)
        })
        .await
}

#[derive(Debug, Clone)]
struct Known {
    message_id: i64,
    key: String,
    flags: String,
    rel_path: String,
}

/// A message read and parsed off the database thread.
struct Prepared {
    key: String,
    rel_path: String,
    offset: Option<u64>,
    flags: FlagSet,
    raw: Vec<u8>,
    parsed: ParsedBody,
    fallback_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FlagSet {
    pub seen: bool,
    pub flagged: bool,
    pub draft: bool,
    pub trashed: bool,
}

impl FlagSet {
    pub(crate) fn encode(self) -> String {
        format!(
            "{}{}{}{}",
            u8::from(self.seen),
            u8::from(self.flagged),
            u8::from(self.draft),
            u8::from(self.trashed)
        )
    }
}

impl From<maildir::Flags> for FlagSet {
    fn from(flags: maildir::Flags) -> Self {
        Self {
            seen: flags.seen,
            flagged: flags.flagged,
            draft: flags.draft,
            trashed: flags.trashed,
        }
    }
}

impl From<&mbox::Flags> for FlagSet {
    fn from(flags: &mbox::Flags) -> Self {
        Self {
            seen: flags.seen,
            flagged: flags.flagged,
            draft: flags.draft,
            trashed: flags.deleted,
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_else(now_ms)
}

/// Import or refresh one folder. Returns the threads that changed.
pub(super) async fn sync_one_folder(
    ctx: &SyncCtx,
    config: &AccountConfig,
    root: &Path,
    folder: &FolderState,
) -> Result<Vec<i64>> {
    let folder_id = folder.id;
    let stored: String = ctx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT fingerprint FROM local_folders WHERE folder_id=?1",
                    params![folder_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_default())
        })
        .await?;
    let current = match &folder.source {
        Source::Maildir(dir) => maildir::fingerprint(dir),
        Source::Mh(dir) => mh::fingerprint(dir),
        Source::Mbox(path) => mbox::fingerprint(path),
    };
    if !stored.is_empty() && stored == current {
        return Ok(Vec::new());
    }

    // The first scan of a folder is a catch-up import: it never notifies.
    let live = !stored.is_empty();
    let known: Vec<Known> = ctx
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT message_id, unique_key, flags, rel_path FROM local_messages
                 WHERE folder_id=?1",
            )?;
            Ok(stmt
                .query_map(params![folder_id], |row| {
                    Ok(Known {
                        message_id: row.get(0)?,
                        key: row.get(1)?,
                        flags: row.get(2)?,
                        rel_path: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    let mut by_key: HashMap<String, Known> = known
        .into_iter()
        .map(|known| (known.key.clone(), known))
        .collect();

    let mut changed_threads: HashSet<i64> = HashSet::new();
    let mut flag_updates: Vec<(i64, String, FlagSet)> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();

    match &folder.source {
        Source::Maildir(dir) | Source::Mh(dir) => {
            let dir_owned = dir.clone();
            let is_mh = matches!(folder.source, Source::Mh(_));
            let entries = tokio::task::spawn_blocking(move || {
                if is_mh {
                    mh::list(&dir_owned)
                } else {
                    maildir::list(&dir_owned)
                }
            })
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;
            let mut fresh = Vec::new();
            for entry in entries {
                let flags = FlagSet::from(entry.flags);
                // Flagged for deletion by another client: treat as removed.
                if flags.trashed {
                    continue;
                }
                seen_keys.insert(entry.key.clone());
                let rel = relative(root, &entry.path);
                match by_key.get(&entry.key) {
                    Some(known) => {
                        if known.flags != flags.encode() || known.rel_path != rel {
                            flag_updates.push((known.message_id, rel, flags));
                        }
                    }
                    None if entry.size > MAX_MESSAGE_BYTES => {
                        tracing::warn!(path = %entry.path.display(), "skipping oversized message");
                    }
                    None => fresh.push((entry.key.clone(), entry.path.clone(), rel, flags)),
                }
            }
            for chunk in chunk_by_budget(fresh, |item| {
                std::fs::metadata(&item.1)
                    .map(|meta| meta.len() as usize)
                    .unwrap_or(0)
            }) {
                let prepared = tokio::task::spawn_blocking(move || {
                    chunk
                        .into_iter()
                        .filter_map(|(key, path, rel, flags)| {
                            let raw = std::fs::read(&path).ok()?;
                            let parsed = crate::mime::parse_message(&raw).ok()?;
                            Some(Prepared {
                                key,
                                rel_path: rel,
                                offset: None,
                                flags,
                                fallback_ms: mtime_ms(&path),
                                raw,
                                parsed,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(|error| CoreError::Other(error.to_string()))?;
                changed_threads.extend(store_batch(ctx, config, folder, prepared, live).await?);
            }
        }
        Source::Mbox(path) => {
            let path_owned = path.clone();
            let parsed = tokio::task::spawn_blocking(move || {
                let bytes = mbox::read_file(&path_owned)?;
                Ok::<_, std::io::Error>(mbox::parse(&bytes))
            })
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;
            let rel = relative(root, path);
            let fallback = mtime_ms(path);
            let mut fresh = Vec::new();
            for message in parsed {
                seen_keys.insert(message.key.clone());
                if by_key.contains_key(&message.key) || message.flags.deleted {
                    continue;
                }
                fresh.push(message);
            }
            for chunk in chunk_by_budget(fresh, |message| message.raw.len()) {
                let rel = rel.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    chunk
                        .into_iter()
                        .filter_map(|message| {
                            let parsed = crate::mime::parse_message(&message.raw).ok()?;
                            Some(Prepared {
                                flags: FlagSet::from(&message.flags),
                                key: message.key,
                                rel_path: rel.clone(),
                                offset: Some(message.offset),
                                raw: message.raw,
                                parsed,
                                fallback_ms: fallback,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .map_err(|error| CoreError::Other(error.to_string()))?;
                changed_threads.extend(store_batch(ctx, config, folder, prepared, live).await?);
            }
        }
    }

    let is_mbox = matches!(folder.source, Source::Mbox(_));
    let gone: Vec<Known> = by_key
        .drain()
        .filter(|(key, _)| !seen_keys.contains(key))
        .map(|(_, known)| known)
        .collect();
    let account_id = config.id;
    let paths = ctx.paths.clone();
    let threads = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut threads: HashSet<i64> = HashSet::new();
            prune_kept_meta(&tx)?;
            if !is_mbox {
                for (message_id, rel, flags) in &flag_updates {
                    // A queued action owns the message's flags until it runs.
                    if repo::actions::has_pending_for_message(&tx, *message_id)? {
                        continue;
                    }
                    tx.execute(
                        "UPDATE local_messages SET flags=?2, rel_path=?3 WHERE message_id=?1",
                        params![message_id, flags.encode(), rel],
                    )?;
                    tx.execute(
                        "UPDATE messages SET is_read=?2, is_starred=?3, is_draft=CASE WHEN ?4 THEN 1 ELSE is_draft END
                         WHERE id=?1",
                        params![message_id, flags.seen, flags.flagged, flags.draft],
                    )?;
                    if let Some(thread_id) = tx
                        .query_row(
                            "SELECT thread_id FROM messages WHERE id=?1",
                            params![message_id],
                            |row| row.get::<_, Option<i64>>(0),
                        )
                        .ok()
                        .flatten()
                    {
                        threads.insert(thread_id);
                    }
                }
            }
            for known in &gone {
                let thread_id: Option<i64> = tx
                    .query_row(
                        "SELECT thread_id FROM messages WHERE id=?1",
                        params![known.message_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                let raw_path: Option<String> = tx
                    .query_row(
                        "SELECT raw_path FROM messages WHERE id=?1",
                        params![known.message_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                keep_meta(&tx, account_id, known.message_id)?;
                repo::messages::delete(&tx, known.message_id)?;
                if let Some(path) = raw_path {
                    let _ = std::fs::remove_file(path);
                }
                threads.extend(thread_id);
            }
            for thread_id in &threads {
                repo::threads::recompute(&tx, *thread_id)?;
            }
            tx.execute(
                "UPDATE local_folders SET fingerprint=?2 WHERE folder_id=?1",
                params![folder_id, current],
            )?;
            tx.commit()?;
            let _ = &paths;
            Ok(threads)
        })
        .await?;
    changed_threads.extend(threads);
    Ok(changed_threads.into_iter().collect())
}

fn chunk_by_budget<T>(items: Vec<T>, size: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for item in items {
        let weight = size(&item);
        if !current.is_empty() && (current.len() >= BATCH_MESSAGES || bytes + weight > BATCH_BYTES)
        {
            chunks.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += weight;
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

async fn store_batch(
    ctx: &SyncCtx,
    config: &AccountConfig,
    folder: &FolderState,
    batch: Vec<Prepared>,
    live: bool,
) -> Result<Vec<i64>> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let account_id = config.id;
    let folder_id = folder.id;
    let role = folder.role.clone();
    let cache_dir = ctx.paths.mail_dir(account_id);
    let notify_folder = live && role.as_deref() == Some(roles::INBOX);
    let (threads, fresh) = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            std::fs::create_dir_all(&cache_dir)?;
            let settings = repo::settings::get(&tx)?;
            let (outgoing_since, incoming_since) =
                repo::contacts::learning_boundaries(&tx, account_id, now_ms())?;
            let own_addresses = repo::sender_identities::own_addresses(&tx, account_id)?;
            let mut threads = HashSet::new();
            let mut enqueued: HashSet<i64> = HashSet::new();
            let mut fresh: Vec<i64> = Vec::new();
            for prepared in &batch {
                let (message_id, thread_id) = insert_prepared(
                    &tx,
                    account_id,
                    folder_id,
                    role.as_deref(),
                    &cache_dir,
                    prepared,
                )?;
                threads.insert(thread_id);
                let headers = &prepared.parsed.headers;
                let is_outgoing = role.as_deref() == Some(roles::SENT);
                let when = headers.date_ms.unwrap_or(prepared.fallback_ms);
                // Same rules as IMAP: outgoing recipients by default, incoming
                // senders only on opt-in and never from automated or junk mail.
                if is_outgoing {
                    if settings.collect_outgoing_contacts && when >= outgoing_since {
                        let mut seen = HashSet::new();
                        for address in headers
                            .to
                            .iter()
                            .chain(headers.cc.iter())
                            .chain(headers.bcc.iter())
                        {
                            let email = address.email.to_ascii_lowercase();
                            if !own_addresses.contains(&email) && seen.insert(email) {
                                repo::contacts::harvest(&tx, account_id, address, true, when)?;
                            }
                        }
                    }
                } else if settings.collect_incoming_contacts
                    && when >= incoming_since
                    && !headers.is_automated
                    && !matches!(role.as_deref(), Some(roles::SPAM) | Some(roles::TRASH))
                    && let Some(from) = &headers.from
                    && !crate::mime::robot_sender(&from.email)
                {
                    repo::contacts::harvest(&tx, account_id, from, false, when)?;
                }
                // `live` stands in for the IMAP UID baseline: files that
                // appear after the folder's first scan are new arrivals.
                if notify_folder
                    && arrival_eligible(
                        Some(0),
                        0,
                        prepared.flags.seen,
                        is_outgoing,
                        prepared.fallback_ms,
                        now_ms(),
                    )
                    && enqueued.insert(thread_id)
                {
                    repo::notifications::enqueue(&tx, message_id)?;
                    let from_email = headers.from.as_ref().map_or("", |from| from.email.as_str());
                    if chime_worthy(headers.is_automated, from_email) {
                        fresh.push(thread_id);
                    }
                }
            }
            for thread_id in &threads {
                repo::threads::recompute(&tx, *thread_id)?;
            }
            tx.commit()?;
            Ok((threads.into_iter().collect::<Vec<_>>(), fresh))
        })
        .await?;
    ctx.bus.emit(CoreEvent::MailUpdated {
        thread_ids: threads.clone(),
    });
    if !fresh.is_empty() {
        ctx.bus.emit(CoreEvent::MailNew {
            account_id,
            thread_ids: fresh,
        });
    }
    Ok(threads)
}

fn insert_prepared(
    tx: &Transaction,
    account_id: i64,
    folder_id: i64,
    role: Option<&str>,
    cache_dir: &Path,
    prepared: &Prepared,
) -> Result<(i64, i64)> {
    let (message_id, thread_id) = insert_parsed(
        tx,
        &ParsedMessage {
            account_id,
            folder_id,
            role,
            cache_dir,
            parsed: &prepared.parsed,
            raw: &prepared.raw,
            flags: prepared.flags,
            fallback_ms: prepared.fallback_ms,
        },
    )?;
    tx.execute(
        "INSERT INTO local_messages(message_id, account_id, folder_id, unique_key, rel_path,
                                    mbox_offset, mbox_len, flags)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            message_id,
            account_id,
            folder_id,
            prepared.key,
            prepared.rel_path,
            prepared.offset.map(|offset| offset as i64),
            prepared.raw.len() as i64,
            prepared.flags.encode(),
        ],
    )?;
    restore_meta(tx, account_id, message_id)?;
    Ok((message_id, thread_id))
}

/// Kept labels and snoozes are dropped after this long without the message
/// coming back.
const KEPT_META_TTL_MS: i64 = 30 * 24 * 3600 * 1000;

fn prune_kept_meta(tx: &Transaction) -> Result<()> {
    let cutoff = now_ms() - KEPT_META_TTL_MS;
    tx.execute(
        "DELETE FROM local_kept_labels WHERE kept_at < ?1",
        params![cutoff],
    )?;
    tx.execute(
        "DELETE FROM local_kept_snoozes WHERE kept_at < ?1",
        params![cutoff],
    )?;
    Ok(())
}

/// Remember the labels and snooze of a message about to be forgotten because
/// its file vanished, so they survive if the message reappears (moved by
/// another program). If a copy with the same Message-ID is already imported,
/// it receives them right away.
fn keep_meta(tx: &Transaction, account_id: i64, message_id: i64) -> Result<()> {
    let header: Option<String> = tx
        .query_row(
            "SELECT message_id FROM messages WHERE id=?1",
            params![message_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let Some(header) = header.filter(|header| !header.is_empty()) else {
        return Ok(());
    };
    let now = now_ms();
    tx.execute(
        "INSERT OR IGNORE INTO local_kept_labels(account_id, header_id, label_id, kept_at)
         SELECT ?1, ?2, label_id, ?3 FROM message_labels WHERE message_id=?4",
        params![account_id, header, now, message_id],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO local_kept_snoozes(account_id, header_id, wake_at, kept_at)
         SELECT ?1, ?2, s.wake_at, ?3 FROM snoozes s
         JOIN messages m ON m.thread_id = s.thread_id WHERE m.id=?4",
        params![account_id, header, now, message_id],
    )?;
    let twin: Option<i64> = tx
        .query_row(
            "SELECT m.id FROM messages m JOIN local_messages l ON l.message_id = m.id
             WHERE m.account_id=?1 AND m.message_id=?2 AND m.id<>?3 LIMIT 1",
            params![account_id, header, message_id],
            |row| row.get(0),
        )
        .ok();
    if let Some(twin) = twin {
        restore_meta(tx, account_id, twin)?;
    }
    Ok(())
}

/// Re-apply (and consume) labels and snooze kept for this message's header.
fn restore_meta(tx: &Transaction, account_id: i64, message_id: i64) -> Result<()> {
    let row: Option<(Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT message_id, thread_id FROM messages WHERE id=?1",
            params![message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let Some((Some(header), thread_id)) = row else {
        return Ok(());
    };
    tx.execute(
        "INSERT OR IGNORE INTO message_labels(message_id, label_id)
         SELECT ?1, label_id FROM local_kept_labels WHERE account_id=?2 AND header_id=?3",
        params![message_id, account_id, header],
    )?;
    tx.execute(
        "DELETE FROM local_kept_labels WHERE account_id=?1 AND header_id=?2",
        params![account_id, header],
    )?;
    let wake: Option<i64> = tx
        .query_row(
            "SELECT wake_at FROM local_kept_snoozes WHERE account_id=?1 AND header_id=?2",
            params![account_id, header],
            |row| row.get(0),
        )
        .ok();
    if let (Some(wake), Some(thread_id)) = (wake, thread_id) {
        let already: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM snoozes WHERE thread_id=?1)",
                params![thread_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if !already {
            let folder: Option<i64> = tx
                .query_row(
                    "SELECT folder_id FROM messages WHERE id=?1",
                    params![message_id],
                    |row| row.get(0),
                )
                .ok();
            repo::snoozes::set(tx, thread_id, account_id, wake, folder)?;
        }
    }
    tx.execute(
        "DELETE FROM local_kept_snoozes WHERE account_id=?1 AND header_id=?2",
        params![account_id, header],
    )?;
    Ok(())
}

/// A parsed message ready to be stored as a cached mail row.
pub(crate) struct ParsedMessage<'a> {
    pub account_id: i64,
    pub folder_id: i64,
    pub role: Option<&'a str>,
    pub cache_dir: &'a Path,
    pub parsed: &'a ParsedBody,
    pub raw: &'a [u8],
    pub flags: FlagSet,
    pub fallback_ms: i64,
}

/// Insert the message, its body, attachments and search entry. Returns
/// `(message_id, thread_id)`.
pub(crate) fn insert_parsed(tx: &Transaction, m: &ParsedMessage) -> Result<(i64, i64)> {
    let headers = &m.parsed.headers;
    let date = headers.date_ms.unwrap_or(m.fallback_ms);
    let thread_id = crate::sync::threading::resolve_thread(tx, m.account_id, headers, date)?;
    let has_attachments = m
        .parsed
        .attachments
        .iter()
        .any(|attachment| !attachment.is_inline);
    let new = repo::messages::NewMessage {
        account_id: m.account_id,
        folder_id: m.folder_id,
        uid: None,
        message_id: headers.message_id.clone(),
        gm_msgid: None,
        gm_thrid: None,
        subject: headers.subject.clone(),
        from: headers.from.clone(),
        to: headers.to.clone(),
        cc: headers.cc.clone(),
        bcc: headers.bcc.clone(),
        date,
        internal_date: Some(date),
        is_read: m.flags.seen,
        is_starred: m.flags.flagged,
        is_draft: m.flags.draft || m.role == Some(roles::DRAFTS),
        is_outgoing: m.role == Some(roles::SENT),
        is_automated: headers.is_automated,
        has_attachments,
        size: Some(m.raw.len() as i64),
        snippet: m.parsed.snippet.clone(),
        references: headers.references.clone(),
        list_unsubscribe: headers.list_unsubscribe.clone(),
        list_unsubscribe_post: headers.list_unsubscribe_post.clone(),
        sender_addr: headers.via.clone(),
        sender_verification: headers.sender_verification,
    };
    let message_id = repo::messages::insert(tx, &new, thread_id)?;
    let cache = m.cache_dir.join(format!("{message_id}.local.eml"));
    std::fs::write(&cache, m.raw)?;
    let cache_string = cache.to_string_lossy().into_owned();
    repo::messages::store_body(
        tx,
        message_id,
        m.parsed.text.as_deref(),
        m.parsed.html.as_deref(),
        Some(&cache_string),
        has_attachments,
        Some(&m.parsed.snippet),
    )?;
    repo::messages::set_sender_verification(tx, message_id, headers.sender_verification)?;
    let attachments = m
        .parsed
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
    repo::messages::replace_attachments(tx, message_id, &attachments)?;
    repo::search::index_message(tx, message_id)?;
    Ok((message_id, thread_id))
}

/// Where a message file lives, as recorded in `local_messages`.
pub(super) struct FileLocation<'a> {
    pub folder_id: i64,
    pub key: &'a str,
    pub rel_path: &'a str,
    pub size: i64,
    pub flags: FlagSet,
}

/// Register a message already present in the app (a just-sent copy) as a file
/// in a Maildir folder.
pub(super) fn register_file(
    tx: &Transaction,
    account_id: i64,
    message_id: i64,
    file: &FileLocation,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO local_messages(message_id, account_id, folder_id, unique_key,
                                               rel_path, mbox_offset, mbox_len, flags)
         VALUES (?1,?2,?3,?4,?5,NULL,?6,?7)",
        params![
            message_id,
            account_id,
            file.folder_id,
            file.key,
            file.rel_path,
            file.size,
            file.flags.encode()
        ],
    )?;
    Ok(())
}
