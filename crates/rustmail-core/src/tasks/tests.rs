use super::*;
use crate::db::testutil;
use rusqlite::Connection;

fn setup() -> (Connection, i64) {
    let mut c = testutil::conn();
    let mut s = SqliteTaskStore::new(&mut c);
    s.ensure_default_project().unwrap();
    let p = s.list_projects().unwrap()[0].id;
    (c, p)
}

fn col(c: &mut Connection, p: i64, kind: ColumnKind) -> TaskColumn {
    SqliteTaskStore::new(c)
        .list_columns(p)
        .unwrap()
        .into_iter()
        .find(|x| x.kind == kind)
        .unwrap()
}

fn task(c: &mut Connection, p: i64, title: &str) -> Task {
    SqliteTaskStore::new(c)
        .create_task(NewTask {
            project_id: p,
            title: title.into(),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn seeds_once_with_default_columns_and_no_sample_tasks() {
    let (mut c, p) = setup();
    let mut s = SqliteTaskStore::new(&mut c);
    s.ensure_default_project().unwrap();
    assert_eq!(s.list_projects().unwrap().len(), 1);
    let kinds: Vec<_> = s.list_columns(p).unwrap().iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            ColumnKind::Inbox,
            ColumnKind::Next,
            ColumnKind::Waiting,
            ColumnKind::Scheduled,
            ColumnKind::Done,
            ColumnKind::Someday
        ]
    );
    assert!(s.list_tasks(&TaskFilter::default()).unwrap().is_empty());
    // Deleting everything must not re-seed.
    s.delete_project(p).unwrap();
    s.ensure_default_project().unwrap();
    assert!(s.list_projects().unwrap().is_empty());
}

#[test]
fn crud_and_validation() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "  Write report ");
    assert_eq!(t.title, "Write report");
    assert_eq!(t.column_id, col(&mut c, p, ColumnKind::Inbox).id);
    let mut s = SqliteTaskStore::new(&mut c);
    assert!(
        s.create_task(NewTask {
            project_id: p,
            title: "  ".into(),
            ..Default::default()
        })
        .is_err()
    );
    let u = s
        .update_task(
            t.id,
            TaskPatch {
                due_at: Some(Some(5)),
                priority: Some(2),
                assignee: Some(Some(("a@x.dev".into(), "Ann".into()))),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!((u.due_at, u.priority), (Some(5), 2));
    assert_eq!(u.assignee_name.as_deref(), Some("Ann"));
    assert!(
        s.update_task(
            t.id,
            TaskPatch {
                priority: Some(9),
                ..Default::default()
            }
        )
        .is_err()
    );
    let cleared = s
        .update_task(
            t.id,
            TaskPatch {
                due_at: Some(None),
                assignee: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!((cleared.due_at, cleared.assignee_email), (None, None));
    s.delete_task(t.id).unwrap();
    assert!(s.get_task(t.id).is_err());
}

#[test]
fn move_reorder_complete_reopen_and_wip_is_advisory() {
    let (mut c, p) = setup();
    let a = task(&mut c, p, "a");
    let b = task(&mut c, p, "b");
    let d = task(&mut c, p, "d");
    let next = col(&mut c, p, ColumnKind::Next);
    let mut s = SqliteTaskStore::new(&mut c);
    s.update_column(
        next.id,
        ColumnPatch {
            wip_limit: Some(Some(1)),
            ..Default::default()
        },
    )
    .unwrap();
    let first = s.move_task(a.id, next.id, None).unwrap();
    assert!(!first.wip_exceeded);
    let second = s.move_task(b.id, next.id, Some(a.id)).unwrap();
    assert!(second.wip_exceeded, "reported, not enforced");
    let third = s.move_task(d.id, next.id, None).unwrap();
    assert!(third.wip_exceeded);
    let order = |s: &mut SqliteTaskStore, col| {
        s.list_tasks(&TaskFilter::default())
            .unwrap()
            .into_iter()
            .filter(|t| t.column_id == col)
            .map(|t| t.title)
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&mut s, next.id), ["d", "a", "b"]);
    // A reorder rewrites only the moved row.
    let before: Vec<_> = [a.id, b.id]
        .iter()
        .map(|i| s.get_task(*i).unwrap().position)
        .collect();
    s.move_task(d.id, next.id, Some(b.id)).unwrap();
    assert_eq!(order(&mut s, next.id), ["a", "b", "d"]);
    let after: Vec<_> = [a.id, b.id]
        .iter()
        .map(|i| s.get_task(*i).unwrap().position)
        .collect();
    assert_eq!(before, after);
    assert!(s.list_columns(p).unwrap().iter().any(|c| c.over_wip));

    let done = s.complete_task(a.id).unwrap();
    assert!(done.completed_at.is_some());
    assert!(
        s.list_tasks(&TaskFilter::default())
            .unwrap()
            .iter()
            .all(|t| t.id != a.id)
    );
    let with_done = TaskFilter {
        include_done: true,
        ..Default::default()
    };
    assert!(
        s.list_tasks(&with_done)
            .unwrap()
            .iter()
            .any(|t| t.id == a.id)
    );
    let reopened = s.reopen_task(a.id).unwrap();
    assert_eq!((reopened.completed_at, reopened.column_id), (None, next.id));
    let log: Vec<_> = s
        .list_activity(a.id)
        .unwrap()
        .into_iter()
        .map(|x| x.kind)
        .collect();
    assert!(log.contains(&"completed".to_owned()) && log.contains(&"reopened".to_owned()));
}

#[test]
fn columns_reorder_collapse_and_delete_moves_tasks() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "x");
    let inbox = col(&mut c, p, ColumnKind::Inbox);
    let next = col(&mut c, p, ColumnKind::Next);
    let mut s = SqliteTaskStore::new(&mut c);
    let extra = s.create_column(p, "Review").unwrap();
    s.reorder_column(extra.id, None).unwrap();
    assert_eq!(s.list_columns(p).unwrap()[0].id, extra.id);
    s.reorder_column(extra.id, Some(inbox.id)).unwrap();
    assert_eq!(s.list_columns(p).unwrap()[1].id, extra.id);
    s.set_column_collapsed(extra.id, true).unwrap();
    assert!(
        s.list_columns(p)
            .unwrap()
            .iter()
            .any(|c| c.id == extra.id && c.collapsed)
    );
    s.delete_column(inbox.id, next.id).unwrap();
    assert_eq!(s.get_task(t.id).unwrap().column_id, next.id);
    assert!(s.delete_column(next.id, next.id).is_err());
}

