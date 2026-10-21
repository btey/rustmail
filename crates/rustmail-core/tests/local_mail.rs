//! Local accounts: Maildir import, flag/move/delete write-back, rescans, mbox.

use rustmail_core::{
    Core,
    config::Paths,
    models::{
        ActionKind, AddPasswordAccountArgs, LocalFormat, MailConnectionSettings, MailProtocol,
        PerformActionArgs,
    },
};
use std::path::{Path, PathBuf};

fn message(subject: &str, id: &str, body: &str) -> String {
    format!(
        "From: Alice <alice@example.org>\r\nTo: me@example.org\r\nSubject: {subject}\r\nMessage-ID: <{id}@example.org>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\r\n{body}\r\n"
    )
}

fn make_maildir(dir: &Path) {
    for sub in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
}

fn args(path: &Path, format: LocalFormat) -> AddPasswordAccountArgs {
    AddPasswordAccountArgs {
        email: "me@example.org".into(),
        display_name: Some("Me".into()),
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

async fn start(temp: &Path) -> Core {
    Core::start(Paths::for_tests(&temp.join("profile")))
        .await
        .unwrap()
}

async fn settle(core: &Core, account_id: i64) {
    // The first cycle starts on spawn; a second request waits for it to finish.
    core.sync_now(Some(account_id)).await.unwrap();
    core.sync_now(Some(account_id)).await.unwrap();
}

#[derive(Debug, Clone)]
struct Row {
    thread_id: i64,
    subject: String,
    is_read: bool,
    is_starred: bool,
    folder: String,
    body: Option<String>,
}

async fn rows(core: &Core) -> Vec<Row> {
    core.db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.thread_id, m.subject, m.is_read, m.is_starred, f.imap_name,
                        b.text_body
                 FROM messages m JOIN folders f ON f.id = m.folder_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 ORDER BY m.subject",
            )?;
            Ok(stmt
                .query_map([], |r| {
                    Ok(Row {
                        thread_id: r.get(1)?,
                        subject: r.get(2)?,
                        is_read: r.get::<_, i64>(3)? != 0,
                        is_starred: r.get::<_, i64>(4)? != 0,
                        folder: r.get(5)?,
                        body: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
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

fn maildir_fixture(temp: &Path) -> PathBuf {
    let root = temp.join("Maildir");
    make_maildir(&root);
    std::fs::write(
        root.join("cur/1700000001.M1P1.host:2,S"),
        message("Read one", "r1", "already read"),
    )
    .unwrap();
    std::fs::write(
        root.join("new/1700000002.M2P1.host"),
        message("Fresh one", "n1", "brand new"),
    )
    .unwrap();
    // Maildir++ sub-folder.
    make_maildir(&root.join(".Projects.Alpha"));
    std::fs::write(
        root.join(".Projects.Alpha/cur/1700000003.M3P1.host:2,FS"),
        message("Alpha plan", "a1", "starred"),
    )
    .unwrap();
    root
}

#[tokio::test]
async fn maildir_import_reads_flags_folders_and_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    assert_eq!(account.mail_protocol, MailProtocol::Local);
    settle(&core, account.id).await;

    let all = rows(&core).await;
    let by = |s: &str| all.iter().find(|r| r.subject == s).unwrap().clone();
    assert_eq!(all.len(), 3);
    assert!(by("Read one").is_read);
    assert!(!by("Fresh one").is_read);
    assert_eq!(by("Fresh one").folder, "INBOX");
    assert_eq!(
        by("Fresh one").body.as_deref().map(str::trim),
        Some("brand new")
    );
    assert!(by("Alpha plan").is_starred && by("Alpha plan").is_read);
    assert_eq!(by("Alpha plan").folder, "Projects/Alpha");

    // Trash, Sent and Drafts are created so moves and sent copies have a home.
    let roles: Vec<String> = core
        .db
        .read(|conn| {
            let mut stmt = conn.prepare("SELECT role FROM folders WHERE role IS NOT NULL")?;
            Ok(stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap();
    for role in ["inbox", "trash", "sent", "drafts"] {
        assert!(roles.iter().any(|r| r == role), "missing {role}: {roles:?}");
    }
    assert!(root.join(".Trash/cur").is_dir());
}

#[tokio::test]
async fn flag_changes_are_written_back_by_renaming_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let fresh = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Fresh one")
        .unwrap();

    core.perform_action(PerformActionArgs {
        kind: ActionKind::MarkRead,
        thread_ids: vec![fresh.thread_id],
        params: None,
    })
    .await
    .unwrap();
    settle(&core, account.id).await;
    assert!(files(&root.join("new")).is_empty(), "message left new/");
    assert!(
        files(&root.join("cur")).contains(&"1700000002.M2P1.host:2,S".to_string()),
        "{:?}",
        files(&root.join("cur"))
    );

    core.perform_action(PerformActionArgs {
        kind: ActionKind::Star,
        thread_ids: vec![fresh.thread_id],
        params: None,
    })
    .await
    .unwrap();
    core.perform_action(PerformActionArgs {
        kind: ActionKind::MarkUnread,
        thread_ids: vec![fresh.thread_id],
        params: None,
    })
    .await
    .unwrap();
    settle(&core, account.id).await;
    assert!(
        files(&root.join("cur")).contains(&"1700000002.M2P1.host:2,F".to_string()),
        "{:?}",
        files(&root.join("cur"))
    );
    // A rescan keeps the app state consistent with the file.
    let again = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Fresh one")
        .unwrap();
    assert!(again.is_starred && !again.is_read);
}

#[tokio::test]
async fn trash_moves_the_file_and_delete_removes_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let target = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Read one")
        .unwrap();

    core.perform_action(PerformActionArgs {
        kind: ActionKind::Trash,
        thread_ids: vec![target.thread_id],
        params: None,
    })
    .await
    .unwrap();
    settle(&core, account.id).await;
    assert_eq!(
        files(&root.join(".Trash/cur")),
        vec!["1700000001.M1P1.host:2,S"]
    );
    assert_eq!(
        rows(&core)
            .await
            .into_iter()
            .find(|r| r.subject == "Read one")
            .unwrap()
            .folder,
        "Trash"
    );

    core.perform_action(PerformActionArgs {
        kind: ActionKind::DeletePermanently,
        thread_ids: vec![target.thread_id],
        params: None,
    })
    .await
    .unwrap();
    settle(&core, account.id).await;
    assert!(files(&root.join(".Trash/cur")).is_empty());
    assert!(rows(&core).await.iter().all(|r| r.subject != "Read one"));
}

#[tokio::test]
async fn rescans_pick_up_new_removed_and_externally_changed_mail() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 3);

    // Another program delivers mail, removes mail and marks mail read.
    std::fs::write(
        root.join("new/1700000009.M9P1.host"),
        message("Late arrival", "l1", "delivered later"),
    )
    .unwrap();
    std::fs::remove_file(root.join("cur/1700000001.M1P1.host:2,S")).unwrap();
    std::fs::rename(
        root.join("new/1700000002.M2P1.host"),
        root.join("cur/1700000002.M2P1.host:2,S"),
    )
    .unwrap();
    settle(&core, account.id).await;

    let all = rows(&core).await;
    assert!(
        all.iter()
            .any(|r| r.subject == "Late arrival" && !r.is_read)
    );
    assert!(all.iter().all(|r| r.subject != "Read one"));
    assert!(
        all.iter()
            .find(|r| r.subject == "Fresh one")
            .unwrap()
            .is_read
    );
}

fn mbox_text(entries: &[(&str, &str, &str)]) -> String {
    entries
        .iter()
        .map(|(subject, id, body)| {
            format!(
                "From a@example.org Mon Jan  1 10:00:00 2024\n{}\n",
                message(subject, id, body).replace("\r\n", "\n").trim_end()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn act(core: &Core, kind: ActionKind, thread_id: i64) {
    core.perform_action(PerformActionArgs {
        kind,
        thread_ids: vec![thread_id],
        params: None,
    })
    .await
    .unwrap();
}

fn mbox_subjects(path: &Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap();
    rustmail_core::local::mbox::parse(&bytes)
        .iter()
        .map(|m| {
            String::from_utf8_lossy(&m.raw)
                .lines()
                .find_map(|l| l.strip_prefix("Subject: "))
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn mbox_import_unescapes_from_lines() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("inbox.mbox");
    let mbox = format!(
        "From a@example.org Mon Jan  1 10:00:00 2024\n{}\n>From the quoted line\nend\n\nFrom b@example.org Mon Jan  1 11:00:00 2024\n{}\n",
        message("First", "m1", "hello")
            .replace("\r\n", "\n")
            .trim_end(),
        message("Second", "m2", "bye")
            .replace("\r\n", "\n")
            .trim_end(),
    );
    std::fs::write(&file, &mbox).unwrap();
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&file, LocalFormat::Mbox))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let all = rows(&core).await;
    assert_eq!(all.len(), 2);
    let first = all.iter().find(|r| r.subject == "First").unwrap();
    assert!(
        first
            .body
            .as_deref()
            .unwrap()
            .contains("From the quoted line")
    );
    assert_eq!(first.folder, "INBOX");
}

#[tokio::test]
async fn mbox_flags_are_written_to_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("inbox.mbox");
    std::fs::write(
        &file,
        mbox_text(&[
            ("One", "m1", "first"),
            ("Two", "m2", "second"),
            ("Three", "m3", "third"),
        ]),
    )
    .unwrap();
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&file, LocalFormat::Mbox))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let find =
        |all: &[Row], subject: &str| all.iter().find(|r| r.subject == subject).cloned().unwrap();
    let all = rows(&core).await;

    // Changing the first message shifts every later offset; later actions
    // must still find their messages.
    act(&core, ActionKind::MarkRead, find(&all, "One").thread_id).await;
    act(&core, ActionKind::Star, find(&all, "Two").thread_id).await;
    act(&core, ActionKind::MarkRead, find(&all, "Three").thread_id).await;
    settle(&core, account.id).await;

    let bytes = std::fs::read(&file).unwrap();
    let parsed = rustmail_core::local::mbox::parse(&bytes);
    assert_eq!(parsed.len(), 3);
    assert!(parsed[0].flags.seen && !parsed[0].flags.flagged);
    assert!(!parsed[1].flags.seen && parsed[1].flags.flagged);
    assert!(parsed[2].flags.seen);
    let after = rows(&core).await;
    assert_eq!(after.len(), 3);
    assert!(find(&after, "One").is_read && find(&after, "Two").is_starred);

    act(&core, ActionKind::MarkUnread, find(&all, "One").thread_id).await;
    act(&core, ActionKind::Unstar, find(&all, "Two").thread_id).await;
    settle(&core, account.id).await;
    let parsed = rustmail_core::local::mbox::parse(&std::fs::read(&file).unwrap());
    assert!(!parsed[0].flags.seen && !parsed[1].flags.flagged);

    // A single file has no Trash folder to move into, and only mail in
    // Trash can be deleted, so nothing leaves the file.
    let error = core
        .perform_action(PerformActionArgs {
            kind: ActionKind::Trash,
            thread_ids: vec![find(&all, "Two").thread_id],
            params: None,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("trash"), "{error}");
    assert_eq!(mbox_subjects(&file), ["One", "Two", "Three"]);
    assert_eq!(rows(&core).await.len(), 3);
}

#[tokio::test]
async fn mbox_trash_moves_between_files_and_delete_removes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mail");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("inbox"),
        mbox_text(&[
            ("One", "m1", "first"),
            ("Two", "m2", "second"),
            ("Three", "m3", "third"),
        ]),
    )
    .unwrap();
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mbox))
        .await
        .unwrap();
    settle(&core, account.id).await;
    // Standard folders are created as empty mbox files.
    assert!(root.join("Trash").is_file());
    let all = rows(&core).await;
    assert_eq!(all.len(), 3);
    let thread = |subject: &str| all.iter().find(|r| r.subject == subject).unwrap().thread_id;

    act(&core, ActionKind::Trash, thread("One")).await;
    settle(&core, account.id).await;
    assert_eq!(mbox_subjects(&root.join("inbox")), ["Two", "Three"]);
    assert_eq!(mbox_subjects(&root.join("Trash")), ["One"]);
    let after = rows(&core).await;
    assert_eq!(after.len(), 3, "a rescan must not duplicate or drop mail");
    assert_eq!(
        after.iter().find(|r| r.subject == "One").unwrap().folder,
        "Trash"
    );

    act(&core, ActionKind::MarkRead, thread("Three")).await;
    act(&core, ActionKind::DeletePermanently, thread("One")).await;
    settle(&core, account.id).await;
    assert!(mbox_subjects(&root.join("Trash")).is_empty());
    assert_eq!(mbox_subjects(&root.join("inbox")), ["Two", "Three"]);
    let parsed = rustmail_core::local::mbox::parse(&std::fs::read(root.join("inbox")).unwrap());
    assert!(!parsed[0].flags.seen && parsed[1].flags.seen);
    assert_eq!(rows(&core).await.len(), 2);
}

#[tokio::test]
async fn invalid_roots_are_rejected_before_anything_is_stored() {
    let temp = tempfile::tempdir().unwrap();
    let core = start(temp.path()).await;
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let error = core
        .add_account_password(args(&empty, LocalFormat::Maildir))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no Maildir"), "{error}");
    let error = core
        .add_account_password(args(&temp.path().join("missing"), LocalFormat::Maildir))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cannot open"), "{error}");
    assert!(core.list_accounts().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_never_followed_outside_the_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    // A secret Maildir outside the account root, linked from inside it.
    let outside = temp.path().join("outside");
    make_maildir(&outside);
    std::fs::write(
        outside.join("cur/777.M1P1.host:2,S"),
        message("Outside secret", "o1", "must not appear"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, root.join(".Linked")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("cur/777.M1P1.host:2,S"),
        root.join("cur/888.M1P1.host:2,S"),
    )
    .unwrap();

    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert!(
        rows(&core)
            .await
            .iter()
            .all(|r| r.subject != "Outside secret")
    );
}

#[tokio::test]
async fn sent_copies_are_filed_in_the_sent_maildir() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;

    // What the SMTP worker leaves behind after a successful send.
    let raw = temp.path().join("outgoing.eml");
    std::fs::write(&raw, message("I sent this", "s1", "outbound")).unwrap();
    let raw_path = raw.to_string_lossy().into_owned();
    let account_id = account.id;
    core.db
        .write(move |conn| {
            let sent: i64 = conn.query_row(
                "SELECT id FROM folders WHERE account_id=?1 AND role='sent'",
                [account_id],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT INTO threads(id,account_id,subject_norm) VALUES (900,?1,'i sent this')",
                [account_id],
            )?;
            conn.execute(
                "INSERT INTO messages(id,account_id,thread_id,folder_id,subject,date,is_read,is_outgoing,body_state,raw_path)
                 VALUES (900,?1,900,?2,'I sent this',1,1,1,'cached',?3)",
                rusqlite::params![account_id, sent, raw_path],
            )?;
            rustmail_core::db::repo::actions::enqueue(
                conn,
                account_id,
                "append_sent",
                Some(900),
                Some(900),
                &serde_json::json!({}),
                None,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    settle(&core, account.id).await;
    let filed = files(&root.join(".Sent/cur"));
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert!(filed[0].ends_with(":2,S"));
    // The rescan recognises the file as the same message (no duplicate).
    settle(&core, account.id).await;
    let count = rows(&core)
        .await
        .iter()
        .filter(|r| r.subject == "I sent this")
        .count();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn editing_a_local_account_keeps_its_mail_and_updates_settings() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;

    let mut changed = args(&root, LocalFormat::Maildir);
    changed.smtp_host = "smtp.example.org".into();
    changed.smtp_port = 587;
    changed.display_name = Some("Renamed".into());
    let again = core.add_account_password(changed).await.unwrap();
    assert_eq!(again.id, account.id);
    settle(&core, account.id).await;
    assert_eq!(rows(&core).await.len(), 3);
    let config = core
        .db
        .read(move |conn| rustmail_core::db::repo::accounts::get_config(conn, again.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(config.smtp_host, "smtp.example.org");
    assert_eq!(config.mail_protocol, MailProtocol::Local);
}

#[tokio::test]
async fn labels_and_snoozes_survive_a_message_moved_by_another_program() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;

    let wake = rustmail_core::models::now_ms() + 3_600_000;
    core.db
        .write(move |conn| {
            let (message, thread): (i64, i64) = conn.query_row(
                "SELECT id, thread_id FROM messages WHERE subject='Fresh one'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let label =
                rustmail_core::db::repo::labels::save(conn, None, "Keep", "#123456", 1, None)?;
            rustmail_core::db::repo::labels::add_to_message(conn, message, label.id)?;
            rustmail_core::db::repo::snoozes::set(conn, thread, 1, wake, None)?;
            Ok(())
        })
        .await
        .unwrap();

    // Another program moves the message into a Maildir++ sub-folder.
    make_maildir(&root.join(".Elsewhere"));
    std::fs::rename(
        root.join("new/1700000002.M2P1.host"),
        root.join(".Elsewhere/cur/1700000002.M2P1.host:2,"),
    )
    .unwrap();
    settle(&core, account.id).await;

    let (labels, snoozed): (i64, Option<i64>) = core
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
    assert_eq!(labels, 1);
    assert_eq!(snoozed, Some(wake));
}

#[tokio::test]
async fn new_local_mail_notifies_and_sent_mail_teaches_contacts() {
    let temp = tempfile::tempdir().unwrap();
    let root = maildir_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Maildir))
        .await
        .unwrap();
    settle(&core, account.id).await;

    let count = |sql: &'static str| {
        let core = core.clone();
        async move {
            core.db
                .read(move |conn| Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?))
                .await
                .unwrap()
        }
    };
    // The catch-up import of existing mail stays silent.
    assert_eq!(count("SELECT COUNT(*) FROM notification_outbox").await, 0);

    // Mail delivered later notifies once; already-read and Sent mail do not.
    std::fs::write(
        root.join("new/1700000010.M10P1.host"),
        message("Just arrived", "n1", "hello"),
    )
    .unwrap();
    std::fs::write(
        root.join("cur/1700000011.M11P1.host:2,S"),
        message("Read on arrival", "n2", "seen"),
    )
    .unwrap();
    let sent = root.join(".Sent");
    assert!(sent.join("cur").is_dir());
    std::fs::write(
        sent.join("cur/1700000012.M12P1.host:2,S"),
        "From: me@example.org\r\nTo: Bob <bob@example.net>\r\nSubject: Outbound\r\nMessage-ID: <o1@example.org>\r\nDate: Mon, 01 Jan 2024 10:00:00 +0000\r\n\r\nhi\r\n",
    )
    .unwrap();
    settle(&core, account.id).await;

    assert_eq!(count("SELECT COUNT(*) FROM notification_outbox").await, 1);
    assert_eq!(
        count("SELECT COUNT(*) FROM notification_outbox n JOIN messages m ON m.id=n.message_id WHERE m.subject='Just arrived'").await,
        1
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM contacts WHERE email='bob@example.net'").await,
        1
    );
}
