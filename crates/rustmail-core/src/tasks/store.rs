//! SQLite implementation of [`TaskStore`]. All mutations run in one
//! transaction and go through [`stamp`], the single change path.

use super::position::{after, between};
use super::*;
use crate::error::CoreError;
use crate::models::now_ms;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

const MAX_NAME_CHARS: usize = 200;
const MAX_TITLE_CHARS: usize = 500;
const MAX_TEXT_BYTES: usize = 100 * 1024;
const MAX_ID_CHARS: usize = 998;
const SEED_FLAG: &str = "tasks_seeded";

/// Default columns: (name, icon token, colour, kind).
const DEFAULT_COLUMNS: [(&str, &str, &str, ColumnKind); 6] = [
    ("Inbox", "inbox", "#6b7280", ColumnKind::Inbox),
    ("Next actions", "bolt", "#2563eb", ColumnKind::Next),
    ("Waiting for", "hourglass", "#d97706", ColumnKind::Waiting),
    ("Scheduled", "calendar", "#7c3aed", ColumnKind::Scheduled),
    ("Done", "check", "#16a34a", ColumnKind::Done),
    ("Someday / Maybe", "moon", "#9ca3af", ColumnKind::Someday),
];

pub struct SqliteTaskStore<'a> {
    pub(super) conn: &'a mut Connection,
}

impl<'a> SqliteTaskStore<'a> {
    pub fn new(conn: &'a mut Connection) -> Self {
        Self { conn }
    }
}

fn invalid(msg: &str) -> CoreError {
    CoreError::Other(msg.to_owned())
}

fn new_uid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn clean_name(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() || s.chars().count() > MAX_NAME_CHARS {
        return Err(invalid("name must be 1-200 characters"));
    }
    Ok(s.to_owned())
}

fn clean_text(raw: &str) -> Result<String> {
    if raw.len() > MAX_TEXT_BYTES {
        return Err(invalid("text is too long"));
    }
    Ok(raw.to_owned())
}

/// The single change path: stamps `updated_at`, marks the row dirty, writes the
/// tombstone for deletes, and appends to `task_changes`.
fn stamp(tx: &Connection, entity: &str, table: &str, id: i64, op: &str) -> Result<()> {
    let at = now_ms();
    let uid: String = tx.query_row(
        &format!("SELECT uid FROM {table} WHERE id = ?1"),
        [id],
        |r| r.get(0),
    )?;
    let sql = if op == "delete" {
        format!(
            "UPDATE {table} SET updated_at = ?2, deleted_at = ?2, sync_state = 'dirty' WHERE id = ?1"
        )
    } else {
        format!("UPDATE {table} SET updated_at = ?2, sync_state = 'dirty' WHERE id = ?1")
    };
    tx.execute(&sql, params![id, at])?;
    tx.execute(
        "INSERT INTO task_changes (entity, uid, op, at) VALUES (?1, ?2, ?3, ?4)",
        params![entity, uid, op, at],
    )?;
    Ok(())
}

fn activity(tx: &Connection, task_id: i64, kind: &str, text: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO task_activity (task_id, kind, text, at) VALUES (?1, ?2, ?3, ?4)",
        params![task_id, kind, text, now_ms()],
    )?;
    Ok(())
}