#[test]
fn contexts_labels_checklist_and_counts() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "t");
    let u = task(&mut c, p, "u");
    let now = 1_000;
    let mut s = SqliteTaskStore::new(&mut c);
    let ctx = s.create_context("@computer").unwrap();
    assert_eq!(ctx.name, "computer");
    assert!(s.create_context("Computer").is_err());
    let lab = s.create_label("Urgent", "#f00").unwrap();
    s.set_task_labels(t.id, &[lab.id]).unwrap();
    s.update_task(
        t.id,
        TaskPatch {
            context_id: Some(Some(ctx.id)),
            due_at: Some(Some(now - 1)),
            ..Default::default()
        },
    )
    .unwrap();
    s.update_task(
        u.id,
        TaskPatch {
            due_at: Some(Some(now + 10)),
            ..Default::default()
        },
    )
    .unwrap();
    let i1 = s.add_checklist_item(t.id, "one").unwrap();
    s.add_checklist_item(t.id, "two").unwrap();
    s.update_checklist_item(i1.id, None, Some(true)).unwrap();
    let got = s.get_task(t.id).unwrap();
    assert_eq!((got.checklist_total, got.checklist_done), (2, 1));
    assert_eq!(got.label_ids, [lab.id]);
    let counts = s.counts(now).unwrap();
    assert_eq!((counts.inbox, counts.today, counts.upcoming), (2, 1, 1));
    assert_eq!(counts.per_project, [(p, 2)]);
    assert_eq!(counts.per_context, [(ctx.id, 1)]);
    assert_eq!(counts.per_label, [(lab.id, 1)]);
    let by_label = TaskFilter {
        label_id: Some(lab.id),
        ..Default::default()
    };
    assert_eq!(s.list_tasks(&by_label).unwrap().len(), 1);
    s.delete_context(ctx.id).unwrap();
    assert_eq!(s.get_task(t.id).unwrap().context_id, None);
    s.delete_label(lab.id).unwrap();
    assert!(s.get_task(t.id).unwrap().label_ids.is_empty());
    s.delete_checklist_item(i1.id).unwrap();
    assert_eq!(s.get_task(t.id).unwrap().checklist_total, 1);
}

