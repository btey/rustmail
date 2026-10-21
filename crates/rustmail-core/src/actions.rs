//! User-defined message actions and the external composing editor.
//!
//! Both run commands the user wrote in their own settings. The message itself
//! is only ever handed over as a file (`%f`) and as environment variables, so
//! hostile subjects or senders cannot change what is executed.

use crate::{
    Core,
    db::repo,
    error::{CoreError, Result},
    models::MessageAction,
};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

pub const MAX_ACTIONS: usize = 30;
const MAX_NAME_BYTES: usize = 60;
const MAX_COMMAND_BYTES: usize = 2048;
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
/// How long an action may run before it is killed.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(60);
/// The editor is interactive, so it gets a very generous limit.
pub const EDITOR_TIMEOUT: Duration = Duration::from_secs(8 * 60 * 60);
/// Largest draft text read back from the editor.
const MAX_EDITED_BYTES: u64 = 5 * 1024 * 1024;
/// Largest original message an action will work on.
const MAX_ORIGINAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    pub success: bool,
    pub code: Option<i32>,
    /// Combined standard output and error, trimmed and limited to 8 KiB.
    pub output: String,
}

/// Quote a path for the platform shell.
pub fn quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(unix)]
    {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
    #[cfg(not(unix))]
    {
        format!("\"{}\"", text.replace('"', ""))
    }
}

/// Replace `%f` with the quoted path and `%%` with `%`. Any other `%x` stays
/// as typed, so commands that use `%` themselves (such as `date +%F`) work.
pub fn expand_command(template: &str, file: &Path) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('f') => {
                chars.next();
                out.push_str(&quote_path(file));
            }
            Some('%') => {
                chars.next();
                out.push('%');
            }
            _ => out.push('%'),
        }
    }
    out
}

fn shell(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    }
    #[cfg(not(unix))]
    {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").arg(command);
        process
    }
}

async fn read_limited<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Keep draining so a chatty command never blocks on a full
                // pipe, but retain only the first part.
                let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    kept
}

/// Run `command` through the shell with no input, a deadline and bounded
/// output. The child is killed when the deadline passes or this future is
/// dropped.
pub async fn run_shell(
    command: &str,
    env: &[(&str, &str)],
    timeout: Duration,
) -> Result<ActionOutcome> {
    let mut process = shell(command);
    for (key, value) in env {
        process.env(key, value);
    }
    let mut child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Other(format!("could not start the command: {e}")))?;
    let stdout = child.stdout.take().map(read_limited);
    let stderr = child.stderr.take().map(read_limited);
    let run = async {
        let (out, err, status) = tokio::join!(
            async {
                match stdout {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            async {
                match stderr {
                    Some(f) => f.await,
                    None => Vec::new(),
                }
            },
            child.wait()
        );
        (out, err, status)
    };
    let (out, err, status) = match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => {
            return Err(CoreError::Other(format!(
                "the command did not finish within {} seconds and was stopped",
                timeout.as_secs()
            )));
        }
    };
    let status = status.map_err(|e| CoreError::Other(e.to_string()))?;
    let mut output = String::from_utf8_lossy(&out).into_owned();
    let err = String::from_utf8_lossy(&err);
    if !err.trim().is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&err);
    }
    output.truncate(output.floor_char_boundary(MAX_OUTPUT_BYTES));
    output.truncate(output.trim_end().len());
    Ok(ActionOutcome {
        success: status.success(),
        code: status.code(),
        output,
    })
}

/// Open `text` in the user's editor and return what they saved. `command` must
/// contain `%f`. The text is never changed when the editor fails.
pub async fn edit_externally(command: &str, text: &str, timeout: Duration) -> Result<String> {
    if !command.contains("%f") {
        return Err(CoreError::Other(
            "the external editor command must contain %f for the file".into(),
        ));
    }
    let dir = tempfile::Builder::new()
        .prefix("rustmail-edit-")
        .tempdir()
        .map_err(|e| CoreError::Other(e.to_string()))?;
    let file = dir.path().join("message.txt");
    tokio::fs::write(&file, text)
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?;
    let outcome = run_shell(&expand_command(command, &file), &[], timeout).await?;
    if !outcome.success {
        return Err(CoreError::Other(format!(
            "the editor exited with {}; your text was not changed",
            outcome
                .code
                .map_or_else(|| "an error".to_owned(), |c| format!("status {c}"))
        )));
    }
    let size = tokio::fs::metadata(&file)
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?
        .len();
    if size > MAX_EDITED_BYTES {
        return Err(CoreError::Other(
            "the edited text is larger than 5 MiB".into(),
        ));
    }
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|e| CoreError::Other(e.to_string()))?;
    Ok(String::from_utf8_lossy(&bytes).replace("\r\n", "\n"))
}

