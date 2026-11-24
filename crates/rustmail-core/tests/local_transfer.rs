//! Keeping mail in a local account: copies and moves from another account, a
//! new local store on an empty folder, and a backup that restores on another
//! profile.

use rustmail_core::{
    Core,
    config::Paths,
    models::{AddPasswordAccountArgs, LocalFormat, MailConnectionSettings, MailProtocol},
};
use std::path::{Path, PathBuf};

const ATTACHMENT_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4v";

fn plain(subject: &str, id: &str, body: &str) -> String {
    format!(
        "From: Alice <alice@example.org>\r\nTo: me@example.org\r\nSubject: {subject}\r\nMessage-ID: <{id}@example.org>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\r\n{body}\r\n"
    )
}

fn with_attachment(subject: &str, id: &str) -> String {
    format!(
        "From: Alice <alice@example.org>\r\nTo: me@example.org\r\nSubject: {subject}\r\nMessage-ID: <{id}@example.org>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"bnd\"\r\n\r\n--bnd\r\nContent-Type: text/plain\r\n\r\nSee the attached file.\r\n--bnd\r\nContent-Type: application/octet-stream; name=\"data.bin\"\r\nContent-Disposition: attachment; filename=\"data.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{ATTACHMENT_B64}\r\n--bnd--\r\n"
    )
}

fn make_maildir(dir: &Path) {
    for sub in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
}

