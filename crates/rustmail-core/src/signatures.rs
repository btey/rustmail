//! Account-scoped signature validation and plain-text composition.
use crate::{
    Core,
    db::repo,
    error::{CoreError, Result},
    models::{Signature, SignatureDefaults, SignatureSource},
};

pub const MAX_SIGNATURE_BYTES: usize = 32 * 1024;
const MAX_SOURCE_VALUE_BYTES: usize = 1024;
/// A signature command that has not finished by now is killed.
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The signature text to insert in a message being composed. `Text` signatures
/// come from the stored body. `File` and `Command` signatures are read or run
/// now, so this may block for up to `COMMAND_TIMEOUT`; callers on a UI thread
/// should expect that. Failures become a short visible note instead of an
/// empty signature, so a broken command is noticed rather than silently
/// dropped.
pub fn resolve_text(signature: &Signature) -> String {
    match signature.source {
        SignatureSource::Text => plain_text(signature),
        SignatureSource::File => match read_signature_file(&signature.source_value) {
            Ok(text) => text,
            Err(error) => format!("[signature file: {error}]"),
        },
        SignatureSource::Command => match run_signature_command(&signature.source_value) {
            Ok(text) => text,
            Err(error) => format!("[signature command: {error}]"),
        },
    }
}

fn clean_output(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).replace('\r', "");
    text.truncate(text.trim_end().len());
    text.replace('\0', "")
}

pub fn read_signature_file(path: &str) -> std::result::Result<String, String> {
    use std::io::Read;
    let path = expand_home(path.trim());
    let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_SIGNATURE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_SIGNATURE_BYTES {
        return Err("file is larger than 32 KiB".into());
    }
    Ok(clean_output(&bytes))
}

/// `~/` in a user-entered path means the home directory.
fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(path),
    }
}

/// Run a user-configured command through the platform shell with no input,
/// a hard timeout and bounded output. The command comes from the user's own
/// settings (never from a message), so no message content is interpolated.
pub fn run_signature_command(command: &str) -> std::result::Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let command = command.trim();
    if command.is_empty() {
        return Err("no command configured".into());
    }
    #[cfg(unix)]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    #[cfg(windows)]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    };
    let mut child = process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("no output pipe")?;
    // Read on a thread so a command that fills the pipe cannot block the wait.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout
            .take(MAX_SIGNATURE_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", COMMAND_TIMEOUT.as_secs()));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    };
    let bytes = reader.join().map_err(|_| "output reader failed")?;
    // Oversized output is reported first: the command is cut off by a broken
    // pipe once reading stops, so its exit status would be misleading.
    if bytes.len() > MAX_SIGNATURE_BYTES {
        return Err("output is larger than 32 KiB".into());
    }
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(clean_output(&bytes))
}

pub fn plain_text(signature: &Signature) -> String {
    let mut text = mail_parser::decoders::html::html_to_text(&signature.html);
    text.truncate(text.trim_end().len());
    let start = text.len() - text.trim_start().len();
    if start > 0 {
        text.drain(..start);
    }
    // The decoder reserves the HTML input length, which can greatly exceed
    // the retained text after markup removal and whitespace trimming.
    text.shrink_to_fit();
    text
}

pub fn text_html(text: &str) -> String {
    let extra: usize = text
        .bytes()
        .map(|byte| match byte {
            b'&' => 4,
            b'<' | b'>' | b'\n' => 3,
            _ => 0,
        })
        .sum();
    let mut html = String::with_capacity(text.len() + extra + 11);
    html.push_str("<div>");
    for ch in text.chars() {
        match ch {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '\n' => html.push_str("<br>"),
            _ => html.push(ch),
        }
    }
    html.push_str("</div>");
    html
}

/// Insert above the quotation, preserving the standard `-- ` delimiter.
pub fn insert(body: &str, signature: &str) -> String {
    let signature = signature.trim();
    if signature.is_empty() {
        return body.to_owned();
    }
    let at = body
        .find("\n\nOn ")
        .or_else(|| body.find("\n\n---------- Forwarded message"))
        .unwrap_or(body.len());
    let mut result = String::with_capacity(body.len() + signature.len() + 6);
    result.push_str(&body[..at]);
    result.push_str("\n\n-- \n");
    result.push_str(signature);
    result.push_str(&body[at..]);
    result
}