fn seed_mail(c: &Connection, subject: &str, thread: i64, mid: &str) {
    c.execute(
        "INSERT OR IGNORE INTO accounts (id, email, provider, auth_kind, username, imap_host,
            imap_port, smtp_host, smtp_port, created_at)
         VALUES (1, 'me@test.dev', 'imap', 'password', 'me', 'h', 993, 'h', 587, 0)",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT OR IGNORE INTO folders (id, account_id, imap_name, role) VALUES (1, 1, 'INBOX', 'inbox')",
        [],
    )
    .unwrap();
    c.execute(
        "INSERT OR IGNORE INTO threads (id, account_id, subject_norm) VALUES (?1, 1, 's')",
        [thread],
    )
    .unwrap();
    c.execute(
        "INSERT INTO messages (account_id, thread_id, folder_id, uid, message_id, subject,
            from_addr, to_json, cc_json, date, snippet)
         VALUES (1, ?1, 1, ?1, ?2, ?3, 'a@x.dev', '[]', '[]', 1, '')",
        rusqlite::params![thread, mid, subject],
    )
    .unwrap();
}

#[test]
fn links_survive_mail_moves_and_missing_mail_is_reported() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "follow up");
    seed_mail(&c, "Quarterly invoice", 1, "<m1@x>");
    let mut s = SqliteTaskStore::new(&mut c);
    let link = |mid: &str, thread| NewLink {
        kind: LinkKind::MailReceived,
        account_email: Some("me@test.dev".into()),
        thread_id: Some(thread),
        message_id: Some(mid.into()),
        event_uid: None,
        other_task_uid: None,
    };
    let l = s.add_link(t.id, link("<m1@x>", 1)).unwrap();
    s.add_link(t.id, link("<gone@x>", 99)).unwrap();
    assert_eq!(
        s.add_link(t.id, link("<m1@x>", 1)).unwrap().id,
        l.id,
        "no duplicates"
    );
    let found = s.list_links(t.id).unwrap();
    assert!(
        matches!(&found[0].target, LinkTarget::Mail { thread_id: 1, subject, .. } if subject == "Quarterly invoice")
    );
    assert!(matches!(found[1].target, LinkTarget::MailNotFound));

    // The thread id changes (mail moved/re-threaded): resolve by Message-ID.
    s.conn
        .execute(
            "INSERT INTO threads (id, account_id, subject_norm) VALUES (7, 1, 's')",
            [],
        )
        .unwrap();
    s.conn
        .execute(
            "UPDATE messages SET thread_id = 7 WHERE message_id = '<m1@x>'",
            [],
        )
        .unwrap();
    assert!(matches!(
        s.list_links(t.id).unwrap()[0].target,
        LinkTarget::Mail { thread_id: 7, .. }
    ));

    // Deleted mail leaves a "not found" link, not a broken task.
    s.conn.execute("DELETE FROM messages", []).unwrap();
    assert!(matches!(
        s.list_links(t.id).unwrap()[0].target,
        LinkTarget::MailNotFound
    ));
    assert_eq!(s.get_task(t.id).unwrap().link_count, 2);
    s.remove_link(l.id).unwrap();
    assert_eq!(s.list_links(t.id).unwrap().len(), 1);
}

#[test]
fn task_links_validate_and_resolve() {
    let (mut c, p) = setup();
    let a = task(&mut c, p, "a");
    let b = task(&mut c, p, "b");
    let mut s = SqliteTaskStore::new(&mut c);
    let to = |uid: &str| NewLink {
        kind: LinkKind::TaskBlocks,
        account_email: None,
        thread_id: None,
        message_id: None,
        event_uid: None,
        other_task_uid: Some(uid.into()),
    };
    assert!(s.add_link(a.id, to(&a.uid)).is_err());
    assert!(s.add_link(a.id, to("nope")).is_err());
    s.add_link(a.id, to(&b.uid)).unwrap();
    assert!(
        matches!(&s.list_links(a.id).unwrap()[0].target, LinkTarget::Task { title, .. } if title == "b")
    );
    s.delete_task(b.id).unwrap();
    assert!(matches!(
        s.list_links(a.id).unwrap()[0].target,
        LinkTarget::TaskNotFound
    ));
}