fn args(email: &str, path: &Path, format: LocalFormat) -> AddPasswordAccountArgs {
    AddPasswordAccountArgs {
        email: email.into(),
        display_name: Some("Test".into()),
        username: String::new(),
        password: String::new(),
        mail_protocol: MailProtocol::Local,
        jmap_url: String::new(),
        imap_host: String::new(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        connection: MailConnectionSettings {
            local_path: path.to_string_lossy().into_owned(),
            local_format: format,
            ..Default::default()
        },
        smtp_password: None,
        client_key_pem: None,
    }
}

async fn start(temp: &Path, profile: &str) -> Core {
    Core::start(Paths::for_tests(&temp.join(profile)))
        .await
        .unwrap()
}

async fn settle(core: &Core, account_id: i64) {
    core.sync_now(Some(account_id)).await.unwrap();
    core.sync_now(Some(account_id)).await.unwrap();
}

fn files(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .map(|read| {
            read.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

async fn folder_id(core: &Core, account_id: i64, name: &'static str) -> i64 {
    core.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT id FROM folders WHERE account_id=?1 AND imap_name=?2",
                rusqlite::params![account_id, name],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

async fn thread_of(core: &Core, account_id: i64, subject: &'static str) -> i64 {
    core.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT thread_id FROM messages WHERE account_id=?1 AND subject=?2",
                rusqlite::params![account_id, subject],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// (folder name, has attachments, read, starred) of every message with this
/// subject in the account.
async fn found(
    core: &Core,
    account_id: i64,
    subject: &'static str,
) -> Vec<(String, bool, bool, bool)> {
    core.db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT f.imap_name, m.has_attachments, m.is_read, m.is_starred
                 FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE m.account_id=?1 AND m.subject=?2 ORDER BY m.id",
            )?;
            Ok(stmt
                .query_map(rusqlite::params![account_id, subject], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, i64>(2)? != 0,
                        r.get::<_, i64>(3)? != 0,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

fn staged(temp: &Path, profile: &str) -> Vec<String> {
    files(&temp.join(profile).join("cache/tmp/local-import"))
}

/// A source Maildir holding "Report" (with an attachment, read, starred) and a
/// plain message, plus an empty folder for the target account.
struct Setup {
    core: Core,
    source: i64,
    target: i64,
    target_root: PathBuf,
    raw: String,
}

async fn setup(temp: &Path, format: LocalFormat) -> Setup {
    let source_root = temp.join("source");
    make_maildir(&source_root);
    let raw = with_attachment("Report", "rep");
    std::fs::write(source_root.join("cur/1700000001.M1P1.host:2,FS"), &raw).unwrap();
    std::fs::write(
        source_root.join("cur/1700000002.M2P1.host:2,"),
        plain("Plain", "pl", "just text"),
    )
    .unwrap();
    let target_root = temp.join("kept");
    std::fs::create_dir_all(&target_root).unwrap();
    let core = start(temp, "profile").await;
    let source = core
        .add_account_password(args(
            "source@example.org",
            &source_root,
            LocalFormat::Maildir,
        ))
        .await
        .unwrap();
    let target = core
        .add_account_password(args("kept@example.org", &target_root, format))
        .await
        .unwrap();
    settle(&core, source.id).await;
    settle(&core, target.id).await;
    Setup {
        core,
        source: source.id,
        target: target.id,
        target_root,
        raw,
    }
}

#[tokio::test]
async fn an_empty_folder_becomes_a_new_local_store_in_every_format() {
    for (format, expect) in [
        (LocalFormat::Maildir, "cur"),
        (LocalFormat::Mh, ".mh_sequences"),
        (LocalFormat::Mbox, "INBOX"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("new-store");
        std::fs::create_dir_all(&root).unwrap();
        let core = start(temp.path(), "profile").await;
        let account = core
            .add_account_password(args("store@example.org", &root, format))
            .await
            .unwrap();
        settle(&core, account.id).await;
        assert!(root.join(expect).exists(), "{format:?}: {:?}", files(&root));
        let inbox = folder_id(&core, account.id, "INBOX").await;
        assert!(inbox > 0);
    }
    // A folder with something in it that is no mail store is still refused.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("notes");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("todo.txt"), "buy milk").unwrap();
    let core = start(temp.path(), "profile").await;
    let error = core
        .add_account_password(args("notes@example.org", &root, LocalFormat::Maildir))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no Maildir"), "{error}");
}

#[tokio::test]
async fn a_message_is_copied_with_its_attachment_and_the_source_stays() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;

    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, false)
        .await
        .unwrap();
    assert_eq!(report.copied, 1, "{report:?}");
    assert_eq!(report.problem(), None);
    assert!(!report.source_removed);

    // The whole original is in the target folder, byte for byte, flags kept.
    let delivered = files(&s.target_root.join("cur"));
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert!(delivered[0].ends_with(":2,FS"), "{}", delivered[0]);
    assert_eq!(
        std::fs::read_to_string(s.target_root.join("cur").join(&delivered[0])).unwrap(),
        s.raw
    );
    assert!(staged(temp.path(), "profile").is_empty());

    // It shows up in the target account, with its attachment, and the source
    // message is untouched.
    settle(&s.core, s.target).await;
    assert_eq!(
        found(&s.core, s.target, "Report").await,
        vec![("INBOX".to_owned(), true, true, true)]
    );
    assert_eq!(
        found(&s.core, s.source, "Report").await,
        vec![("INBOX".to_owned(), true, true, true)]
    );
}

#[tokio::test]
async fn a_move_trashes_the_source_after_the_copy() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;

    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, true)
        .await
        .unwrap();
    assert_eq!(report.copied, 1, "{report:?}");
    assert!(report.source_removed);
    settle(&s.core, s.source).await;
    settle(&s.core, s.target).await;
    assert_eq!(found(&s.core, s.target, "Report").await.len(), 1);
    let source = found(&s.core, s.source, "Report").await;
    assert_eq!(source.len(), 1);
    assert_eq!(source[0].0, "Trash", "{source:?}");
    // The other message stays where it was.
    assert_eq!(found(&s.core, s.source, "Plain").await[0].0, "INBOX");
}

#[tokio::test]
async fn a_message_without_a_stored_original_is_downloaded_whole_by_its_account() {
    // IMAP and Gmail keep only the parts that were opened; their account is
    // asked for the whole message. A local account answers the same request
    // from its files, which exercises the same path.
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;
    let source = s.source;
    s.core
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE messages SET raw_path = NULL WHERE account_id=?1 AND subject='Report'",
                [source],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, false)
        .await
        .unwrap();
    assert_eq!(report.copied, 1, "{report:?}");
    let delivered = files(&s.target_root.join("cur"));
    assert_eq!(
        std::fs::read_to_string(s.target_root.join("cur").join(&delivered[0])).unwrap(),
        s.raw
    );
}

#[tokio::test]
async fn a_message_that_cannot_be_read_in_full_is_skipped_and_the_source_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;
    // The stored original of the message is gone.
    let raw_path: String = s
        .core
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT raw_path FROM messages WHERE account_id=?1 AND subject='Report'",
                [s.source],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    std::fs::remove_file(&raw_path).unwrap();

    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, true)
        .await
        .unwrap();
    assert_eq!(report.copied, 0);
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].contains("Report"), "{report:?}");
    assert!(!report.source_removed);
    assert!(report.problem().unwrap().contains("originals were kept"));
    settle(&s.core, s.source).await;
    assert_eq!(found(&s.core, s.source, "Report").await[0].0, "INBOX");
    assert!(files(&s.target_root.join("cur")).is_empty());
}

