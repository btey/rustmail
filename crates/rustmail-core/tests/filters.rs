use rustmail_core::{
    Core,
    config::Paths,
    models::{FilterAction, FilterCondition, FilterField, FilterOp, FilterRule},
};

fn rule(name: &str, from: &str, actions: &[(&str, &str)], stop: bool) -> FilterRule {
    FilterRule {
        id: String::new(),
        name: name.into(),
        enabled: true,
        match_all: true,
        conditions: vec![FilterCondition {
            field: FilterField::From,
            op: FilterOp::Contains,
            value: from.into(),
            name: String::new(),
        }],
        actions: actions
            .iter()
            .map(|(k, v)| FilterAction {
                kind: (*k).into(),
                value: (*v).into(),
            })
            .collect(),
        stop,
    }
}

async fn setup(rules: Vec<FilterRule>) -> (tempfile::TempDir, Core) {
    let temp = tempfile::tempdir().unwrap();
    let core = Core::start(Paths::for_tests(temp.path())).await.unwrap();
    // Rules exist before the mail arrives, as in real use.
    if !rules.is_empty() {
        core.save_filter_rules(rules).await.unwrap();
    }
    core.db
        .write(|conn| {
            conn.execute_batch(
                "INSERT INTO accounts(id,email,provider,auth_kind,username,imap_host,imap_port,smtp_host,smtp_port,created_at) VALUES (1,'alice@example.org','imap','password','alice','imap.example.org',993,'smtp.example.org',465,0);
                 INSERT INTO folders (id,account_id,imap_name,role,selectable,delimiter) VALUES (1,1,'INBOX','inbox',1,'/'),(2,1,'Lists/Receipts','',1,'/'),(3,1,'Sent','sent',1,'/');
                 INSERT INTO threads (id,account_id,subject_norm,last_message_at,message_count) VALUES (1,1,'a',1,1),(2,1,'b',2,1),(3,1,'c',3,1);",
            )?;
            for (id, thread, folder, from, outgoing) in [
                (1, 1, 1, "eve@example.net", 0),
                (2, 2, 1, "bob@example.org", 0),
                (3, 3, 3, "eve@example.net", 1),
            ] {
                conn.execute(
                    "INSERT INTO messages (id,account_id,thread_id,folder_id,uid,subject,from_name,from_addr,date,is_read,is_outgoing,body_state)
                     VALUES (?1,1,?2,?3,?1,'Hello',NULL,?4,?1,0,?5,'cached')",
                    rusqlite::params![id, thread, folder, from, outgoing],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    (temp, core)
}

async fn read_flags(core: &Core) -> Vec<(i64, bool, bool, i64)> {
    core.db
        .read(|conn| {
            let mut s = conn
                .prepare("SELECT id, is_read, is_starred, folder_id FROM messages ORDER BY id")?;
            Ok(s.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, i64>(2)? != 0,
                    r.get(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn new_inbox_mail_is_filtered_once_and_other_mail_is_left_alone() {
    let (_temp, core) = setup(vec![rule(
        "Eve",
        "eve@",
        &[("mark_read", ""), ("star", "")],
        false,
    )])
    .await;
    // The background loop may get to the new mail first; either way it is
    // filtered exactly once.
    core.apply_pending_filters().await.unwrap();
    let flags = read_flags(&core).await;
    assert_eq!(flags[0], (1, true, true, 1), "the matching inbox message");
    assert_eq!(flags[1], (2, false, false, 1), "a non-matching message");
    assert_eq!(
        flags[2],
        (3, false, false, 3),
        "outgoing mail is never filtered"
    );
    assert_eq!(
        core.apply_pending_filters().await.unwrap(),
        0,
        "handled mail is skipped"
    );
}

#[tokio::test]
async fn running_filters_on_a_folder_moves_mail_by_folder_name() {
    let (_temp, core) = setup(Vec::new()).await;
    core.save_filter_rules(vec![
        rule("Bob", "bob@", &[("move", "receipts")], true),
        rule("Never reached", "bob@", &[("star", "")], false),
        rule(
            "Missing folder",
            "eve@",
            &[("move", "No such folder")],
            false,
        ),
    ])
    .await
    .unwrap();
    let summary = core.run_filters_now(1).await.unwrap();
    assert_eq!((summary.examined, summary.matched), (2, 2));
    let flags = read_flags(&core).await;
    assert_eq!(
        flags[1].3, 2,
        "moved to Lists/Receipts by its last path segment"
    );
    assert!(
        !flags[1].2,
        "a stopped rule list never reaches the later rule"
    );
    assert_eq!(
        flags[0].3, 1,
        "an unknown target folder is skipped, not an error"
    );
}

#[tokio::test]
async fn invalid_rule_lists_are_rejected_without_changing_the_saved_ones() {
    let (_temp, core) = setup(Vec::new()).await;
    core.save_filter_rules(vec![rule("Keep", "x", &[("star", "")], false)])
        .await
        .unwrap();
    let mut bad = rule("Bad", "x", &[("trash", "")], false);
    bad.conditions[0].op = FilterOp::Regex;
    bad.conditions[0].value = "(".into();
    assert!(core.save_filter_rules(vec![bad]).await.is_err());
    let settings = core.get_settings().await.unwrap();
    assert_eq!(settings.filter_rules.len(), 1);
    assert_eq!(settings.filter_rules[0].name, "Keep");
}

fn condition(field: FilterField, op: FilterOp, name: &str, value: &str) -> FilterCondition {
    FilterCondition {
        field,
        op,
        value: value.into(),
        name: name.into(),
    }
}

#[tokio::test]
async fn header_body_age_and_flag_conditions_with_a_command_action() {
    let (temp, core) = setup(Vec::new()).await;
    let eml = temp.path().join("one.eml");
    std::fs::write(
        &eml,
        "From: eve@example.net\r\nSubject: Hello\r\nX-Test: Yes\r\n\r\nthe quick brown fox\r\n",
    )
    .unwrap();
    let eml_text = eml.to_string_lossy().into_owned();
    core.db
        .write(move |conn| {
            conn.execute(
                "UPDATE messages SET raw_path = ?1 WHERE id = 1",
                rusqlite::params![eml_text],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let out = temp.path().join("copied.eml");
    let mut r = rule(
        "Everything",
        "eve@",
        &[
            ("run_command", &format!("cp %f '{}'", out.display())),
            ("star", ""),
        ],
        false,
    );
    r.conditions = vec![
        condition(FilterField::Header, FilterOp::Equals, "x-test", "yes"),
        condition(FilterField::Body, FilterOp::Contains, "", "brown fox"),
        condition(FilterField::Age, FilterOp::Greater, "", "30"),
        condition(FilterField::Read, FilterOp::No, "", ""),
        condition(FilterField::Starred, FilterOp::No, "", ""),
    ];
    core.save_filter_rules(vec![r]).await.unwrap();
    let summary = core.run_filters_now(1).await.unwrap();
    assert_eq!((summary.examined, summary.matched), (2, 1));
    let copied = std::fs::read_to_string(&out).expect("the command ran on a copy of the message");
    assert!(copied.contains("the quick brown fox"));
    let flags = read_flags(&core).await;
    assert!(flags[0].2, "the thread action ran too");
    assert!(!flags[1].2, "message 2 has no original, so it cannot match");
}

#[tokio::test]
async fn copy_and_forward_are_refused_safely() {
    let (_temp, core) = setup(Vec::new()).await;
    let mut r = rule(
        "Copy",
        "eve@",
        &[("copy", "INBOX"), ("forward", "alice@example.org")],
        false,
    );
    r.conditions[0].value = "eve@".into();
    core.save_filter_rules(vec![r]).await.unwrap();
    // The account has no stored original and no server: both actions fail,
    // the failure is logged, and the run still finishes.
    let summary = core.run_filters_now(1).await.unwrap();
    assert_eq!(summary.matched, 1);
    let mut bad = rule("Bad", "x", &[("forward", "not an address")], false);
    assert!(core.save_filter_rules(vec![bad.clone()]).await.is_err());
    bad.actions = vec![FilterAction {
        kind: "copy".into(),
        value: String::new(),
    }];
    assert!(core.save_filter_rules(vec![bad]).await.is_err());
}

#[tokio::test]
async fn the_create_task_action_makes_one_linked_task_per_conversation() {
    // No rules while the mail is set up, so the background filter loop has
    // nothing to race with; the rules are saved and run explicitly after.
    let (_temp, core) = setup(vec![]).await;
    core.db
        .write(|conn| {
            conn.execute("UPDATE messages SET message_id = '<m' || id || '@x>'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    core.save_filter_rules(vec![rule(
        "Eve",
        "eve@",
        &[("create_task", "waiting")],
        false,
    )])
    .await
    .unwrap();
    core.run_filters_now(1).await.unwrap();
    let linked = core.task_threads(None).await.unwrap();
    assert_eq!(linked.len(), 1, "only the matching conversation");
    assert_eq!(linked[0].thread_id, 1);
    assert_eq!(linked[0].status, "Waiting for");
    assert!(linked[0].waiting_on_reply);

    // Running the filters again must not add a second task.
    core.run_filters_now(1).await.unwrap();
    let tasks = core
        .task_list(rustmail_core::tasks::TaskFilter::default())
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Hello");

    // A bad value is rejected when the rules are saved.
    let bad = rule("Bad", "eve@", &[("create_task", "someday-maybe")], false);
    assert!(core.save_filter_rules(vec![bad]).await.is_err());
    let none = rule("None", "eve@", &[("create_task", "")], false);
    assert!(core.save_filter_rules(vec![none]).await.is_err());
}