#[test]
fn text_search_covers_title_description_linked_subject_and_escapes_wildcards() {
    let (mut c, p) = setup();
    let a = task(&mut c, p, "Pay invoice");
    let b = task(&mut c, p, "Call");
    let d = task(&mut c, p, "100% done");
    seed_mail(&c, "Budget approval", 1, "<m@x>");
    let mut s = SqliteTaskStore::new(&mut c);
    s.update_task(
        b.id,
        TaskPatch {
            description: Some("about the roadmap".into()),
            ..Default::default()
        },
    )
    .unwrap();
    s.add_link(
        d.id,
        NewLink {
            kind: LinkKind::MailSent,
            account_email: Some("me@test.dev".into()),
            thread_id: None,
            message_id: Some("<m@x>".into()),
            event_uid: None,
            other_task_uid: None,
        },
    )
    .unwrap();
    let q = |s: &mut SqliteTaskStore, t: &str| {
        s.list_tasks(&TaskFilter {
            text: Some(t.into()),
            ..Default::default()
        })
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect::<Vec<_>>()
    };
    assert_eq!(q(&mut s, "INVOICE"), [a.id]);
    assert_eq!(q(&mut s, "roadmap"), [b.id]);
    assert_eq!(q(&mut s, "budget"), [d.id], "linked mail subject");
    assert_eq!(q(&mut s, "100%"), [d.id]);
    assert!(q(&mut s, "%").len() == 1, "% is literal");
}

#[test]
fn focus_lists() {
    let (mut c, p) = setup();
    let wait = col(&mut c, p, ColumnKind::Waiting);
    let some = col(&mut c, p, ColumnKind::Someday);
    let a = task(&mut c, p, "inbox");
    let b = task(&mut c, p, "waiting");
    let d = task(&mut c, p, "someday");
    let m = SqliteTaskStore::new(&mut c)
        .create_task(NewTask {
            project_id: p,
            title: "mail".into(),
            from_mail: true,
            ..Default::default()
        })
        .unwrap();
    let mut s = SqliteTaskStore::new(&mut c);
    s.move_task(b.id, wait.id, None).unwrap();
    s.move_task(d.id, some.id, None).unwrap();
    let f = |focus| TaskFilter {
        focus: Some(focus),
        ..Default::default()
    };
    let ids = |s: &mut SqliteTaskStore, focus| {
        s.list_tasks(&f(focus))
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&mut s, TaskFocus::Inbox), [a.id, m.id]);
    assert_eq!(ids(&mut s, TaskFocus::WaitingFor), [b.id]);
    assert_eq!(ids(&mut s, TaskFocus::Someday), [d.id]);
    assert_eq!(ids(&mut s, TaskFocus::FromEmail), [m.id]);
}