#[tokio::test]
async fn a_copy_goes_into_mbox_and_mh_targets_too() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Mbox).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;
    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, false)
        .await
        .unwrap();
    assert_eq!(report.copied, 1, "{report:?}");
    let mbox = std::fs::read_to_string(s.target_root.join("INBOX")).unwrap();
    assert!(mbox.starts_with("From "), "{mbox}");
    assert!(mbox.contains("Subject: Report"));
    assert!(mbox.contains(ATTACHMENT_B64));
    settle(&s.core, s.target).await;
    assert_eq!(found(&s.core, s.target, "Report").await.len(), 1);

    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Mh).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    let thread = thread_of(&s.core, s.source, "Report").await;
    let report = s
        .core
        .copy_threads_to_folder(vec![thread], inbox, false)
        .await
        .unwrap();
    assert_eq!(report.copied, 1, "{report:?}");
    assert_eq!(
        std::fs::read_to_string(s.target_root.join("1")).unwrap(),
        s.raw
    );
    settle(&s.core, s.target).await;
    assert_eq!(found(&s.core, s.target, "Report").await.len(), 1);
}

#[tokio::test]
async fn copies_into_a_missing_folder_or_within_a_move_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let thread = thread_of(&s.core, s.source, "Report").await;
    assert!(
        s.core
            .copy_threads_to_folder(vec![thread], 999_999, false)
            .await
            .is_err()
    );
    // Nothing to do is not an error.
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    assert_eq!(
        s.core
            .copy_threads_to_folder(Vec::new(), inbox, false)
            .await
            .unwrap(),
        Default::default()
    );
    // A "move" into the account the mail already lives in is left to Move.
    let own = folder_id(&s.core, s.source, "Trash").await;
    let report = s
        .core
        .copy_threads_to_folder(vec![thread], own, true)
        .await
        .unwrap();
    assert_eq!(report.copied, 0);
    assert_eq!(report.skipped.len(), 1);
    assert!(!report.source_removed);
    assert_eq!(found(&s.core, s.source, "Report").await[0].0, "INBOX");
}

#[tokio::test]
async fn a_failed_copy_can_be_retried_from_the_failed_changes() {
    let temp = tempfile::tempdir().unwrap();
    let s = setup(temp.path(), LocalFormat::Maildir).await;
    let inbox = folder_id(&s.core, s.target, "INBOX").await;
    // A queued copy whose staged file is not in the staging folder is refused
    // and named by its subject in the failed list.
    let outside = temp.path().join("outside.eml");
    std::fs::write(&outside, plain("Outside", "out", "not staged")).unwrap();
    let outside_path = outside.to_string_lossy().into_owned();
    let id: i64 = s
        .core
        .db
        .write(move |conn| {
            rustmail_core::db::repo::actions::enqueue(
                conn,
                s.target,
                "import_raw",
                None,
                None,
                &serde_json::json!({
                    "folderId": inbox,
                    "path": outside_path,
                    "subject": "Quarterly report",
                }),
                None,
            )
        })
        .await
        .unwrap();
    // Eight attempts, each waiting out its backoff, would take too long: put
    // the row straight into the failed state the worker would reach.
    s.core
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE pending_actions SET state='failed', last_error='the staged message is missing' WHERE id=?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let list = s.core.failed_change_list(None, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].subject, "Quarterly report");
    assert_eq!(list[0].kind, "import_raw");

    // A real worker run refuses a path outside the staging folder.
    assert!(s.core.retry_failed_change(id).await.unwrap());
    for _ in 0..3 {
        settle(&s.core, s.target).await;
    }
    assert!(files(&s.target_root.join("cur")).is_empty());
    assert!(
        outside.exists(),
        "the file outside the staging folder is left alone"
    );
}