impl Core {
    pub async fn save_signature(&self, mut signature: Signature) -> Result<Signature> {
        signature.name = signature.name.trim().to_owned();
        signature.source_value = signature.source_value.trim().to_owned();
        if signature.source == SignatureSource::Text {
            signature.source_value.clear();
        } else {
            if signature.source_value.is_empty()
                || signature.source_value.len() > MAX_SOURCE_VALUE_BYTES
                || signature.source_value.contains('\0')
            {
                return Err(CoreError::Other(
                    "Enter a file path or command of up to 1024 bytes".into(),
                ));
            }
            // The text is produced when composing, not stored with the signature.
            signature.html.clear();
        }
        if signature.name.is_empty()
            || signature.name.len() > 100
            || signature.html.len() > MAX_SIGNATURE_BYTES
        {
            return Err(CoreError::Other(
                "Use a signature name of 1–100 bytes and a body under 32 KiB".into(),
            ));
        }
        // No active HTML, images or remote requests in signature settings.
        signature.html = ammonia::Builder::default()
            .rm_tags(&["img"])
            .clean(&signature.html)
            .to_string();
        if signature.id.is_empty() {
            signature.id = format!("sig-{:032x}", rand::random::<u128>());
        }
        self.db
            .write(move |conn| {
                if repo::accounts::get(conn, signature.account_id)?.is_none() {
                    return Err(CoreError::NotFound("account".into()));
                }
                let mut settings = repo::settings::get(conn)?;
                if let Some(existing) = settings
                    .signature_list
                    .iter_mut()
                    .find(|s| s.id == signature.id)
                {
                    if existing.account_id != signature.account_id {
                        return Err(CoreError::Other(
                            "Signature belongs to another account".into(),
                        ));
                    }
                    *existing = signature.clone();
                } else {
                    if settings
                        .signature_list
                        .iter()
                        .filter(|s| s.account_id == signature.account_id)
                        .count()
                        >= 20
                    {
                        return Err(CoreError::Other(
                            "An account can have up to 20 signatures".into(),
                        ));
                    }
                    settings.signature_list.push(signature.clone());
                }
                repo::settings::set(conn, &settings)?;
                Ok(signature)
            })
            .await
    }

    pub async fn delete_signature(&self, account_id: i64, id: String) -> Result<()> {
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings
                    .signature_list
                    .retain(|s| !(s.account_id == account_id && s.id == id));
                if let Some(defaults) = settings.signature_defaults.get_mut(&account_id.to_string())
                {
                    if defaults.new_id.as_ref() == Some(&id) {
                        defaults.new_id = None;
                    }
                    if defaults.reply_id.as_ref() == Some(&id) {
                        defaults.reply_id = None;
                    }
                }
                repo::settings::set(conn, &settings)
            })
            .await
    }

    pub async fn set_signature_defaults(
        &self,
        account_id: i64,
        defaults: SignatureDefaults,
    ) -> Result<()> {
        self.db
            .write(move |conn| {
                if repo::accounts::get(conn, account_id)?.is_none() {
                    return Err(CoreError::NotFound("account".into()));
                }
                let mut settings = repo::settings::get(conn)?;
                for id in [&defaults.new_id, &defaults.reply_id].into_iter().flatten() {
                    if !settings
                        .signature_list
                        .iter()
                        .any(|s| s.account_id == account_id && &s.id == id)
                    {
                        return Err(CoreError::Other(
                            "Choose a signature belonging to this account".into(),
                        ));
                    }
                }
                settings
                    .signature_defaults
                    .insert(account_id.to_string(), defaults);
                repo::settings::set(conn, &settings)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inserts_before_history_and_escapes_text() {
        assert_eq!(
            insert("Reply\n\nOn Monday, Pat wrote:\n> Hello", "Me"),
            "Reply\n\n-- \nMe\n\nOn Monday, Pat wrote:\n> Hello"
        );
        let s = Signature {
            id: "s".into(),
            account_id: 1,
            name: "Work".into(),
            html: text_html("Zoë <me@example.org>\nEngineer"),
            source: Default::default(),
            source_value: String::new(),
        };
        assert_eq!(plain_text(&s), "Zoë <me@example.org>\nEngineer");
        assert_eq!(insert("Body", ""), "Body");
    }

    fn sig(source: SignatureSource, value: &str) -> Signature {
        Signature {
            id: "s".into(),
            account_id: 1,
            name: "Dynamic".into(),
            html: String::new(),
            source,
            source_value: value.into(),
        }
    }

    #[test]
    fn file_signatures_are_read_trimmed_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sig.txt");
        std::fs::write(&path, "Zoë\r\nEngineer\n\n").unwrap();
        assert_eq!(
            resolve_text(&sig(SignatureSource::File, path.to_str().unwrap())),
            "Zoë\nEngineer"
        );
        std::fs::write(&path, vec![b'a'; MAX_SIGNATURE_BYTES + 1]).unwrap();
        assert!(
            read_signature_file(path.to_str().unwrap())
                .unwrap_err()
                .contains("32 KiB")
        );
        let missing = resolve_text(&sig(SignatureSource::File, "/nonexistent/sig.txt"));
        assert!(missing.starts_with("[signature file:"), "{missing}");
    }

    #[cfg(unix)]
    #[test]
    fn command_signatures_run_without_input_and_report_failures() {
        assert_eq!(
            run_signature_command("printf 'Line one\\nLine two\\n\\n'").unwrap(),
            "Line one\nLine two"
        );
        // No terminal input: a command that reads stdin sees end of file.
        assert_eq!(run_signature_command("cat; printf done").unwrap(), "done");
        assert!(
            run_signature_command("exit 3")
                .unwrap_err()
                .contains("exited")
        );
        assert!(run_signature_command("  ").is_err());
        let failed = resolve_text(&sig(SignatureSource::Command, "exit 1"));
        assert!(failed.starts_with("[signature command:"), "{failed}");
    }

    #[cfg(unix)]
    #[test]
    fn runaway_commands_are_killed_and_output_is_capped() {
        let started = std::time::Instant::now();
        let error = run_signature_command("sleep 30").unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < COMMAND_TIMEOUT + std::time::Duration::from_secs(2));
        let error = run_signature_command("yes | head -c 200000").unwrap_err();
        assert!(error.contains("32 KiB"), "{error}");
    }
}
