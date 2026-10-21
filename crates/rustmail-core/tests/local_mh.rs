//! Local accounts in MH format: import, flag/move/delete write-back, sent copies, safety.

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

fn mh_fixture(temp: &Path) -> PathBuf {
    let root = temp.join("Mail");
    std::fs::create_dir_all(root.join("inbox")).unwrap();
    std::fs::write(
        root.join("inbox/1"),
        message("Read one", "r1", "already read"),
    )
    .unwrap();
    std::fs::write(
        root.join("inbox/2"),
        message("Fresh one", "n1", "brand new"),
    )
    .unwrap();
    std::fs::write(root.join("inbox/.mh_sequences"), "unseen: 2\nflagged: 1\n").unwrap();
    std::fs::create_dir_all(root.join("Lists/rust")).unwrap();
    std::fs::write(
        root.join("Lists/rust/5"),
        message("Rust list", "l1", "hello"),
    )
    .unwrap();
    root
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

#[tokio::test]
async fn mh_import_reads_sequences_and_nested_folders() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let all = rows(&core).await;
    assert_eq!(all.len(), 3);
    let read = all.iter().find(|r| r.subject == "Read one").unwrap();
    assert!(read.is_read && read.is_starred && read.folder == "inbox");
    assert_eq!(read.body.as_deref().map(str::trim), Some("already read"));
    let fresh = all.iter().find(|r| r.subject == "Fresh one").unwrap();
    assert!(!fresh.is_read && !fresh.is_starred);
    assert_eq!(
        all.iter()
            .find(|r| r.subject == "Rust list")
            .unwrap()
            .folder,
        "Lists/rust"
    );
    // Standard folders are created so moves and sent copies have a target.
    assert!(root.join("Trash").is_dir() && root.join("Sent").is_dir());
}

#[tokio::test]
async fn mh_flag_changes_are_written_to_mh_sequences() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let fresh = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Fresh one")
        .unwrap();
    act(&core, ActionKind::MarkRead, fresh.thread_id).await;
    act(&core, ActionKind::Star, fresh.thread_id).await;
    settle(&core, account.id).await;
    let sequences = std::fs::read_to_string(root.join("inbox/.mh_sequences")).unwrap();
    assert!(!sequences.contains("unseen"), "{sequences}");
    assert!(sequences.contains("flagged: 1-2"), "{sequences}");
    let again = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Fresh one")
        .unwrap();
    assert!(again.is_read && again.is_starred);
}

#[tokio::test]
async fn mh_trash_moves_the_file_and_delete_removes_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
    let target = rows(&core)
        .await
        .into_iter()
        .find(|r| r.subject == "Read one")
        .unwrap();
    act(&core, ActionKind::Trash, target.thread_id).await;
    settle(&core, account.id).await;
    assert_eq!(files(&root.join("Trash")), vec![".mh_sequences", "1"]);
    assert_eq!(files(&root.join("inbox")), vec![".mh_sequences", "2"]);
    assert_eq!(
        rows(&core)
            .await
            .into_iter()
            .find(|r| r.subject == "Read one")
            .unwrap()
            .folder,
        "Trash"
    );
    act(&core, ActionKind::DeletePermanently, target.thread_id).await;
    settle(&core, account.id).await;
    assert_eq!(files(&root.join("Trash")), vec![".mh_sequences"]);
    assert!(rows(&core).await.iter().all(|r| r.subject != "Read one"));
}

#[tokio::test]
async fn mh_rescan_picks_up_new_and_removed_mail() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
    std::fs::write(root.join("inbox/3"), message("Late arrival", "x1", "later")).unwrap();
    std::fs::remove_file(root.join("inbox/1")).unwrap();
    settle(&core, account.id).await;
    let all = rows(&core).await;
    assert!(all.iter().any(|r| r.subject == "Late arrival"));
    assert!(all.iter().all(|r| r.subject != "Read one"));
}

#[tokio::test]
async fn mh_invalid_roots_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let core = start(temp.path()).await;
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let error = core
        .add_account_password(args(&empty, LocalFormat::Mh))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no MH folders"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn mh_symlinks_are_never_followed_outside_the_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("1"), message("Outside secret", "o1", "no")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("Linked")).unwrap();
    std::os::unix::fs::symlink(outside.join("1"), root.join("inbox/9")).unwrap();
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
    assert!(
        rows(&core)
            .await
            .iter()
            .all(|r| r.subject != "Outside secret")
    );
    assert_eq!(files(&outside), vec!["1"]);
}

#[tokio::test]
async fn mh_sent_copies_are_filed_in_the_sent_folder() {
    let temp = tempfile::tempdir().unwrap();
    let root = mh_fixture(temp.path());
    let core = start(temp.path()).await;
    let account = core
        .add_account_password(args(&root, LocalFormat::Mh))
        .await
        .unwrap();
    settle(&core, account.id).await;
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
    assert_eq!(files(&root.join("Sent")), vec![".mh_sequences", "1"]);
    settle(&core, account.id).await;
    let count = rows(&core)
        .await
        .iter()
        .filter(|r| r.subject == "I sent this")
        .count();
    assert_eq!(count, 1);
}