#[test]
fn every_mutation_stamps_logs_and_tombstones_purge() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "t");
    let mut s = SqliteTaskStore::new(&mut c);
    let state = |s: &SqliteTaskStore, id: i64| -> (String, i64, Option<i64>) {
        s.conn
            .query_row(
                "SELECT sync_state, updated_at, deleted_at FROM tasks WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    s.conn
        .execute(
            "UPDATE tasks SET sync_state = 'synced', updated_at = 0 WHERE id = ?1",
            [t.id],
        )
        .unwrap();
    s.update_task(
        t.id,
        TaskPatch {
            title: Some("t2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (st, upd, del) = state(&s, t.id);
    assert_eq!((st.as_str(), del), ("dirty", None));
    assert!(upd > 0);
    s.delete_task(t.id).unwrap();
    let (_, _, del) = state(&s, t.id);
    assert!(del.is_some(), "tombstone, not a hard delete");
    let ops: Vec<String> = s
        .conn
        .prepare("SELECT op FROM task_changes WHERE entity = 'task' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ops, ["create", "update", "delete"]);
    assert_eq!(s.purge_tombstones(0).unwrap(), 0, "not old enough");
    assert!(s.purge_tombstones(i64::MAX).unwrap() >= 1);
    let left: i64 = s
        .conn
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn project_delete_tombstones_children_and_cross_project_moves_are_refused() {
    let (mut c, p) = setup();
    let t = task(&mut c, p, "t");
    let mut s = SqliteTaskStore::new(&mut c);
    let other = s.create_project("Work", "").unwrap();
    let other_col = s.list_columns(other.id).unwrap()[0].id;
    assert!(s.move_task(t.id, other_col, None).is_err());
    s.delete_project(p).unwrap();
    assert!(s.get_task(t.id).is_err());
    assert_eq!(s.list_projects().unwrap().len(), 1);
    assert!(s.list_columns(p).unwrap().is_empty());
}

#[test]
fn a_task_from_a_thread_links_the_newest_message() {
    let (mut c, p) = setup();
    seed_mail(&c, "Quarterly invoice", 1, "<old@x>");
    c.execute(
        "INSERT INTO messages (account_id, thread_id, folder_id, uid, message_id, subject,
            from_addr, to_json, cc_json, date, snippet)
         VALUES (1, 1, 1, 2, '<new@x>', 'Re: Quarterly invoice', 'a@x.dev', '[]', '[]', 5, '')",
        [],
    )
    .unwrap();
    let waiting = col(&mut c, p, ColumnKind::Waiting);
    let mut s = SqliteTaskStore::new(&mut c);
    let draft = |status| TaskDraft {
        title: None,
        project_id: None,
        status,
        due_at: Some(10),
        context_id: None,
        label_ids: vec![],
    };
    let task = s.create_from_thread(1, draft(ColumnKind::Waiting)).unwrap();
    assert_eq!(task.title, "Re: Quarterly invoice");
    assert_eq!(task.column_id, waiting.id);
    assert_eq!(task.source, TaskSource::Mail);
    assert_eq!((task.due_at, task.link_count), (Some(10), 1));
    let links = s.list_links(task.id).unwrap();
    assert_eq!(links[0].link.message_id.as_deref(), Some("<new@x>"));
    assert!(matches!(
        links[0].target,
        LinkTarget::Mail { thread_id: 1, .. }
    ));
    assert!(
        s.list_activity(task.id)
            .unwrap()
            .iter()
            .any(|a| a.kind == "from_mail" && a.text.contains("Quarterly invoice"))
    );

    // An edited title wins; a project without the status column uses its Inbox.
    let named = TaskDraft {
        title: Some("Pay it".into()),
        ..draft(ColumnKind::Custom)
    };
    let task = s.create_from_thread(1, named).unwrap();
    assert_eq!(task.title, "Pay it");

    assert!(matches!(
        s.create_from_thread(99, draft(ColumnKind::Inbox)),
        Err(crate::error::CoreError::NotFound(_))
    ));
    s.conn
        .execute(
            "UPDATE messages SET message_id = NULL WHERE thread_id = 1",
            [],
        )
        .unwrap();
    let before = s.list_tasks(&TaskFilter::default()).unwrap().len();
    assert!(s.create_from_thread(1, draft(ColumnKind::Inbox)).is_err());
    assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);
}

#[test]
fn threads_report_their_task_status_and_whether_a_reply_is_awaited() {
    let (mut c, p) = setup();
    seed_mail(&c, "Invoice", 1, "<a@x>");
    seed_mail(&c, "Contract", 2, "<b@x>");
    seed_mail(&c, "Untouched", 3, "<c@x>");
    let waiting = col(&mut c, p, ColumnKind::Waiting);
    let mut s = SqliteTaskStore::new(&mut c);
    let draft = |status| TaskDraft {
        title: None,
        project_id: None,
        status,
        due_at: None,
        context_id: None,
        label_ids: vec![],
    };
    let invoice = s.create_from_thread(1, draft(ColumnKind::Waiting)).unwrap();
    s.create_from_thread(2, draft(ColumnKind::Next)).unwrap();

    let found = s.threads_with_tasks(None).unwrap();
    assert_eq!(found.len(), 2, "unlinked threads have no chip");
    let by = |id| found.iter().find(|t| t.thread_id == id).unwrap();
    assert_eq!(by(1).status, waiting.name);
    assert!(by(1).waiting_on_reply, "no reply yet");
    assert!(!by(2).waiting_on_reply, "not in Waiting for");
    assert_eq!(s.threads_with_tasks(Some(&[2])).unwrap().len(), 1);

    // A reply arrives after the task was made: no longer awaited.
    s.conn
        .execute(
            "INSERT INTO messages (account_id, thread_id, folder_id, uid, message_id, subject,
                from_addr, to_json, cc_json, date, snippet)
             VALUES (1, 1, 1, 9, '<r@x>', 'Re: Invoice', 'a@x.dev', '[]', '[]', ?1, '')",
            [crate::models::now_ms() + 60_000],
        )
        .unwrap();
    let found = s.threads_with_tasks(Some(&[1])).unwrap();
    assert!(!found[0].waiting_on_reply);
    assert_eq!(found[0].task_id, invoice.id);

    // A deleted task drops the chip.
    s.delete_task(invoice.id).unwrap();
    assert!(s.threads_with_tasks(Some(&[1])).unwrap().is_empty());
}

#[test]
fn a_reply_is_noted_once_and_moves_a_waiting_task_back_when_asked() {
    let (mut c, p) = setup();
    seed_mail(&c, "Invoice", 1, "<a@x>");
    seed_mail(&c, "Contract", 2, "<b@x>");
    let next = col(&mut c, p, ColumnKind::Next);
    let waiting = col(&mut c, p, ColumnKind::Waiting);
    let mut s = SqliteTaskStore::new(&mut c);
    let draft = || TaskDraft {
        title: None,
        project_id: None,
        status: ColumnKind::Waiting,
        due_at: None,
        context_id: None,
        label_ids: vec![],
    };
    let moves = s.create_from_thread(1, draft()).unwrap();
    let stays = s.create_from_thread(2, draft()).unwrap();
    s.update_task(
        moves.id,
        TaskPatch {
            move_back_on_reply: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(s.get_task(moves.id).unwrap().move_back_on_reply);
    assert!(!s.get_task(stays.id).unwrap().move_back_on_reply);

    // Nothing has come in yet.
    assert!(s.note_replies(&[1, 2]).unwrap().is_empty());

    let later = crate::models::now_ms() + 60_000;
    for (thread, uid, mid) in [(1_i64, 11_i64, "<r1@x>"), (2, 12, "<r2@x>")] {
        s.conn
            .execute(
                "INSERT INTO messages (account_id, thread_id, folder_id, uid, message_id, subject,
                    from_addr, to_json, cc_json, date, snippet)
                 VALUES (1, ?1, 1, ?2, ?3, 'Re: x', 'a@x.dev', '[]', '[]', ?4, '')",
                rusqlite::params![thread, uid, mid, later],
            )
            .unwrap();
    }
    // Only the threads named in the call are looked at.
    assert_eq!(s.note_replies(&[1]).unwrap(), vec![moves.id]);
    let moved = s.get_task(moves.id).unwrap();
    assert_eq!(moved.column_id, next.id, "moved back to Next actions");
    let log = s.list_activity(moves.id).unwrap();
    assert!(
        log.iter()
            .any(|a| a.kind == "reply" && a.text.contains("Re: x"))
    );
    assert!(log.iter().any(|a| a.text.starts_with("Moved back")));

    // The other task only gets the note; it was not asked to move.
    assert_eq!(s.note_replies(&[2]).unwrap(), vec![stays.id]);
    assert_eq!(s.get_task(stays.id).unwrap().column_id, waiting.id);

    // The same reply is not noted twice.
    assert!(s.note_replies(&[1, 2]).unwrap().is_empty());
    let replies = |s: &mut SqliteTaskStore, id| {
        s.list_activity(id)
            .unwrap()
            .iter()
            .filter(|a| a.kind == "reply")
            .count()
    };
    assert_eq!(
        (replies(&mut s, moves.id), replies(&mut s, stays.id)),
        (1, 1)
    );
}
