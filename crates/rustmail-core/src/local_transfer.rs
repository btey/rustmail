//! Keeping mail in a local account: copying or moving messages from any
//! account into a folder of a Local account, and backing a local account up
//! to (or restoring it from) a folder that can be carried to another computer.
//!
//! A copy takes the complete original message (every part and attachment) that
//! Rustmail already holds for the source message, stages it in a file inside
//! the app's temp folder and queues one `import_raw` action for the target
//! account. That account's own worker writes the message into the Maildir, MH
//! folder or mbox file and the normal scan imports it. A move is the same copy
//! followed by the normal Trash action on the source, queued only once every
//! message of the conversation has been filed.
//!
//! A backup is a folder holding `mail/` (a plain copy of the account's mail
//! files in their own format, readable by any mail program) and
//! `rustmail-local-backup.json` (the account's settings without credentials,
//! plus the labels and snoozes that only Rustmail knows about, keyed by
//! Message-ID). Restoring copies `mail/` to a new empty folder, adds the
//! account and hands the labels and snoozes to the first scan.

use crate::Core;
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::local;
use crate::models::{
    ActionKind, AddPasswordAccountArgs, MailProtocol, PerformActionArgs, PortableAccountConfig,
    now_ms,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Conversations a single copy may carry, and messages in them.
const MAX_COPY_THREADS: usize = 500;
const MAX_COPY_MESSAGES: usize = 2000;
/// How long a copy waits for the target account to file its messages.
const FILING_TIMEOUT: Duration = Duration::from_secs(90);
/// How long a copy waits for a message that is not downloaded yet.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_RAW_BYTES: usize = 128 * 1024 * 1024;

static STAGED: AtomicU64 = AtomicU64::new(0);

/// What a copy or move did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Messages written into the target folder.
    pub copied: usize,
    /// Messages that were not copied, each with its subject and the reason.
    pub skipped: Vec<String>,
    /// Messages still queued when the wait ended (the target account is busy
    /// or offline); they are filed later and the source is kept.
    pub waiting: usize,
    /// The source conversations were moved to Trash (a move that copied
    /// everything).
    pub source_removed: bool,
}

impl CopyReport {
    /// One line saying what went wrong, or `None` when everything was copied.
    pub fn problem(&self) -> Option<String> {
        if self.skipped.is_empty() && self.waiting == 0 {
            return None;
        }
        let mut text = format!("Copied {}", self.copied);
        if !self.skipped.is_empty() {
            text.push_str(&format!(
                "; {} could not be copied ({})",
                self.skipped.len(),
                self.skipped[0]
            ));
        }
        if self.waiting > 0 {
            text.push_str(&format!(
                "; {} are still waiting for the local account",
                self.waiting
            ));
        }
        if !self.source_removed {
            text.push_str("; the originals were kept");
        }
        Some(text)
    }
}

/// What a backup wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalBackupReport {
    pub files: u64,
    pub bytes: u64,
    /// Messages whose labels were saved with the backup.
    pub labelled_messages: usize,
    /// Conversations whose snooze was saved with the backup.
    pub snoozed_threads: usize,
}