fn require(tx: &Connection, table: &str, id: i64) -> Result<()> {
    let found: Option<i64> = tx
        .query_row(
            &format!("SELECT 1 FROM {table} WHERE id = ?1 AND deleted_at IS NULL"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    found
        .map(|_| ())
        .ok_or_else(|| CoreError::NotFound(format!("{table} {id}")))
}

// ---------- projects / columns ----------

fn project_from_row(r: &Row) -> rusqlite::Result<TaskProject> {
    Ok(TaskProject {
        id: r.get(0)?,
        uid: r.get(1)?,
        name: r.get(2)?,
        colour: r.get(3)?,
        position: r.get(4)?,
    })
}

fn insert_project(tx: &Connection, name: &str, colour: &str) -> Result<TaskProject> {
    let last: Option<String> = tx
        .query_row(
            "SELECT MAX(position) FROM task_projects WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let now = now_ms();
    tx.execute(
        "INSERT INTO task_projects (uid, name, colour, position, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![new_uid(), name, colour, after(last.as_deref()), now],
    )?;
    let id = tx.last_insert_rowid();
    stamp(tx, "project", "task_projects", id, "create")?;
    for (name, icon, colour, kind) in DEFAULT_COLUMNS {
        insert_column(tx, id, name, icon, colour, kind)?;
    }
    get_project(tx, id)
}

fn get_project(tx: &Connection, id: i64) -> Result<TaskProject> {
    tx.query_row(
        "SELECT id, uid, name, colour, position FROM task_projects
         WHERE id = ?1 AND deleted_at IS NULL",
        [id],
        project_from_row,
    )
    .optional()?
    .ok_or_else(|| CoreError::NotFound(format!("project {id}")))
}

fn insert_column(
    tx: &Connection,
    project_id: i64,
    name: &str,
    icon: &str,
    colour: &str,
    kind: ColumnKind,
) -> Result<i64> {
    let last: Option<String> = tx
        .query_row(
            "SELECT MAX(position) FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL",
            [project_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let now = now_ms();
    tx.execute(
        "INSERT INTO task_columns (uid, project_id, name, icon, colour, position, kind, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            new_uid(),
            project_id,
            name,
            icon,
            colour,
            after(last.as_deref()),
            kind.as_str(),
            now
        ],
    )?;
    let id = tx.last_insert_rowid();
    stamp(tx, "column", "task_columns", id, "create")?;
    Ok(id)
}

const COLUMN_SELECT: &str = "SELECT c.id, c.uid, c.project_id, c.name, c.icon, c.colour,
        c.wip_limit, c.position, c.collapsed, c.kind,
        (SELECT COUNT(*) FROM tasks t WHERE t.column_id = c.id
           AND t.deleted_at IS NULL AND t.completed_at IS NULL)
     FROM task_columns c";

fn column_from_row(r: &Row) -> rusqlite::Result<TaskColumn> {
    let kind: String = r.get(9)?;
    let wip: Option<i64> = r.get(6)?;
    let count: i64 = r.get(10)?;
    Ok(TaskColumn {
        id: r.get(0)?,
        uid: r.get(1)?,
        project_id: r.get(2)?,
        name: r.get(3)?,
        icon: r.get(4)?,
        colour: r.get(5)?,
        wip_limit: wip,
        position: r.get(7)?,
        collapsed: r.get::<_, i64>(8)? != 0,
        kind: ColumnKind::parse(&kind).unwrap_or(ColumnKind::Custom),
        task_count: count,
        over_wip: wip.is_some_and(|w| count > w),
    })
}

fn get_column(tx: &Connection, id: i64) -> Result<TaskColumn> {
    tx.query_row(
        &format!("{COLUMN_SELECT} WHERE c.id = ?1 AND c.deleted_at IS NULL"),
        [id],
        column_from_row,
    )
    .optional()?
    .ok_or_else(|| CoreError::NotFound(format!("column {id}")))
}

// ---------- tasks ----------

const TASK_SELECT: &str = "SELECT t.id, t.uid, t.project_id, t.column_id, t.title, t.description,
        t.due_at, t.start_at, t.priority, t.context_id, t.assignee_email, t.assignee_name,
        t.position, t.completed_at, t.source, t.created_at, t.updated_at,
        (SELECT COUNT(*) FROM task_checklist_items c
           WHERE c.task_id = t.id AND c.deleted_at IS NULL),
        (SELECT COUNT(*) FROM task_checklist_items c
           WHERE c.task_id = t.id AND c.deleted_at IS NULL AND c.done = 1),
        (SELECT COUNT(*) FROM task_links l WHERE l.task_id = t.id AND l.deleted_at IS NULL)
     FROM tasks t";

fn task_from_row(r: &Row) -> rusqlite::Result<Task> {
    let source: String = r.get(14)?;
    Ok(Task {
        id: r.get(0)?,
        uid: r.get(1)?,
        project_id: r.get(2)?,
        column_id: r.get(3)?,
        title: r.get(4)?,
        description: r.get(5)?,
        due_at: r.get(6)?,
        start_at: r.get(7)?,
        priority: r.get(8)?,
        context_id: r.get(9)?,
        assignee_email: r.get(10)?,
        assignee_name: r.get(11)?,
        position: r.get(12)?,
        completed_at: r.get(13)?,
        source: if source == "mail" {
            TaskSource::Mail
        } else {
            TaskSource::Manual
        },
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
        label_ids: Vec::new(),
        checklist_total: r.get(17)?,
        checklist_done: r.get(18)?,
        link_count: r.get(19)?,
    })
}

fn attach_labels(tx: &Connection, tasks: &mut [Task]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let ids = tasks
        .iter()
        .map(|t| t.id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = tx.prepare(&format!(
        "SELECT l.task_id, l.label_id FROM task_label_links l
         JOIN task_labels b ON b.id = l.label_id AND b.deleted_at IS NULL
         WHERE l.task_id IN ({ids}) ORDER BY b.name COLLATE NOCASE"
    ))?;
    let pairs = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (task_id, label_id) in pairs {
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            t.label_ids.push(label_id);
        }
    }
    Ok(())
}

fn fetch_task(tx: &Connection, id: i64) -> Result<Task> {
    let mut task = tx
        .query_row(
            &format!("{TASK_SELECT} WHERE t.id = ?1 AND t.deleted_at IS NULL"),
            [id],
            task_from_row,
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound(format!("task {id}")))?;
    attach_labels(tx, std::slice::from_mut(&mut task))?;
    Ok(task)
}

enum Place {
    Top,
    After(i64),
    End,
}

/// Position key for a task in `column_id`, ignoring `exclude` (the task being
/// moved, if it already sits in that column).
fn task_position(tx: &Connection, column_id: i64, place: Place, exclude: i64) -> Result<String> {
    let keys = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<Option<String>> {
        Ok(tx.query_row(sql, args, |r| r.get(0)).optional()?)
    };
    match place {
        Place::End => {
            let last = keys(
                "SELECT position FROM tasks WHERE column_id = ?1 AND deleted_at IS NULL AND id != ?2
                 ORDER BY position DESC LIMIT 1",
                &[&column_id, &exclude],
            )?;
            Ok(after(last.as_deref()))
        }
        Place::Top => {
            let first = keys(
                "SELECT position FROM tasks WHERE column_id = ?1 AND deleted_at IS NULL AND id != ?2
                 ORDER BY position LIMIT 1",
                &[&column_id, &exclude],
            )?;
            Ok(between("", first.as_deref()))
        }
        Place::After(a) => {
            let prev = keys(
                "SELECT position FROM tasks WHERE id = ?1 AND column_id = ?2 AND deleted_at IS NULL",
                &[&a, &column_id],
            )?
            .ok_or_else(|| invalid("anchor task is not in the target column"))?;
            let next = keys(
                "SELECT position FROM tasks WHERE column_id = ?1 AND deleted_at IS NULL
                   AND id != ?2 AND position > ?3 ORDER BY position LIMIT 1",
                &[&column_id, &exclude, &prev],
            )?;
            Ok(between(&prev, next.as_deref()))
        }
    }
}

fn column_kind(tx: &Connection, id: i64) -> Result<(i64, String, ColumnKind)> {
    let (project, name, kind): (i64, String, String) = tx
        .query_row(
            "SELECT project_id, name, kind FROM task_columns WHERE id = ?1 AND deleted_at IS NULL",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| CoreError::NotFound(format!("column {id}")))?;
    Ok((
        project,
        name,
        ColumnKind::parse(&kind).unwrap_or(ColumnKind::Custom),
    ))
}

fn move_task_inner(tx: &Connection, id: i64, column_id: i64, place: Place) -> Result<MoveOutcome> {
    let task = fetch_task(tx, id)?;
    let (project, name, kind) = column_kind(tx, column_id)?;
    if project != task.project_id {
        return Err(invalid("a task can only move within its project"));
    }
    let position = task_position(tx, column_id, place, id)?;
    let done = kind == ColumnKind::Done;
    tx.execute(
        "UPDATE tasks SET column_id = ?2, position = ?3,
                completed_at = CASE WHEN ?4 THEN COALESCE(completed_at, ?5) ELSE NULL END
         WHERE id = ?1",
        params![id, column_id, position, done, now_ms()],
    )?;
    stamp(tx, "task", "tasks", id, "update")?;
    if column_id != task.column_id {
        let (_, from, _) = column_kind(tx, task.column_id)?;
        activity(
            tx,
            id,
            if done { "completed" } else { "moved" },
            &format!("Moved from {from} to {name}"),
        )?;
    }
    let column = get_column(tx, column_id)?;
    Ok(MoveOutcome {
        task: fetch_task(tx, id)?,
        wip_exceeded: column.over_wip,
    })
}

fn like_pattern(raw: &str) -> String {
    let mut s = String::from("%");
    for c in raw.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

fn focus_sql(focus: TaskFocus) -> &'static str {
    match focus {
        TaskFocus::Inbox => "c.kind = 'inbox'",
        TaskFocus::Today => "t.due_at IS NOT NULL AND t.due_at <= ?",
        TaskFocus::Upcoming => "t.due_at IS NOT NULL AND t.due_at > ?",
        TaskFocus::WaitingFor => "c.kind = 'waiting'",
        TaskFocus::FromEmail => "t.source = 'mail'",
        TaskFocus::Someday => "c.kind = 'someday'",
    }
}

const LIVE_JOINS: &str = " JOIN task_columns c ON c.id = t.column_id
     JOIN task_projects p ON p.id = t.project_id AND p.deleted_at IS NULL";

fn count_focus(tx: &Connection, focus: TaskFocus, today_end_ms: i64) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM tasks t {LIVE_JOINS}
         WHERE t.deleted_at IS NULL AND t.completed_at IS NULL AND {}",
        focus_sql(focus)
    );
    let n = if matches!(focus, TaskFocus::Today | TaskFocus::Upcoming) {
        tx.query_row(&sql, [today_end_ms], |r| r.get(0))?
    } else {
        tx.query_row(&sql, [], |r| r.get(0))?
    };
    Ok(n)
}

fn pairs(tx: &Connection, sql: &str) -> Result<Vec<(i64, i64)>> {
    let mut stmt = tx.prepare(sql)?;
    Ok(stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn resolve_link(tx: &Connection, link: TaskLink) -> Result<ResolvedLink> {
    let target = if link.kind.is_mail() {
        let found = match &link.message_id {
            Some(mid) => tx
                .query_row(
                    "SELECT m.account_id, m.thread_id, m.subject
                     FROM messages m JOIN accounts a ON a.id = m.account_id
                     WHERE m.message_id = ?1 AND m.thread_id IS NOT NULL
                       AND (?2 IS NULL OR a.email = ?2)
                     ORDER BY (m.thread_id = ?3) DESC, m.date DESC LIMIT 1",
                    params![mid, link.account_email, link.thread_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?,
            None => None,
        };
        match found {
            Some((account_id, thread_id, subject)) => LinkTarget::Mail {
                account_id,
                thread_id,
                subject,
            },
            None => LinkTarget::MailNotFound,
        }
    } else if link.kind.is_task() {
        let found = match &link.other_task_uid {
            Some(uid) => tx
                .query_row(
                    "SELECT id, title FROM tasks WHERE uid = ?1 AND deleted_at IS NULL",
                    [uid],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?,
            None => None,
        };
        match found {
            Some((task_id, title)) => LinkTarget::Task { task_id, title },
            None => LinkTarget::TaskNotFound,
        }
    } else {
        LinkTarget::Event
    };
    Ok(ResolvedLink { link, target })
}

fn link_from_row(r: &Row) -> rusqlite::Result<TaskLink> {
    let kind: String = r.get(3)?;
    Ok(TaskLink {
        id: r.get(0)?,
        uid: r.get(1)?,
        task_id: r.get(2)?,
        kind: LinkKind::parse(&kind).unwrap_or(LinkKind::Event),
        account_email: r.get(4)?,
        thread_id: r.get(5)?,
        message_id: r.get(6)?,
        event_uid: r.get(7)?,
        other_task_uid: r.get(8)?,
        created_at: r.get(9)?,
    })
}

const LINK_SELECT: &str = "SELECT id, uid, task_id, kind, account_email, thread_id, message_id,
        event_uid, other_task_uid, created_at FROM task_links";

fn checklist_from_row(r: &Row) -> rusqlite::Result<ChecklistItem> {
    Ok(ChecklistItem {
        id: r.get(0)?,
        uid: r.get(1)?,
        task_id: r.get(2)?,
        text: r.get(3)?,
        done: r.get::<_, i64>(4)? != 0,
        position: r.get(5)?,
    })
}

const CHECKLIST_SELECT: &str =
    "SELECT id, uid, task_id, text, done, position FROM task_checklist_items";

fn checklist_task(tx: &Connection, id: i64) -> Result<i64> {
    tx.query_row(
        "SELECT task_id FROM task_checklist_items WHERE id = ?1 AND deleted_at IS NULL",
        [id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| CoreError::NotFound(format!("checklist item {id}")))
}

fn opt_clean(v: &Option<String>) -> Result<Option<String>> {
    match v {
        Some(s) if s.len() > MAX_ID_CHARS => Err(invalid("identifier is too long")),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => Ok(Some(s.trim().to_owned())),
        None => Ok(None),
    }
}

fn unique_name_error(e: rusqlite::Error, what: &str) -> CoreError {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            CoreError::Other(format!("a {what} with that name already exists"))
        }
        other => other.into(),
    }
}

impl SqliteTaskStore<'_> {
    /// For each mail thread that has a linked task, the newest such task.
    /// Links resolve through the Message-ID, like `list_links`, so moved or
    /// re-threaded mail keeps its chip. `only` limits the answer to those
    /// threads; `None` answers for every linked thread.
    pub fn threads_with_tasks(&mut self, only: Option<&[i64]>) -> Result<Vec<ThreadTask>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.thread_id, t.id, t.title, c.name, c.kind, t.due_at, l.created_at,
                    (SELECT MAX(x.date) FROM messages x
                      WHERE x.thread_id = m.thread_id AND x.is_outgoing = 0)
             FROM task_links l
             JOIN tasks t ON t.id = l.task_id AND t.deleted_at IS NULL
             JOIN task_columns c ON c.id = t.column_id
             JOIN messages m ON m.message_id = l.message_id
             JOIN accounts a ON a.id = m.account_id
              AND (l.account_email IS NULL OR a.email = l.account_email)
             WHERE l.deleted_at IS NULL AND l.kind IN ('mail_received', 'mail_sent')
               AND m.thread_id IS NOT NULL
             ORDER BY t.created_at DESC, t.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            let (thread_id, task_id, title, status, kind, due_at, linked_at, latest) = row?;
            if only.is_some_and(|ids| !ids.contains(&thread_id)) || !seen.insert(thread_id) {
                continue;
            }
            let status_kind = ColumnKind::parse(&kind).unwrap_or(ColumnKind::Custom);
            out.push(ThreadTask {
                thread_id,
                task_id,
                title,
                status,
                status_kind,
                due_at,
                waiting_on_reply: status_kind == ColumnKind::Waiting
                    && latest.is_none_or(|d| d <= linked_at),
            });
        }
        Ok(out)
    }
    /// Create a task for a mail thread and link its newest message, so later
    /// replies resolve through the thread. Returns the new task.
    pub fn create_from_thread(&mut self, thread_id: i64, draft: TaskDraft) -> Result<Task> {
        let (account_email, message_id, subject) = self
            .conn
            .query_row(
                "SELECT a.email, m.message_id, m.subject
                 FROM messages m JOIN accounts a ON a.id = m.account_id
                 WHERE m.thread_id = ?1 ORDER BY m.date DESC, m.id DESC LIMIT 1",
                [thread_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound("mail thread".into()))?;
        let message_id = message_id
            .filter(|m| !m.trim().is_empty())
            .ok_or_else(|| invalid("the message has no Message-ID to link"))?;
        let project_id = match draft.project_id {
            Some(p) => p,
            None => self
                .list_projects()?
                .first()
                .map(|p| p.id)
                .ok_or_else(|| invalid("no project"))?,
        };
        let column_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM task_columns
                 WHERE project_id = ?1 AND kind = ?2 AND deleted_at IS NULL
                 ORDER BY position LIMIT 1",
                params![project_id, draft.status.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        let title = draft
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                if subject.trim().is_empty() {
                    "(no subject)".to_owned()
                } else {
                    subject.clone()
                }
            });
        let task = self.create_task(NewTask {
            project_id,
            column_id,
            title,
            due_at: draft.due_at,
            context_id: draft.context_id,
            label_ids: draft.label_ids,
            from_mail: true,
            ..Default::default()
        })?;
        let linked = self.add_link(
            task.id,
            NewLink {
                kind: LinkKind::MailReceived,
                account_email: Some(account_email),
                thread_id: Some(thread_id),
                message_id: Some(message_id),
                event_uid: None,
                other_task_uid: None,
            },
        );
        if let Err(error) = linked {
            // Do not leave a task that points at nothing.
            let _ = self.delete_task(task.id);
            return Err(error);
        }
        activity(
            self.conn,
            task.id,
            "from_mail",
            &format!("Created from email: {subject}"),
        )?;
        self.get_task(task.id)
    }
}

impl TaskStore for SqliteTaskStore<'_> {
    fn ensure_default_project(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let seeded: Option<String> = tx
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                [SEED_FLAG],
                |r| r.get(0),
            )
            .optional()?;
        if seeded.is_none() {
            insert_project(&tx, "Personal", "#2563eb")?;
            tx.execute(
                "INSERT INTO app_settings (key, value) VALUES (?1, '1')",
                [SEED_FLAG],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn list_projects(&mut self) -> Result<Vec<TaskProject>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, name, colour, position FROM task_projects
             WHERE deleted_at IS NULL ORDER BY position",
        )?;
        Ok(stmt
            .query_map([], project_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn create_project(&mut self, name: &str, colour: &str) -> Result<TaskProject> {
        let name = clean_name(name)?;
        let tx = self.conn.transaction()?;
        let p = insert_project(&tx, &name, colour)?;
        tx.commit()?;
        Ok(p)
    }

    fn update_project(&mut self, id: i64, name: &str, colour: &str) -> Result<()> {
        let name = clean_name(name)?;
        let tx = self.conn.transaction()?;
        require(&tx, "task_projects", id)?;
        tx.execute(
            "UPDATE task_projects SET name = ?2, colour = ?3 WHERE id = ?1",
            params![id, name, colour],
        )?;
        stamp(&tx, "project", "task_projects", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn delete_project(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "task_projects", id)?;
        let ids = |sql: &str| -> Result<Vec<i64>> {
            let mut stmt = tx.prepare(sql)?;
            Ok(stmt
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        };
        let tasks = ids("SELECT id FROM tasks WHERE project_id = ?1 AND deleted_at IS NULL")?;
        let cols = ids("SELECT id FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL")?;
        for t in tasks {
            stamp(&tx, "task", "tasks", t, "delete")?;
        }
        for c in cols {
            stamp(&tx, "column", "task_columns", c, "delete")?;
        }
        stamp(&tx, "project", "task_projects", id, "delete")?;
        tx.commit()?;
        Ok(())
    }

    fn list_columns(&mut self, project_id: i64) -> Result<Vec<TaskColumn>> {
        let mut stmt = self.conn.prepare(&format!(
            "{COLUMN_SELECT} WHERE c.project_id = ?1 AND c.deleted_at IS NULL ORDER BY c.position"
        ))?;
        Ok(stmt
            .query_map([project_id], column_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn create_column(&mut self, project_id: i64, name: &str) -> Result<TaskColumn> {
        let name = clean_name(name)?;
        let tx = self.conn.transaction()?;
        require(&tx, "task_projects", project_id)?;
        let id = insert_column(&tx, project_id, &name, "", "", ColumnKind::Custom)?;
        let col = get_column(&tx, id)?;
        tx.commit()?;
        Ok(col)
    }

    fn update_column(&mut self, id: i64, patch: ColumnPatch) -> Result<()> {
        let tx = self.conn.transaction()?;
        let col = get_column(&tx, id)?;
        let name = match &patch.name {
            Some(n) => clean_name(n)?,
            None => col.name,
        };
        let wip = match patch.wip_limit {
            Some(Some(w)) if w <= 0 => return Err(invalid("WIP limit must be positive")),
            Some(w) => w,
            None => col.wip_limit,
        };
        tx.execute(
            "UPDATE task_columns SET name = ?2, icon = ?3, colour = ?4, wip_limit = ?5 WHERE id = ?1",
            params![
                id,
                name,
                patch.icon.unwrap_or(col.icon),
                patch.colour.unwrap_or(col.colour),
                wip
            ],
        )?;
        stamp(&tx, "column", "task_columns", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn reorder_column(&mut self, id: i64, after_id: Option<i64>) -> Result<()> {
        let tx = self.conn.transaction()?;
        let col = get_column(&tx, id)?;
        let pos = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Result<Option<String>> {
            Ok(tx.query_row(sql, args, |r| r.get(0)).optional()?)
        };
        let (prev, next) = match after_id {
            Some(a) => {
                let prev = pos(
                    "SELECT position FROM task_columns
                     WHERE id = ?1 AND project_id = ?2 AND deleted_at IS NULL",
                    &[&a, &col.project_id],
                )?
                .ok_or_else(|| invalid("anchor column is not in this project"))?;
                let next = pos(
                    "SELECT position FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL
                       AND id != ?2 AND position > ?3 ORDER BY position LIMIT 1",
                    &[&col.project_id, &id, &prev],
                )?;
                (prev, next)
            }
            None => {
                let first = pos(
                    "SELECT position FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL
                       AND id != ?2 ORDER BY position LIMIT 1",
                    &[&col.project_id, &id],
                )?;
                (String::new(), first)
            }
        };
        let key = between(&prev, next.as_deref());
        tx.execute(
            "UPDATE task_columns SET position = ?2 WHERE id = ?1",
            params![id, key],
        )?;
        stamp(&tx, "column", "task_columns", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn set_column_collapsed(&mut self, id: i64, collapsed: bool) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "task_columns", id)?;
        tx.execute(
            "UPDATE task_columns SET collapsed = ?2 WHERE id = ?1",
            params![id, collapsed],
        )?;
        stamp(&tx, "column", "task_columns", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn delete_column(&mut self, id: i64, move_to: i64) -> Result<()> {
        if id == move_to {
            return Err(invalid("choose another column for the tasks"));
        }
        let tx = self.conn.transaction()?;
        let col = get_column(&tx, id)?;
        let (project, _, _) = column_kind(&tx, move_to)?;
        if project != col.project_id {
            return Err(invalid("the target column is in another project"));
        }
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM tasks WHERE column_id = ?1 AND deleted_at IS NULL ORDER BY position",
            )?;
            stmt.query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for t in ids {
            move_task_inner(&tx, t, move_to, Place::End)?;
        }
        stamp(&tx, "column", "task_columns", id, "delete")?;
        tx.commit()?;
        Ok(())
    }

    fn create_task(&mut self, new: NewTask) -> Result<Task> {
        let title = new.title.trim().to_owned();
        if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
            return Err(invalid("title must be 1-500 characters"));
        }
        if !(0..=3).contains(&new.priority) {
            return Err(invalid("priority must be 0-3"));
        }
        let description = clean_text(&new.description)?;
        let assignee_email = opt_clean(&new.assignee_email)?;
        let tx = self.conn.transaction()?;
        require(&tx, "task_projects", new.project_id)?;
        let column_id = match new.column_id {
            Some(c) => c,
            None => tx
                .query_row(
                    "SELECT id FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL
                     ORDER BY (kind = 'inbox') DESC, position LIMIT 1",
                    [new.project_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| invalid("project has no columns"))?,
        };
        let (project, _, kind) = column_kind(&tx, column_id)?;
        if project != new.project_id {
            return Err(invalid("column is not in this project"));
        }
        if let Some(ctx) = new.context_id {
            require(&tx, "task_contexts", ctx)?;
        }
        let now = now_ms();
        let position = task_position(&tx, column_id, Place::End, 0)?;
        tx.execute(
            "INSERT INTO tasks (uid, project_id, column_id, title, description, due_at, start_at,
                priority, context_id, assignee_email, assignee_name, position, completed_at,
                source, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15)",
            params![
                new_uid(),
                new.project_id,
                column_id,
                title,
                description,
                new.due_at,
                new.start_at,
                new.priority,
                new.context_id,
                assignee_email,
                new.assignee_name.filter(|_| assignee_email.is_some()),
                position,
                (kind == ColumnKind::Done).then_some(now),
                if new.from_mail { "mail" } else { "manual" },
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        stamp(&tx, "task", "tasks", id, "create")?;
        for label in new.label_ids {
            require(&tx, "task_labels", label)?;
            tx.execute(
                "INSERT OR IGNORE INTO task_label_links (task_id, label_id) VALUES (?1, ?2)",
                params![id, label],
            )?;
        }
        activity(&tx, id, "created", "Task created")?;
        let task = fetch_task(&tx, id)?;
        tx.commit()?;
        Ok(task)
    }

    fn get_task(&mut self, id: i64) -> Result<Task> {
        fetch_task(self.conn, id)
    }

    fn update_task(&mut self, id: i64, patch: TaskPatch) -> Result<Task> {
        let tx = self.conn.transaction()?;
        let t = fetch_task(&tx, id)?;
        let mut changed: Vec<&str> = Vec::new();
        let title = match patch.title {
            Some(s) => {
                let s = s.trim().to_owned();
                if s.is_empty() || s.chars().count() > MAX_TITLE_CHARS {
                    return Err(invalid("title must be 1-500 characters"));
                }
                changed.push("title");
                s
            }
            None => t.title,
        };
        let description = match patch.description {
            Some(s) => {
                changed.push("description");
                clean_text(&s)?
            }
            None => t.description,
        };
        let due_at = patch
            .due_at
            .inspect(|_| changed.push("due date"))
            .unwrap_or(t.due_at);
        let start_at = patch
            .start_at
            .inspect(|_| changed.push("start date"))
            .unwrap_or(t.start_at);
        let priority = match patch.priority {
            Some(p) if !(0..=3).contains(&p) => return Err(invalid("priority must be 0-3")),
            Some(p) => {
                changed.push("priority");
                p
            }
            None => t.priority,
        };
        let context_id = match patch.context_id {
            Some(c) => {
                if let Some(c) = c {
                    require(&tx, "task_contexts", c)?;
                }
                changed.push("context");
                c
            }
            None => t.context_id,
        };
        let (assignee_email, assignee_name) = match patch.assignee {
            Some(Some((email, name))) => {
                changed.push("assignee");
                (opt_clean(&Some(email))?, Some(name))
            }
            Some(None) => {
                changed.push("assignee");
                (None, None)
            }
            None => (t.assignee_email, t.assignee_name),
        };
        let assignee_name = assignee_name.filter(|_| assignee_email.is_some());
        tx.execute(
            "UPDATE tasks SET title = ?2, description = ?3, due_at = ?4, start_at = ?5,
                priority = ?6, context_id = ?7, assignee_email = ?8, assignee_name = ?9
             WHERE id = ?1",
            params![
                id,
                title,
                description,
                due_at,
                start_at,
                priority,
                context_id,
                assignee_email,
                assignee_name
            ],
        )?;
        stamp(&tx, "task", "tasks", id, "update")?;
        if !changed.is_empty() {
            activity(&tx, id, "edited", &format!("Edited {}", changed.join(", ")))?;
        }
        let task = fetch_task(&tx, id)?;
        tx.commit()?;
        Ok(task)
    }

    fn delete_task(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "tasks", id)?;
        stamp(&tx, "task", "tasks", id, "delete")?;
        tx.commit()?;
        Ok(())
    }

    fn move_task(&mut self, id: i64, column_id: i64, after_id: Option<i64>) -> Result<MoveOutcome> {
        let tx = self.conn.transaction()?;
        let place = after_id.map_or(Place::Top, Place::After);
        let out = move_task_inner(&tx, id, column_id, place)?;
        tx.commit()?;
        Ok(out)
    }

    fn complete_task(&mut self, id: i64) -> Result<Task> {
        let tx = self.conn.transaction()?;
        let t = fetch_task(&tx, id)?;
        let done: i64 = tx
            .query_row(
                "SELECT id FROM task_columns WHERE project_id = ?1 AND kind = 'done'
                   AND deleted_at IS NULL ORDER BY position LIMIT 1",
                [t.project_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| invalid("project has no Done column"))?;
        let out = move_task_inner(&tx, id, done, Place::End)?;
        tx.commit()?;
        Ok(out.task)
    }

    fn reopen_task(&mut self, id: i64) -> Result<Task> {
        let tx = self.conn.transaction()?;
        let t = fetch_task(&tx, id)?;
        let target: i64 = tx
            .query_row(
                "SELECT id FROM task_columns WHERE project_id = ?1 AND deleted_at IS NULL
                   AND kind != 'done'
                 ORDER BY CASE kind WHEN 'next' THEN 0 WHEN 'inbox' THEN 1 ELSE 2 END, position
                 LIMIT 1",
                [t.project_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| invalid("project has no open column"))?;
        let out = move_task_inner(&tx, id, target, Place::End)?;
        activity(&tx, id, "reopened", "Reopened")?;
        tx.commit()?;
        Ok(out.task)
    }

    fn set_task_labels(&mut self, id: i64, label_ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "tasks", id)?;
        for l in label_ids {
            require(&tx, "task_labels", *l)?;
        }
        tx.execute("DELETE FROM task_label_links WHERE task_id = ?1", [id])?;
        for l in label_ids {
            tx.execute(
                "INSERT OR IGNORE INTO task_label_links (task_id, label_id) VALUES (?1, ?2)",
                params![id, l],
            )?;
        }
        stamp(&tx, "task", "tasks", id, "update")?;
        activity(&tx, id, "labels", "Changed labels")?;
        tx.commit()?;
        Ok(())
    }

    fn list_tasks(&mut self, f: &TaskFilter) -> Result<Vec<Task>> {
        let mut clauses = vec!["t.deleted_at IS NULL".to_owned()];
        let mut args: Vec<Value> = Vec::new();
        if !f.include_done {
            clauses.push("t.completed_at IS NULL".into());
        }
        if let Some(p) = f.project_id {
            clauses.push("t.project_id = ?".into());
            args.push(p.into());
        }
        if let Some(focus) = f.focus {
            clauses.push(focus_sql(focus).to_owned());
            if matches!(focus, TaskFocus::Today | TaskFocus::Upcoming) {
                args.push(f.today_end_ms.into());
            }
        }
        if let Some(c) = f.context_id {
            clauses.push("t.context_id = ?".into());
            args.push(c.into());
        }
        if let Some(l) = f.label_id {
            clauses.push("EXISTS (SELECT 1 FROM task_label_links k WHERE k.task_id = t.id AND k.label_id = ?)".into());
            args.push(l.into());
        }
        if let Some(text) = f.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            clauses.push(
                "(t.title LIKE ? ESCAPE '\\' OR t.description LIKE ? ESCAPE '\\'
                  OR EXISTS (SELECT 1 FROM task_links l
                     JOIN accounts a ON a.email = l.account_email
                     JOIN messages m ON m.account_id = a.id AND m.message_id = l.message_id
                     WHERE l.task_id = t.id AND l.deleted_at IS NULL
                       AND m.subject LIKE ? ESCAPE '\\'))"
                    .into(),
            );
            let pat = like_pattern(text);
            args.extend([pat.clone().into(), pat.clone().into(), pat.into()]);
        }
        let sql = format!(
            "{TASK_SELECT} {LIVE_JOINS} WHERE {} ORDER BY p.position, c.position, t.position",
            clauses.join(" AND ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut tasks = stmt
            .query_map(params_from_iter(args), task_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        attach_labels(self.conn, &mut tasks)?;
        Ok(tasks)
    }

    fn counts(&mut self, today_end_ms: i64) -> Result<TaskCounts> {
        let c = &*self.conn;
        let open =
            "FROM tasks t JOIN task_projects p ON p.id = t.project_id AND p.deleted_at IS NULL
                    WHERE t.deleted_at IS NULL AND t.completed_at IS NULL";
        Ok(TaskCounts {
            inbox: count_focus(c, TaskFocus::Inbox, today_end_ms)?,
            today: count_focus(c, TaskFocus::Today, today_end_ms)?,
            upcoming: count_focus(c, TaskFocus::Upcoming, today_end_ms)?,
            waiting_for: count_focus(c, TaskFocus::WaitingFor, today_end_ms)?,
            from_email: count_focus(c, TaskFocus::FromEmail, today_end_ms)?,
            someday: count_focus(c, TaskFocus::Someday, today_end_ms)?,
            per_project: pairs(
                c,
                &format!("SELECT t.project_id, COUNT(*) {open} GROUP BY t.project_id"),
            )?,
            per_context: pairs(
                c,
                &format!(
                    "SELECT t.context_id, COUNT(*) {open} AND t.context_id IS NOT NULL
                     GROUP BY t.context_id"
                ),
            )?,
            per_label: pairs(
                c,
                "SELECT k.label_id, COUNT(*) FROM tasks t
                 JOIN task_projects p ON p.id = t.project_id AND p.deleted_at IS NULL
                 JOIN task_label_links k ON k.task_id = t.id
                 JOIN task_labels b ON b.id = k.label_id AND b.deleted_at IS NULL
                 WHERE t.deleted_at IS NULL AND t.completed_at IS NULL
                 GROUP BY k.label_id",
            )?,
        })
    }

    fn list_activity(&mut self, task_id: i64) -> Result<Vec<TaskActivity>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, text, at FROM task_activity
             WHERE task_id = ?1 ORDER BY at DESC, id DESC",
        )?;
        Ok(stmt
            .query_map([task_id], |r| {
                Ok(TaskActivity {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    kind: r.get(2)?,
                    text: r.get(3)?,
                    at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn list_contexts(&mut self) -> Result<Vec<TaskContext>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, name FROM task_contexts WHERE deleted_at IS NULL
             ORDER BY name COLLATE NOCASE",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(TaskContext {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    name: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn create_context(&mut self, name: &str) -> Result<TaskContext> {
        let name = clean_name(name.trim().trim_start_matches('@'))?;
        let tx = self.conn.transaction()?;
        let now = now_ms();
        let uid = new_uid();
        tx.execute(
            "INSERT INTO task_contexts (uid, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![uid, name, now],
        )
        .map_err(|e| unique_name_error(e, "context"))?;
        let id = tx.last_insert_rowid();
        stamp(&tx, "context", "task_contexts", id, "create")?;
        tx.commit()?;
        Ok(TaskContext { id, uid, name })
    }

    fn rename_context(&mut self, id: i64, name: &str) -> Result<()> {
        let name = clean_name(name.trim().trim_start_matches('@'))?;
        let tx = self.conn.transaction()?;
        require(&tx, "task_contexts", id)?;
        tx.execute(
            "UPDATE task_contexts SET name = ?2 WHERE id = ?1",
            params![id, name],
        )
        .map_err(|e| unique_name_error(e, "context"))?;
        stamp(&tx, "context", "task_contexts", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn delete_context(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "task_contexts", id)?;
        let tasks: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM tasks WHERE context_id = ?1")?;
            stmt.query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "UPDATE tasks SET context_id = NULL WHERE context_id = ?1",
            [id],
        )?;
        for t in tasks {
            stamp(&tx, "task", "tasks", t, "update")?;
        }
        stamp(&tx, "context", "task_contexts", id, "delete")?;
        tx.commit()?;
        Ok(())
    }

    fn list_labels(&mut self) -> Result<Vec<TaskLabel>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uid, name, colour FROM task_labels WHERE deleted_at IS NULL
             ORDER BY name COLLATE NOCASE",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(TaskLabel {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    name: r.get(2)?,
                    colour: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn create_label(&mut self, name: &str, colour: &str) -> Result<TaskLabel> {
        let name = clean_name(name)?;
        let tx = self.conn.transaction()?;
        let uid = new_uid();
        tx.execute(
            "INSERT INTO task_labels (uid, name, colour, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![uid, name, colour, now_ms()],
        )
        .map_err(|e| unique_name_error(e, "label"))?;
        let id = tx.last_insert_rowid();
        stamp(&tx, "label", "task_labels", id, "create")?;
        tx.commit()?;
        Ok(TaskLabel {
            id,
            uid,
            name,
            colour: colour.to_owned(),
        })
    }

    fn update_label(&mut self, id: i64, name: &str, colour: &str) -> Result<()> {
        let name = clean_name(name)?;
        let tx = self.conn.transaction()?;
        require(&tx, "task_labels", id)?;
        tx.execute(
            "UPDATE task_labels SET name = ?2, colour = ?3 WHERE id = ?1",
            params![id, name, colour],
        )
        .map_err(|e| unique_name_error(e, "label"))?;
        stamp(&tx, "label", "task_labels", id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn delete_label(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        require(&tx, "task_labels", id)?;
        let tasks: Vec<i64> = {
            let mut stmt =
                tx.prepare("SELECT task_id FROM task_label_links WHERE label_id = ?1")?;
            stmt.query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        tx.execute("DELETE FROM task_label_links WHERE label_id = ?1", [id])?;
        for t in tasks {
            stamp(&tx, "task", "tasks", t, "update")?;
        }
        stamp(&tx, "label", "task_labels", id, "delete")?;
        tx.commit()?;
        Ok(())
    }

    fn list_checklist(&mut self, task_id: i64) -> Result<Vec<ChecklistItem>> {
        let mut stmt = self.conn.prepare(&format!(
            "{CHECKLIST_SELECT} WHERE task_id = ?1 AND deleted_at IS NULL ORDER BY position"
        ))?;
        Ok(stmt
            .query_map([task_id], checklist_from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn add_checklist_item(&mut self, task_id: i64, text: &str) -> Result<ChecklistItem> {
        let text = clean_name(text)?;
        let tx = self.conn.transaction()?;
        require(&tx, "tasks", task_id)?;
        let last: Option<String> = tx
            .query_row(
                "SELECT MAX(position) FROM task_checklist_items
                 WHERE task_id = ?1 AND deleted_at IS NULL",
                [task_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let now = now_ms();
        tx.execute(
            "INSERT INTO task_checklist_items (uid, task_id, text, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![new_uid(), task_id, text, after(last.as_deref()), now],
        )?;
        let id = tx.last_insert_rowid();
        stamp(&tx, "checklist_item", "task_checklist_items", id, "create")?;
        stamp(&tx, "task", "tasks", task_id, "update")?;
        activity(
            &tx,
            task_id,
            "checklist_added",
            &format!("Added checklist item: {text}"),
        )?;
        let item = tx.query_row(
            &format!("{CHECKLIST_SELECT} WHERE id = ?1"),
            [id],
            checklist_from_row,
        )?;
        tx.commit()?;
        Ok(item)
    }

    fn update_checklist_item(
        &mut self,
        id: i64,
        text: Option<&str>,
        done: Option<bool>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let task_id = checklist_task(&tx, id)?;
        if let Some(t) = text {
            tx.execute(
                "UPDATE task_checklist_items SET text = ?2 WHERE id = ?1",
                params![id, clean_name(t)?],
            )?;
        }
        if let Some(d) = done {
            tx.execute(
                "UPDATE task_checklist_items SET done = ?2 WHERE id = ?1",
                params![id, d],
            )?;
            activity(
                &tx,
                task_id,
                "checklist_done",
                if d {
                    "Checked an item"
                } else {
                    "Unchecked an item"
                },
            )?;
        }
        stamp(&tx, "checklist_item", "task_checklist_items", id, "update")?;
        stamp(&tx, "task", "tasks", task_id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn delete_checklist_item(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        let task_id = checklist_task(&tx, id)?;
        stamp(&tx, "checklist_item", "task_checklist_items", id, "delete")?;
        stamp(&tx, "task", "tasks", task_id, "update")?;
        tx.commit()?;
        Ok(())
    }

    fn add_link(&mut self, task_id: i64, link: NewLink) -> Result<TaskLink> {
        let account_email = opt_clean(&link.account_email)?;
        let message_id = opt_clean(&link.message_id)?;
        let event_uid = opt_clean(&link.event_uid)?;
        let other = opt_clean(&link.other_task_uid)?;
        let tx = self.conn.transaction()?;
        let me = fetch_task(&tx, task_id)?;
        let key: &Option<String> = if link.kind.is_mail() {
            if message_id.is_none() {
                return Err(invalid("a mail link needs a Message-ID"));
            }
            &message_id
        } else if link.kind.is_task() {
            let uid = other
                .as_deref()
                .ok_or_else(|| invalid("a task link needs the other task"))?;
            if uid == me.uid {
                return Err(invalid("a task cannot link to itself"));
            }
            tx.query_row(
                "SELECT 1 FROM tasks WHERE uid = ?1 AND deleted_at IS NULL",
                [uid],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound("linked task".into()))?;
            &other
        } else {
            if event_uid.is_none() {
                return Err(invalid("an event link needs an event UID"));
            }
            &event_uid
        };
        let existing = tx
            .query_row(
                &format!(
                    "{LINK_SELECT} WHERE task_id = ?1 AND kind = ?2 AND deleted_at IS NULL
                       AND COALESCE(message_id, event_uid, other_task_uid) = ?3"
                ),
                params![task_id, link.kind.as_str(), key],
                link_from_row,
            )
            .optional()?;
        if let Some(l) = existing {
            return Ok(l);
        }
        tx.execute(
            "INSERT INTO task_links (uid, task_id, kind, account_email, thread_id, message_id,
                event_uid, other_task_uid, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                new_uid(),
                task_id,
                link.kind.as_str(),
                account_email,
                link.thread_id,
                message_id,
                event_uid,
                other,
                now_ms()
            ],
        )?;
        let id = tx.last_insert_rowid();
        stamp(&tx, "link", "task_links", id, "create")?;
        activity(&tx, task_id, "linked", "Linked an item")?;
        let l = tx.query_row(&format!("{LINK_SELECT} WHERE id = ?1"), [id], link_from_row)?;
        tx.commit()?;
        Ok(l)
    }

    fn remove_link(&mut self, id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        let task_id: i64 = tx
            .query_row(
                "SELECT task_id FROM task_links WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::NotFound(format!("link {id}")))?;
        stamp(&tx, "link", "task_links", id, "delete")?;
        activity(&tx, task_id, "unlinked", "Removed a link")?;
        tx.commit()?;
        Ok(())
    }

    fn list_links(&mut self, task_id: i64) -> Result<Vec<ResolvedLink>> {
        let links: Vec<TaskLink> = {
            let mut stmt = self.conn.prepare(&format!(
                "{LINK_SELECT} WHERE task_id = ?1 AND deleted_at IS NULL ORDER BY created_at, id"
            ))?;
            stmt.query_map([task_id], link_from_row)?
                .collect::<rusqlite::Result<_>>()?
        };
        links
            .into_iter()
            .map(|l| resolve_link(self.conn, l))
            .collect()
    }

    fn purge_tombstones(&mut self, older_than_ms: i64) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        // Children first so foreign keys never block.
        for table in [
            "task_links",
            "task_checklist_items",
            "tasks",
            "task_columns",
            "task_projects",
            "task_contexts",
            "task_labels",
        ] {
            n += tx.execute(
                &format!("DELETE FROM {table} WHERE deleted_at IS NOT NULL AND deleted_at < ?1"),
                [older_than_ms],
            )?;
        }
        tx.execute("DELETE FROM task_changes WHERE at < ?1", [older_than_ms])?;
        tx.commit()?;
        Ok(n)
    }
}
