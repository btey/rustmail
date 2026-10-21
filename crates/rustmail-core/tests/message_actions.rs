use rustmail_core::{Core, config::Paths, models::MessageAction};

async fn core_with_message(raw_path: Option<&str>) -> (tempfile::TempDir, Core) {
    let temp = tempfile::tempdir().unwrap();
    let core = Core::start(Paths::for_tests(temp.path())).await.unwrap();
    let raw = raw_path.map(str::to_owned);
    core.db
        .write(move |conn| {
            conn.execute_batch(
                "INSERT INTO accounts(id,email,provider,auth_kind,username,imap_host,imap_port,smtp_host,smtp_port,created_at) VALUES (1,'alice@example.org','imap','password','alice','127.0.0.1',1,'127.0.0.1',1,0);
                 INSERT INTO folders (id,account_id,imap_name,role) VALUES (1,1,'INBOX','inbox');
                 INSERT INTO threads (id,account_id,subject_norm,last_message_at,message_count) VALUES (1,1,'hello',1,1);",
            )?;
            conn.execute(
                "INSERT INTO messages (id,account_id,thread_id,folder_id,uid,subject,from_name,from_addr,date,is_read,body_state,raw_path)
                 VALUES (1,1,1,1,1,'Hello $(id)','Eve','eve@example.net',1,1,'cached',?1)",
                rusqlite::params![raw],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (temp, core)
}

fn action(name: &str, command: &str) -> MessageAction {
    MessageAction {
        id: String::new(),
        name: name.into(),
        command: command.into(),
    }
}

#[tokio::test]
async fn actions_and_the_editor_command_are_validated_and_persisted() {
    let (_temp, core) = core_with_message(None).await;
    assert!(
        core.save_message_actions(vec![action("  ", "true")])
            .await
            .is_err()
    );
    assert!(
        core.save_message_actions(vec![action("x", " ")])
            .await
            .is_err()
    );
    assert!(
        core.save_message_actions(vec![action("x", "a\0b")])
            .await
            .is_err()
    );
    assert!(
        core.save_message_actions((0..31).map(|i| action(&format!("a{i}"), "true")).collect())
            .await
            .is_err()
    );

    let saved = core
        .save_message_actions(vec![action(" Open ", " xdg-open %f ")])
        .await
        .unwrap();
    assert_eq!(saved[0].name, "Open");
    assert_eq!(saved[0].command, "xdg-open %f");
    assert!(saved[0].id.starts_with("act-"));
    assert_eq!(core.get_settings().await.unwrap().message_actions, saved);

    // The editor command must name the file to edit; empty switches it off.
    assert!(
        core.set_external_editor_command("vim".into())
            .await
            .is_err()
    );
    assert_eq!(
        core.set_external_editor_command(" foot -e nvim %f ".into())
            .await
            .unwrap(),
        "foot -e nvim %f"
    );
    assert_eq!(
        core.set_external_editor_command("".into()).await.unwrap(),
        ""
    );
    assert!(
        core.get_settings()
            .await
            .unwrap()
            .external_editor_command
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_action_gets_a_private_copy_and_the_message_details_in_its_environment() {
    let dir = tempfile::tempdir().unwrap();
    let eml = dir.path().join("original.eml");
    std::fs::write(&eml, "Subject: Hello\r\n\r\nbody\r\n").unwrap();
    let (_temp, core) = core_with_message(Some(eml.to_str().unwrap())).await;
    let saved = core
        .save_message_actions(vec![
            // Modifies and then deletes its copy, and prints what it was given.
            action(
                "inspect",
                "printf 'copy=%s subj=%s from=%s\\n' \"$(wc -c < %f)\" \"$RUSTMAIL_SUBJECT\" \"$RUSTMAIL_FROM\"; echo changed > %f; rm -f %f",
            ),
            action("fails", "echo broken >&2; exit 4"),
        ])
        .await
        .unwrap();

    let outcome = core
        .run_message_action(1, saved[0].id.clone())
        .await
        .unwrap();
    assert!(outcome.success, "{outcome:?}");
    assert_eq!(
        outcome.output,
        "copy=24 subj=Hello $(id) from=eve@example.net"
    );
    // The stored message is untouched, whatever the action did.
    assert_eq!(
        std::fs::read(&eml).unwrap(),
        b"Subject: Hello\r\n\r\nbody\r\n"
    );

    let failed = core
        .run_message_action(1, saved[1].id.clone())
        .await
        .unwrap();
    assert!(!failed.success);
    assert_eq!(failed.code, Some(4));
    assert_eq!(failed.output, "broken");

    assert!(
        core.run_message_action(1, "act-missing".into())
            .await
            .is_err()
    );
    assert!(
        core.run_message_action(99, saved[0].id.clone())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_message_without_a_local_copy_is_fetched_from_the_server_or_fails_cleanly() {
    // The account points at a closed local port, so the on-demand download
    // fails quickly instead of the action silently doing nothing.
    let (_temp, core) = core_with_message(None).await;
    let saved = core
        .save_message_actions(vec![action("a", "true")])
        .await
        .unwrap();
    let error = core
        .run_message_action(1, saved[0].id.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("Open it first"), "{error}");
    // Without a reachable credential store the download cannot even start;
    // with one, the missing password or the closed port is reported.
    let lower = error.to_lowercase();
    assert!(
        lower.contains("connect") || lower.contains("credential"),
        "{error}"
    );
    assert!(core.original_message(99).await.is_err());
}

#[tokio::test]
async fn a_conversation_resolves_to_its_newest_real_message() {
    let (_temp, core) = core_with_message(None).await;
    core.db
        .write(|conn| {
            for (id, date, draft) in [(2, 50, 0), (3, 90, 0), (4, 99, 1)] {
                conn.execute(
                    "INSERT INTO messages (id,account_id,thread_id,folder_id,uid,subject,from_addr,date,is_draft,body_state)
                     VALUES (?1,1,1,1,?1,'Reply','bob@example.org',?2,?3,'cached')",
                    rusqlite::params![id, date, draft],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
    // Message 1 has date 1; the draft (99) never wins; the newest real one does.
    assert_eq!(core.latest_message_in_thread(1).await.unwrap(), 3);
    assert!(core.latest_message_in_thread(404).await.is_err());
}