fn validate_command(command: &str, what: &str) -> Result<()> {
    if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err(CoreError::Other(format!(
            "Enter a {what} of 1–{MAX_COMMAND_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Core {
    pub async fn save_message_actions(
        &self,
        actions: Vec<MessageAction>,
    ) -> Result<Vec<MessageAction>> {
        if actions.len() > MAX_ACTIONS {
            return Err(CoreError::Other(format!(
                "Use at most {MAX_ACTIONS} actions"
            )));
        }
        let mut cleaned = Vec::with_capacity(actions.len());
        for mut action in actions {
            action.name = action.name.trim().to_owned();
            action.command = action.command.trim().to_owned();
            if action.name.is_empty() || action.name.len() > MAX_NAME_BYTES {
                return Err(CoreError::Other(format!(
                    "Use an action name of 1–{MAX_NAME_BYTES} bytes"
                )));
            }
            validate_command(&action.command, "command")?;
            if action.id.is_empty() {
                action.id = format!("act-{:032x}", rand::random::<u128>());
            }
            cleaned.push(action);
        }
        let saved = cleaned.clone();
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings.message_actions = cleaned;
                repo::settings::set(conn, &settings)
            })
            .await?;
        Ok(saved)
    }

    pub async fn set_external_editor_command(&self, command: String) -> Result<String> {
        let command = command.trim().to_owned();
        if !command.is_empty() {
            validate_command(&command, "editor command")?;
            if !command.contains("%f") {
                return Err(CoreError::Other(
                    "The editor command must contain %f, the file to edit".into(),
                ));
            }
        }
        let saved = command.clone();
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings.external_editor_command = command;
                repo::settings::set(conn, &settings)
            })
            .await?;
        Ok(saved)
    }

    /// The newest real message of a conversation, for callers that only know
    /// the conversation (its drafts are never chosen).
    pub async fn latest_message_in_thread(&self, thread_id: i64) -> Result<i64> {
        self.db
            .read(move |conn| {
                conn.query_row(
                    "SELECT id FROM messages WHERE thread_id = ?1 AND is_draft = 0
                     ORDER BY date DESC, id DESC LIMIT 1",
                    rusqlite::params![thread_id],
                    |r| r.get(0),
                )
                .map_err(|_| CoreError::NotFound("message".into()))
            })
            .await
    }

    /// The original message (RFC 5322 source). It comes from the local copy
    /// when there is one; otherwise it is downloaded from the IMAP server on
    /// demand, because the app normally keeps only a message's decoded parts.
    pub async fn original_message(&self, message_id: i64) -> Result<Vec<u8>> {
        let (row, folder, config) = self
            .db
            .read(move |conn| {
                let row = repo::messages::get_row(conn, message_id)?
                    .ok_or_else(|| CoreError::NotFound("message".into()))?;
                let folder = match row.folder_id {
                    Some(id) => repo::folders::get(conn, id)?,
                    None => None,
                };
                let config = repo::accounts::get_config(conn, row.account_id)?;
                Ok((row, folder, config))
            })
            .await?;
        if let Some(path) = &row.raw_path
            && let Ok(bytes) = crate::file_io::read(path, MAX_ORIGINAL_BYTES, "message").await
        {
            return Ok(bytes);
        }
        let config = config.ok_or_else(|| CoreError::NotFound("account".into()))?;
        let (Some(folder), Some(uid)) = (folder, row.uid) else {
            return Err(CoreError::Other(
                "The original of this message is no longer available".into(),
            ));
        };
        if config.provider != crate::models::Provider::Imap
            || config.mail_protocol != crate::models::MailProtocol::Imap
        {
            return Err(CoreError::Other(
                "Actions can only fetch the original message from IMAP accounts".into(),
            ));
        }
        let uid = u32::try_from(uid)
            .map_err(|_| CoreError::Other("The message has an invalid server id".into()))?;
        let mut session = self.connect_folder_imap(&config).await?;
        let fetched = async {
            crate::imap::select(&mut session, &folder.imap_name).await?;
            crate::imap::fetch_full(&mut session, uid).await
        }
        .await;
        crate::imap::logout(session).await;
        let bytes =
            fetched?.ok_or_else(|| CoreError::NotFound("the message on the server".into()))?;
        if bytes.len() > MAX_ORIGINAL_BYTES {
            return Err(CoreError::Other("The message is larger than 64 MiB".into()));
        }
        Ok(bytes)
    }

    /// Run a saved action on a message. The command works on a private
    /// temporary copy of the original, so it cannot damage the stored
    /// message, and the copy is removed afterwards.
    pub async fn run_message_action(
        &self,
        message_id: i64,
        action_id: String,
    ) -> Result<ActionOutcome> {
        let (action, subject, from) = self
            .db
            .read(move |conn| {
                let settings = repo::settings::get(conn)?;
                let action = settings
                    .message_actions
                    .into_iter()
                    .find(|a| a.id == action_id)
                    .ok_or_else(|| CoreError::NotFound("action".into()))?;
                let detail = repo::messages::detail(conn, message_id)?;
                Ok((action, detail.subject, detail.from.email))
            })
            .await?;
        let original = self.original_message(message_id).await?;
        let dir = tempfile::Builder::new()
            .prefix("rustmail-action-")
            .tempdir()
            .map_err(|e| CoreError::Other(e.to_string()))?;
        let file = dir.path().join("message.eml");
        tokio::fs::write(&file, &original)
            .await
            .map_err(|e| CoreError::Other(format!("could not save the message: {e}")))?;
        let command = expand_command(&action.command, &file);
        let file_text = file.to_string_lossy().into_owned();
        run_shell(
            &command,
            &[
                ("RUSTMAIL_MESSAGE_FILE", file_text.as_str()),
                ("RUSTMAIL_SUBJECT", subject.as_str()),
                ("RUSTMAIL_FROM", from.as_str()),
            ],
            ACTION_TIMEOUT,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_expand_with_safe_quoting() {
        let path = Path::new("/tmp/with space/it's.eml");
        #[cfg(unix)]
        assert_eq!(
            expand_command("cat %f | wc -l; date +%F; echo 100%%", path),
            "cat '/tmp/with space/it'\\''s.eml' | wc -l; date +%F; echo 100%"
        );
        assert_eq!(expand_command("no placeholders", path), "no placeholders");
        assert_eq!(expand_command("trailing %", path), "trailing %");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_with_environment_output_and_status() {
        let ok = run_shell(
            "printf '%s|%s' \"$A\" \"$B\"; printf ' err' >&2",
            &[("A", "x y"), ("B", "$(touch /tmp/should-not-exist)")],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(ok.success);
        assert_eq!(ok.output, "x y|$(touch /tmp/should-not-exist)\n err");
        assert!(!Path::new("/tmp/should-not-exist").exists());

        let failed = run_shell("echo nope; exit 7", &[], Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!failed.success);
        assert_eq!(failed.code, Some(7));
        assert_eq!(failed.output, "nope");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_commands_are_stopped_and_output_is_bounded() {
        let started = std::time::Instant::now();
        let error = run_shell("sleep 30", &[], Duration::from_millis(300))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let flood = run_shell(
            "yes abcdefghij | head -c 1000000",
            &[],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(flood.output.len() <= MAX_OUTPUT_BYTES);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_editor_round_trips_text_and_never_loses_it_on_failure() {
        let edited = edit_externally(
            "sed -i 's/world/there/' %f",
            "hello world\nline two\n",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(edited, "hello there\nline two\n");
        let error = edit_externally("exit 1; : %f", "keep me", Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not changed"), "{error}");
        assert!(
            edit_externally("vim", "x", Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