#[tokio::test]
async fn a_backup_restores_on_another_profile_with_labels_snoozes_and_flags() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Maildir");
    make_maildir(&root);
    std::fs::write(
        root.join("cur/1700000001.M1P1.host:2,S"),
        plain("Read one", "r1", "already read"),
    )
    .unwrap();
    std::fs::write(
        root.join("new/1700000002.M2P1.host"),
        plain("Fresh one", "n1", "brand new"),
    )
    .unwrap();
    make_maildir(&root.join(".Projects.Alpha"));
    let alpha = with_attachment("Alpha plan", "a1");
    std::fs::write(
        root.join(".Projects.Alpha/cur/1700000003.M3P1.host:2,FS"),
        &alpha,
    )
    .unwrap();

    let core = start(temp.path(), "profile").await;
    let account = core
        .add_account_password(args("home@example.org", &root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let wake = rustmail_core::models::now_ms() + 3_600_000;
    let account_id = account.id;
    core.db
        .write(move |conn| {
            let (message, thread): (i64, i64) = conn.query_row(
                "SELECT id, thread_id FROM messages WHERE subject='Fresh one'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let label = rustmail_core::db::repo::labels::save(
                conn,
                None,
                "Keep",
                "#123456",
                1,
                Some(account_id),
            )?;
            rustmail_core::db::repo::labels::add_to_message(conn, message, label.id)?;
            rustmail_core::db::repo::snoozes::set(conn, thread, account_id, wake, None)?;
            Ok(())
        })
        .await
        .unwrap();

    // The destination must be new or empty and outside the mail folder.
    assert!(
        core.backup_local_account(account.id, root.join("backup"))
            .await
            .is_err()
    );
    let occupied = temp.path().join("occupied");
    std::fs::create_dir_all(&occupied).unwrap();
    std::fs::write(occupied.join("file"), "x").unwrap();
    assert!(
        core.backup_local_account(account.id, occupied)
            .await
            .is_err()
    );

    let backup = temp.path().join("backup");
    let report = core
        .backup_local_account(account.id, backup.clone())
        .await
        .unwrap();
    assert_eq!(report.files, 3, "{report:?}");
    assert_eq!(report.labelled_messages, 1);
    assert_eq!(report.snoozed_threads, 1);
    assert!(backup.join("rustmail-local-backup.json").is_file());
    let description = std::fs::read_to_string(backup.join("rustmail-local-backup.json")).unwrap();
    assert!(description.contains("home@example.org"));
    assert!(!description.contains(&root.to_string_lossy().into_owned()));
    assert_eq!(
        std::fs::read_to_string(backup.join("mail/.Projects.Alpha/cur/1700000003.M3P1.host:2,FS"))
            .unwrap(),
        alpha
    );

    // Another computer: a fresh profile.
    let other = start(temp.path(), "profile2").await;
    let target = temp.path().join("restored");
    assert!(
        other
            .restore_local_account(temp.path().join("occupied"), target.clone())
            .await
            .is_err(),
        "a folder that is no backup is refused"
    );
    let restored = other
        .restore_local_account(backup.clone(), target.clone())
        .await
        .unwrap();
    assert_eq!(restored.email, "home@example.org");
    settle(&other, restored.id).await;

    assert_eq!(files(&target.join("cur")), files(&root.join("cur")));
    assert_eq!(
        found(&other, restored.id, "Read one").await,
        vec![("INBOX".to_owned(), false, true, false)]
    );
    assert_eq!(
        found(&other, restored.id, "Fresh one").await,
        vec![("INBOX".to_owned(), false, false, false)]
    );
    let alpha_rows = found(&other, restored.id, "Alpha plan").await;
    assert_eq!(alpha_rows.len(), 1);
    assert!(alpha_rows[0].1, "the attachment came along");
    assert!(alpha_rows[0].3, "the star came along");

    let (labels, snoozed): (i64, Option<i64>) = other
        .db
        .read(|conn| {
            let labels = conn.query_row(
                "SELECT COUNT(*) FROM messages m JOIN message_labels ml ON ml.message_id=m.id
                 JOIN labels l ON l.id=ml.label_id
                 WHERE m.subject='Fresh one' AND l.name='Keep'",
                [],
                |r| r.get(0),
            )?;
            let snoozed = conn
                .query_row(
                    "SELECT s.wake_at FROM snoozes s JOIN messages m ON m.thread_id=s.thread_id
                     WHERE m.subject='Fresh one'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            Ok((labels, snoozed))
        })
        .await
        .unwrap();
    assert_eq!(labels, 1, "the label came along");
    assert_eq!(snoozed, Some(wake), "the snooze came along");

    // The same address cannot be restored twice, nor into a used folder.
    let again = other
        .restore_local_account(backup.clone(), temp.path().join("restored-2"))
        .await
        .unwrap_err();
    assert!(again.to_string().contains("already exists"), "{again}");
    assert!(!temp.path().join("restored-2").join("cur").exists());
    let third = start(temp.path(), "profile3").await;
    let used = third
        .restore_local_account(backup, target)
        .await
        .unwrap_err();
    assert!(used.to_string().contains("new or empty"), "{used}");
}

#[tokio::test]
async fn a_single_mbox_file_account_backs_up_and_restores() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("all.mbox");
    std::fs::write(
        &file,
        format!(
            "From a@example.org Mon Jan  1 10:00:00 2024\n{}\n",
            plain("In the file", "m1", "text").replace("\r\n", "\n")
        ),
    )
    .unwrap();
    let core = start(temp.path(), "profile").await;
    let account = core
        .add_account_password(args("box@example.org", &file, LocalFormat::Mbox))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let backup = temp.path().join("backup");
    let report = core
        .backup_local_account(account.id, backup.clone())
        .await
        .unwrap();
    assert_eq!(report.files, 1);
    let other = start(temp.path(), "profile2").await;
    let restored = other
        .restore_local_account(backup, temp.path().join("restored"))
        .await
        .unwrap();
    settle(&other, restored.id).await;
    assert_eq!(found(&other, restored.id, "In the file").await.len(), 1);
}