const BACKUP_FORMAT: &str = "rustmail-local-backup";
const BACKUP_VERSION: u32 = 1;
const BACKUP_FILE: &str = "rustmail-local-backup.json";
const BACKUP_MAIL_DIR: &str = "mail";
const MAX_BACKUP_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BACKUP_ENTRIES: usize = 500_000;
const MAX_TREE_DEPTH: usize = 40;
const MAX_TREE_FILES: u64 = 5_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupLabel {
    name: String,
    color: String,
    /// A label of every account rather than of this one.
    global: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupMessage {
    /// The Message-ID header (the key Rustmail uses to find the message again).
    message_id: String,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    snooze_wake_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalBackupFile {
    format: String,
    version: u32,
    created_at: String,
    core_version: String,
    account: PortableAccountConfig,
    /// The account is one mbox file rather than a folder tree; its name.
    #[serde(default)]
    single_file: Option<String>,
    #[serde(default)]
    labels: Vec<BackupLabel>,
    #[serde(default)]
    messages: Vec<BackupMessage>,
}

/// One message of a conversation that is to be copied.
#[derive(Debug, Clone)]
struct SourceMessage {
    id: i64,
    account_id: i64,
    thread_id: i64,
    subject: String,
    is_read: bool,
    is_starred: bool,
}

fn partial_reason(
    account_is_exchange: bool,
    text: Option<&str>,
    html: Option<&str>,
) -> Option<&'static str> {
    if account_is_exchange {
        match crate::exchange::sync::stub_kind(text, html) {
            Some("rebuilt") => return Some("only a rebuilt copy of this message is available"),
            Some(_) => return Some("only the headers of this message are downloaded"),
            None => {}
        }
    }
    let head: String = text.unwrap_or_default().chars().take(200).collect();
    if head.trim_start().starts_with("[This message is large (") {
        return Some("only the headers of this message were downloaded");
    }
    None
}

impl Core {
    /// Copy (or, with `remove_source`, move) whole conversations into a folder
    /// of a Local account. Every message is copied with all its parts; a
    /// message that cannot be read in full is skipped and named in the report.
    /// A move trashes the source conversations only when every message in
    /// them was filed, so nothing is ever lost.
    pub async fn copy_threads_to_folder(
        &self,
        thread_ids: Vec<i64>,
        target_folder_id: i64,
        remove_source: bool,
    ) -> Result<CopyReport> {
        if thread_ids.is_empty() {
            return Ok(CopyReport::default());
        }
        if thread_ids.len() > MAX_COPY_THREADS {
            return Err(CoreError::Other(format!(
                "choose at most {MAX_COPY_THREADS} conversations at a time"
            )));
        }
        let (target_account, is_local, has_folder) = self
            .db
            .read(move |conn| {
                let folder = repo::folders::get(conn, target_folder_id)?
                    .ok_or_else(|| CoreError::NotFound("target folder".into()))?;
                let config = repo::accounts::get_config(conn, folder.account_id)?
                    .ok_or_else(|| CoreError::NotFound("target account".into()))?;
                let has_folder: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM local_folders WHERE folder_id = ?1)",
                    params![target_folder_id],
                    |row| row.get(0),
                )?;
                Ok((
                    config.id,
                    config.mail_protocol == MailProtocol::Local,
                    has_folder,
                ))
            })
            .await?;
        if !is_local {
            return Err(CoreError::Other(
                "mail can only be copied into the folders of a Local folders account".into(),
            ));
        }
        if !has_folder {
            return Err(CoreError::Other(
                "that folder cannot receive mail (it only holds other folders)".into(),
            ));
        }

        let ids = thread_ids.clone();
        let messages: Vec<SourceMessage> = self
            .db
            .read(move |conn| {
                let mut out = Vec::new();
                for thread_id in ids {
                    let mut statement = conn.prepare(
                        "SELECT id, account_id, COALESCE(subject, ''), is_read, is_starred
                         FROM messages WHERE thread_id = ?1 ORDER BY date, id",
                    )?;
                    let rows = statement.query_map(params![thread_id], |row| {
                        Ok(SourceMessage {
                            id: row.get(0)?,
                            account_id: row.get(1)?,
                            thread_id,
                            subject: row.get(2)?,
                            is_read: row.get::<_, i64>(3)? != 0,
                            is_starred: row.get::<_, i64>(4)? != 0,
                        })
                    })?;
                    for row in rows {
                        out.push(row?);
                    }
                }
                Ok(out)
            })
            .await?;
        if messages.len() > MAX_COPY_MESSAGES {
            return Err(CoreError::Other(format!(
                "that is more than {MAX_COPY_MESSAGES} messages; choose fewer conversations"
            )));
        }

        let mut report = CopyReport::default();
        // Per conversation: the queued actions, and whether anything stopped it
        // from being complete.
        let mut staged: Vec<(SourceMessage, PathBuf)> = Vec::new();
        let mut incomplete: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for message in messages {
            if remove_source && message.account_id == target_account {
                report.skipped.push(format!(
                    "{}: it is already in this account, use Move",
                    shorten(&message.subject)
                ));
                incomplete.insert(message.thread_id);
                continue;
            }
            match self.read_original(&message).await {
                Ok(raw) => match self.stage_message(&raw).await {
                    Ok(path) => staged.push((message, path)),
                    Err(error) => {
                        report
                            .skipped
                            .push(format!("{}: {error}", shorten(&message.subject)));
                        incomplete.insert(message.thread_id);
                    }
                },
                Err(reason) => {
                    report
                        .skipped
                        .push(format!("{}: {reason}", shorten(&message.subject)));
                    incomplete.insert(message.thread_id);
                }
            }
        }

        // One transaction queues every staged message for the target account.
        let jobs: Vec<(SourceMessage, PathBuf)> = staged.clone();
        let action_ids: Vec<i64> = if jobs.is_empty() {
            Vec::new()
        } else {
            self.db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let mut ids = Vec::new();
                    for (message, path) in &jobs {
                        ids.push(repo::actions::enqueue(
                            &tx,
                            target_account,
                            local::IMPORT_RAW,
                            None,
                            None,
                            &serde_json::json!({
                                "folderId": target_folder_id,
                                "path": path.to_string_lossy(),
                                "seen": message.is_read,
                                "flagged": message.is_starred,
                                "subject": shorten(&message.subject),
                            }),
                            None,
                        )?);
                    }
                    tx.commit()?;
                    Ok(ids)
                })
                .await?
        };
        if action_ids.is_empty() {
            return Ok(report);
        }

        // The target account files them in its next cycle; ask for one now. A
        // second request waits for a cycle that began after the first rows were
        // queued.
        for _ in 0..2 {
            let _ = self.sync_now(Some(target_account)).await;
        }
        let outcome = self.wait_for_actions(&action_ids).await?;
        for ((message, _), action_id) in staged.iter().zip(&action_ids) {
            match outcome.get(action_id) {
                Some((state, _)) if state == "done" => report.copied += 1,
                Some((state, error)) if state == "failed" || state == "cancelled" => {
                    report.skipped.push(format!(
                        "{}: {}",
                        shorten(&message.subject),
                        error
                            .clone()
                            .unwrap_or_else(|| "it could not be filed".into())
                    ));
                    incomplete.insert(message.thread_id);
                }
                _ => {
                    report.waiting += 1;
                    incomplete.insert(message.thread_id);
                }
            }
        }

        if remove_source {
            let mut done_threads: Vec<i64> = Vec::new();
            for (message, _) in &staged {
                if !incomplete.contains(&message.thread_id)
                    && !done_threads.contains(&message.thread_id)
                {
                    done_threads.push(message.thread_id);
                }
            }
            if !done_threads.is_empty() {
                self.perform_action(PerformActionArgs {
                    kind: ActionKind::Trash,
                    thread_ids: done_threads,
                    params: None,
                })
                .await?;
                report.source_removed = true;
            }
        }
        Ok(report)
    }

    /// The complete original of a message, downloading it first when needed.
    async fn read_original(&self, message: &SourceMessage) -> std::result::Result<Vec<u8>, String> {
        let message_id = message.id;
        let load = |core: &Core| {
            let core = core.db.clone();
            async move {
                core.read(move |conn| {
                    Ok(conn.query_row(
                        "SELECT m.body_state, m.raw_path, b.text_body, b.html_body,
                                (SELECT mail_protocol FROM accounts WHERE id = m.account_id)
                         FROM messages m LEFT JOIN message_bodies b ON b.message_id = m.id
                         WHERE m.id = ?1",
                        params![message_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, Option<String>>(3)?,
                                row.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )?)
                })
                .await
                .map_err(|error| error.to_string())
            }
        };
        let mut state = load(self).await?;
        // IMAP and Gmail keep only the parts that were opened, not the whole
        // message: their account downloads the original. Other accounts answer
        // "cannot" at once and the stored copy is used below.
        if state.1.is_none() {
            let handle = self.handles.read().await.get(&message.account_id).cloned();
            if let Some(handle) = handle
                && let Ok(bytes) = handle.fetch_original(message_id).await
                && bytes.len() <= MAX_RAW_BYTES
            {
                return Ok(bytes);
            }
        }
        if state.0 != "cached" {
            self.request_body(message.account_id, message_id).await;
            let deadline = tokio::time::Instant::now() + DOWNLOAD_TIMEOUT;
            while state.0 != "cached" && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(250)).await;
                state = load(self).await?;
            }
            if state.0 != "cached" {
                return Err("the message could not be downloaded".into());
            }
        }
        let is_exchange = state.4.as_deref() == Some("exchange");
        if let Some(reason) = partial_reason(is_exchange, state.2.as_deref(), state.3.as_deref()) {
            // A message cached as headers only can be downloaded whole on
            // Microsoft 365; anything else is refused rather than copied short.
            if is_exchange && !reason.contains("rebuilt") {
                self.exchange_download_message(message_id)
                    .await
                    .map_err(|error| error.to_string())?;
                state = load(self).await?;
                if partial_reason(is_exchange, state.2.as_deref(), state.3.as_deref()).is_some() {
                    return Err(reason.into());
                }
            } else {
                return Err(reason.into());
            }
        }
        let path = state
            .1
            .ok_or_else(|| "no stored original of this message".to_owned())?;
        crate::file_io::read(&path, MAX_RAW_BYTES, "stored message")
            .await
            .map_err(|_| "the stored original of this message is missing".to_owned())
    }

    /// Write a message into the staging folder of the local import queue.
    async fn stage_message(&self, raw: &[u8]) -> std::result::Result<PathBuf, String> {
        use tokio::io::AsyncWriteExt;
        let dir = local::staging_dir(&self.paths);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| error.to_string())?;
        let name = format!(
            "{}-{}.eml",
            now_ms(),
            STAGED.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(raw)
            .await
            .map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        Ok(path)
    }

    /// Wait until every queued action is done, failed or cancelled, or the
    /// timeout passes. Returns each action's last state and error.
    async fn wait_for_actions(
        &self,
        ids: &[i64],
    ) -> Result<HashMap<i64, (String, Option<String>)>> {
        let list = serde_json::to_string(ids)?;
        let deadline = tokio::time::Instant::now() + FILING_TIMEOUT;
        loop {
            let list = list.clone();
            let states: HashMap<i64, (String, Option<String>)> = self
                .db
                .read(move |conn| {
                    let mut statement = conn.prepare(
                        "SELECT id, state, last_error FROM pending_actions
                         WHERE id IN (SELECT value FROM json_each(?1))",
                    )?;
                    let rows = statement.query_map(params![list], |row| {
                        Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
                    })?;
                    Ok(rows.collect::<std::result::Result<_, _>>()?)
                })
                .await?;
            let finished = ids.iter().all(|id| {
                states
                    .get(id)
                    // A row garbage collected after it finished counts as done.
                    .is_none_or(|(state, _)| {
                        matches!(state.as_str(), "done" | "failed" | "cancelled")
                    })
            });
            if finished || tokio::time::Instant::now() >= deadline {
                let mut out = states;
                for id in ids {
                    out.entry(*id).or_insert_with(|| ("done".to_owned(), None));
                }
                return Ok(out);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Back a local account up into `destination` (a new or empty folder).
    /// See the module notes for what the folder holds.
    pub async fn backup_local_account(
        &self,
        account_id: i64,
        destination: PathBuf,
    ) -> Result<LocalBackupReport> {
        let config = self
            .db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        if config.mail_protocol != MailProtocol::Local {
            return Err(CoreError::Other(
                "only a Local folders account can be backed up this way".into(),
            ));
        }
        let root = local::scan::root_of(&config)?;

        let (labels, messages, labelled_messages, snoozed_threads) = self
            .db
            .read(move |conn| collect_meta(conn, account_id))
            .await?;

        let mut portable = PortableAccountConfig {
            email: config.email.clone(),
            display_name: config.display_name.clone(),
            provider: config.provider,
            auth_kind: config.auth_kind,
            mail_protocol: config.mail_protocol,
            username: config.username.clone(),
            jmap_url: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: config.smtp_host.clone(),
            smtp_port: config.smtp_port,
            settings: config.settings.clone(),
        };
        // The new computer has its own folder, and no secret travels.
        portable.settings.connection.local_path = String::new();
        portable.settings.connection.client_key_pem = Default::default();
        portable.settings.connection.proxy_password = Default::default();

        let single_file = if std::fs::symlink_metadata(&root)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
        {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        } else {
            None
        };
        let file = LocalBackupFile {
            format: BACKUP_FORMAT.into(),
            version: BACKUP_VERSION,
            created_at: chrono::Utc::now().to_rfc3339(),
            core_version: env!("CARGO_PKG_VERSION").into(),
            account: portable,
            single_file: single_file.clone(),
            labels,
            messages,
        };
        let json = serde_json::to_vec_pretty(&file)?;

        let (files, bytes) = tokio::task::spawn_blocking(move || -> Result<(u64, u64)> {
            prepare_destination(&destination, &root)?;
            let mail = destination.join(BACKUP_MAIL_DIR);
            std::fs::create_dir(&mail).map_err(io)?;
            let counts = match &single_file {
                Some(name) => copy_file(&root, &mail.join(name)).map(|size| (1, size)),
                None => copy_tree(&root, &mail),
            }
            .map_err(io)?;
            // The sidecar goes last: a backup without it is plainly unfinished.
            std::fs::write(destination.join(BACKUP_FILE), json).map_err(io)?;
            Ok(counts)
        })
        .await
        .map_err(|error| CoreError::Other(error.to_string()))??;
        Ok(LocalBackupReport {
            files,
            bytes,
            labelled_messages,
            snoozed_threads,
        })
    }

    /// Restore a backup made by `backup_local_account` into `target` (a new or
    /// empty folder) and add the account. Refused when an account with the
    /// same address already exists here.
    pub async fn restore_local_account(
        &self,
        backup: PathBuf,
        target: PathBuf,
    ) -> Result<crate::models::Account> {
        let read_from = backup.clone();
        let file = tokio::task::spawn_blocking(move || read_backup_file(&read_from))
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;
        let email = file.account.email.clone();
        let existing = self
            .db
            .read({
                let email = email.clone();
                move |conn| repo::accounts::find_by_email(conn, &email)
            })
            .await?;
        if existing.is_some() {
            return Err(CoreError::Other(format!(
                "an account for {email} already exists here; remove it first"
            )));
        }
        let single_file = file.single_file.clone();
        let source = backup.join(BACKUP_MAIL_DIR);
        let target_for_copy = target.clone();
        let (root, local_path) =
            tokio::task::spawn_blocking(move || -> Result<(PathBuf, PathBuf)> {
                let root = prepare_restore_target(&target_for_copy, &source)?;
                let outcome = match &single_file {
                    Some(name) => {
                        if !valid_file_name(name) {
                            return Err(CoreError::Other(
                                "the backup names an invalid file".into(),
                            ));
                        }
                        copy_file(&source.join(name), &root.join(name)).map(|_| root.join(name))
                    }
                    None => copy_tree(&source, &root).map(|_| root.clone()),
                };
                match outcome {
                    Ok(path) => Ok((root, path)),
                    Err(error) => {
                        // Nothing was there before, so remove what was copied.
                        let _ = clear_dir(&root);
                        Err(io(error))
                    }
                }
            })
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;

        let mut connection = file.account.settings.connection.clone();
        connection.local_path = local_path.to_string_lossy().into_owned();
        let args = AddPasswordAccountArgs {
            email: email.clone(),
            display_name: file.account.display_name.clone(),
            username: file.account.username.clone(),
            password: String::new(),
            mail_protocol: MailProtocol::Local,
            jmap_url: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: file.account.smtp_host.clone(),
            smtp_port: file.account.smtp_port,
            connection,
            smtp_password: None,
            client_key_pem: None,
        };
        let account = match self.add_account_password(args).await {
            Ok(account) => account,
            Err(error) => {
                let clean = root.clone();
                let _ = tokio::task::spawn_blocking(move || clear_dir(&clean)).await;
                return Err(error);
            }
        };

        let account_id = account.id;
        let labels = file.labels.clone();
        let messages = file.messages.clone();
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                apply_meta(&tx, account_id, &labels, &messages)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        // Show the restored labels at once.
        let _ = self.sync_now(Some(account_id)).await;
        Ok(account)
    }
}

fn io(error: std::io::Error) -> CoreError {
    CoreError::Other(format!("backup: {error}"))
}

fn shorten(subject: &str) -> String {
    let line = subject.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = if line.is_empty() {
        "(no subject)".to_owned()
    } else {
        line
    };
    line.chars().take(60).collect()
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// The labels and snoozes of a local account's messages, keyed by Message-ID.
#[allow(clippy::type_complexity)]
fn collect_meta(
    conn: &rusqlite::Connection,
    account_id: i64,
) -> Result<(Vec<BackupLabel>, Vec<BackupMessage>, usize, usize)> {
    let mut labels: Vec<BackupLabel> = Vec::new();
    let mut by_header: HashMap<String, BackupMessage> = HashMap::new();
    {
        let mut statement = conn.prepare(
            "SELECT m.message_id, l.name, l.color, l.owner_account_id IS NULL
             FROM messages m
             JOIN message_labels ml ON ml.message_id = m.id
             JOIN labels l ON l.id = ml.label_id
             WHERE m.account_id = ?1 AND m.message_id <> '' AND l.is_auto = 0
             ORDER BY m.id, l.name",
        )?;
        let rows = statement.query_map(params![account_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        for row in rows {
            let (header, name, color, global) = row?;
            if !labels.iter().any(|label| label.name == name) {
                labels.push(BackupLabel {
                    name: name.clone(),
                    color,
                    global,
                });
            }
            let entry = by_header
                .entry(header.clone())
                .or_insert_with(|| BackupMessage {
                    message_id: header,
                    labels: Vec::new(),
                    snooze_wake_at: None,
                });
            if !entry.labels.contains(&name) {
                entry.labels.push(name);
            }
        }
    }
    let labelled_messages = by_header.len();
    let mut snoozed_threads = 0;
    {
        let mut statement = conn.prepare(
            "SELECT m.message_id, s.wake_at FROM snoozes s
             JOIN messages m ON m.id = (SELECT MIN(id) FROM messages
                                         WHERE thread_id = s.thread_id AND message_id <> '')
             WHERE s.account_id = ?1",
        )?;
        let rows = statement.query_map(params![account_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (header, wake_at) = row?;
            by_header
                .entry(header.clone())
                .or_insert_with(|| BackupMessage {
                    message_id: header,
                    labels: Vec::new(),
                    snooze_wake_at: None,
                })
                .snooze_wake_at = Some(wake_at);
            snoozed_threads += 1;
        }
    }
    let mut messages: Vec<BackupMessage> = by_header.into_values().collect();
    messages.sort_by(|a, b| a.message_id.cmp(&b.message_id));
    Ok((labels, messages, labelled_messages, snoozed_threads))
}

/// Create the labels of a backup for the restored account and park its labels
/// and snoozes under their Message-IDs for the scan to apply.
fn apply_meta(
    tx: &rusqlite::Transaction,
    account_id: i64,
    labels: &[BackupLabel],
    messages: &[BackupMessage],
) -> Result<()> {
    let mut ids: HashMap<String, i64> = HashMap::new();
    for label in labels {
        let name = label.name.trim();
        if name.is_empty() || name.len() > 200 {
            continue;
        }
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM labels
                 WHERE name = ?1 COLLATE NOCASE
                   AND ((scope = 'global') OR (scope = 'account' AND owner_account_id = ?2))
                 ORDER BY scope = 'account' DESC LIMIT 1",
                params![name, account_id],
                |row| row.get(0),
            )
            .ok();
        let id = match existing {
            Some(id) => id,
            None => {
                let color = if label.color.len() <= 16 && label.color.starts_with('#') {
                    label.color.as_str()
                } else {
                    "#6b7280"
                };
                repo::labels::save(tx, None, name, color, 0, Some(account_id))?.id
            }
        };
        ids.insert(label.name.clone(), id);
    }
    let now = now_ms();
    for message in messages {
        let header = message.message_id.trim();
        if header.is_empty() || header.len() > 998 {
            continue;
        }
        for name in &message.labels {
            if let Some(label_id) = ids.get(name) {
                tx.execute(
                    "INSERT OR IGNORE INTO local_kept_labels(account_id, header_id, label_id, kept_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![account_id, header, label_id, now],
                )?;
            }
        }
        if let Some(wake_at) = message.snooze_wake_at {
            tx.execute(
                "INSERT OR REPLACE INTO local_kept_snoozes(account_id, header_id, wake_at, kept_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account_id, header, wake_at, now],
            )?;
        }
    }
    // The first scan may already have imported the messages.
    local::scan::restore_all_kept(tx, account_id)?;
    Ok(())
}

fn read_backup_file(dir: &Path) -> Result<LocalBackupFile> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|error| CoreError::Other(format!("cannot open the backup folder: {error}")))?;
    if !meta.is_dir() {
        return Err(CoreError::Other("choose the backup folder".into()));
    }
    let mail = dir.join(BACKUP_MAIL_DIR);
    let mail_ok = std::fs::symlink_metadata(&mail)
        .map(|meta| meta.is_dir())
        .unwrap_or(false);
    let path = dir.join(BACKUP_FILE);
    let regular = std::fs::symlink_metadata(&path)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if !mail_ok || !regular {
        return Err(CoreError::Other(
            "this folder is not a finished Rustmail local backup".into(),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(io)?
        .take(MAX_BACKUP_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_BACKUP_FILE_BYTES {
        return Err(CoreError::Other(
            "the backup description is too large".into(),
        ));
    }
    let file: LocalBackupFile = serde_json::from_slice(&bytes)
        .map_err(|error| CoreError::Other(format!("the backup description is damaged: {error}")))?;
    if file.format != BACKUP_FORMAT {
        return Err(CoreError::Other(
            "this is not a Rustmail local backup".into(),
        ));
    }
    if file.version != BACKUP_VERSION {
        return Err(CoreError::Other(
            "this backup was made by a newer Rustmail".into(),
        ));
    }
    if file.account.mail_protocol != MailProtocol::Local {
        return Err(CoreError::Other(
            "this backup is not of a local account".into(),
        ));
    }
    if file.messages.len() > MAX_BACKUP_ENTRIES || file.labels.len() > 10_000 {
        return Err(CoreError::Other(
            "the backup description is too large".into(),
        ));
    }
    let email = file.account.email.trim();
    if email.len() > 320 || !email.contains('@') {
        return Err(CoreError::Other(
            "the backup has an invalid email address".into(),
        ));
    }
    Ok(file)
}

/// The destination must be new or empty and must not lie inside the mail
/// folder being copied (nor contain it).
fn prepare_destination(destination: &Path, root: &Path) -> Result<()> {
    if std::fs::symlink_metadata(destination).is_ok() {
        if !local::is_empty_dir(destination) {
            return Err(CoreError::Other(
                "choose a new or empty folder for the backup".into(),
            ));
        }
    } else {
        std::fs::create_dir_all(destination).map_err(io)?;
    }
    let destination = std::fs::canonicalize(destination).map_err(io)?;
    let root = std::fs::canonicalize(root).map_err(io)?;
    if destination.starts_with(&root) || root.starts_with(&destination) {
        return Err(CoreError::Other(
            "the backup folder cannot be inside the mail folder (or contain it)".into(),
        ));
    }
    Ok(())
}

/// A restore target must be new or empty and outside the backup.
fn prepare_restore_target(target: &Path, source: &Path) -> Result<PathBuf> {
    if std::fs::symlink_metadata(target).is_ok() {
        if !local::is_empty_dir(target) {
            return Err(CoreError::Other(
                "choose a new or empty folder to restore into".into(),
            ));
        }
    } else {
        std::fs::create_dir_all(target).map_err(io)?;
    }
    let target = std::fs::canonicalize(target).map_err(io)?;
    let source = std::fs::canonicalize(source).map_err(io)?;
    if target.starts_with(&source) || source.starts_with(&target) {
        return Err(CoreError::Other(
            "restore into a folder outside the backup".into(),
        ));
    }
    Ok(target)
}

fn clear_dir(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let kind = std::fs::symlink_metadata(&path)?;
        if kind.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Copy one regular file, keeping its modification time. Returns its size.
fn copy_file(from: &Path, to: &Path) -> std::io::Result<u64> {
    let meta = std::fs::symlink_metadata(from)?;
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let size = std::fs::copy(from, to)?;
    if let Ok(modified) = meta.modified()
        && let Ok(file) = std::fs::OpenOptions::new().write(true).open(to)
    {
        let _ = file.set_modified(modified);
    }
    Ok(size)
}

/// Copy a directory tree: regular files and folders only (links and special
/// files are left out), bounded in depth and size, modification times kept.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<(u64, u64)> {
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    copy_dir(from, to, 0, &mut files, &mut bytes)?;
    Ok((files, bytes))
}

fn copy_dir(
    from: &Path,
    to: &Path,
    depth: usize,
    files: &mut u64,
    bytes: &mut u64,
) -> std::io::Result<()> {
    if depth > MAX_TREE_DEPTH {
        return Err(std::io::Error::other("the folders are nested too deeply"));
    }
    std::fs::create_dir_all(to)?;
    let source_meta = std::fs::symlink_metadata(from)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_dir(&entry.path(), &target, depth + 1, files, bytes)?;
        } else if kind.is_file() {
            *files += 1;
            if *files > MAX_TREE_FILES {
                return Err(std::io::Error::other("there are too many files to copy"));
            }
            *bytes += copy_file(&entry.path(), &target)?;
        }
    }
    // Directory times last, because filing files into them changed them: a
    // Maildir's cur/ and new/ times are what a rescan compares.
    if let Ok(modified) = source_meta.modified()
        && let Ok(dir) = std::fs::File::open(to)
    {
        let _ = dir.set_modified(modified);
    }
    Ok(())
}
