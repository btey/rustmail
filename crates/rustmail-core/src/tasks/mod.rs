//! Local-first task engine (GTD kanban). Tasks live in the mail database,
//! behind the [`TaskStore`] trait so callers never touch SQL.
//!
//! # Sync readiness (nothing syncs today)
//! Every synced entity has a stable `uid`, UTC millisecond `created_at` /
//! `updated_at`, a `deleted_at` tombstone, and `sync_state` / `remote_id` /
//! `etag` columns. All mutations go through one change path that stamps
//! `updated_at`, sets `sync_state = 'dirty'` and appends to `task_changes`.
//! Tombstones are purged by [`TaskStore::purge_tombstones`] after a retention
//! period. The user-visible `task_activity` history is separate.
//!
//! # Conflict policy (decided, not implemented)
//! Per-field last-writer-wins by `updated_at`; a delete loses to a later edit
//! of the same entity.
//!
//! # iCalendar VTODO mapping
//! title→SUMMARY, description→DESCRIPTION, due→DUE, start→DTSTART,
//! priority→PRIORITY, completed_at→COMPLETED, labels→CATEGORIES,
//! task_blocks/task_depends→RELATED-TO (RELTYPE), checklist→sub-VTODOs
//! (RELATED-TO PARENT) or `X-RUSTMAIL-CHECKLIST`, column→STATUS plus
//! `X-RUSTMAIL-COLUMN`. No standard mapping (would need `X-` properties or a
//! Rustmail-only sidecar): WIP limit, column colour/icon/collapsed, context,
//! assignee display-name snapshot, and mail/event links.

mod api;
pub mod position;
mod store;

pub use store::SqliteTaskStore;

use crate::error::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Inbox,
    Next,
    Waiting,
    Scheduled,
    Done,
    Someday,
    Custom,
}

impl ColumnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ColumnKind::Inbox => "inbox",
            ColumnKind::Next => "next",
            ColumnKind::Waiting => "waiting",
            ColumnKind::Scheduled => "scheduled",
            ColumnKind::Done => "done",
            ColumnKind::Someday => "someday",
            ColumnKind::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => ColumnKind::Inbox,
            "next" => ColumnKind::Next,
            "waiting" => ColumnKind::Waiting,
            "scheduled" => ColumnKind::Scheduled,
            "done" => ColumnKind::Done,
            "someday" => ColumnKind::Someday,
            "custom" => ColumnKind::Custom,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    Manual,
    Mail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    MailReceived,
    MailSent,
    Event,
    TaskBlocks,
    TaskDepends,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::MailReceived => "mail_received",
            LinkKind::MailSent => "mail_sent",
            LinkKind::Event => "event",
            LinkKind::TaskBlocks => "task_blocks",
            LinkKind::TaskDepends => "task_depends",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mail_received" => LinkKind::MailReceived,
            "mail_sent" => LinkKind::MailSent,
            "event" => LinkKind::Event,
            "task_blocks" => LinkKind::TaskBlocks,
            "task_depends" => LinkKind::TaskDepends,
            _ => return None,
        })
    }

    pub fn is_mail(self) -> bool {
        matches!(self, LinkKind::MailReceived | LinkKind::MailSent)
    }

    pub fn is_task(self) -> bool {
        matches!(self, LinkKind::TaskBlocks | LinkKind::TaskDepends)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProject {
    pub id: i64,
    pub uid: String,
    pub name: String,
    pub colour: String,
    pub position: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskColumn {
    pub id: i64,
    pub uid: String,
    pub project_id: i64,
    pub name: String,
    pub icon: String,
    pub colour: String,
    pub wip_limit: Option<i64>,
    pub position: String,
    pub collapsed: bool,
    pub kind: ColumnKind,
    /// Open tasks currently in the column.
    pub task_count: i64,
    /// `task_count` exceeds `wip_limit`. Advisory: moves are never refused.
    pub over_wip: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub id: i64,
    pub uid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLabel {
    pub id: i64,
    pub uid: String,
    pub name: String,
    pub colour: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistItem {
    pub id: i64,
    pub uid: String,
    pub task_id: i64,
    pub text: String,
    pub done: bool,
    pub position: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub uid: String,
    pub project_id: i64,
    pub column_id: i64,
    pub title: String,
    pub description: String,
    pub due_at: Option<i64>,
    pub start_at: Option<i64>,
    /// 0 none .. 3 highest.
    pub priority: i64,
    pub context_id: Option<i64>,
    pub assignee_email: Option<String>,
    pub assignee_name: Option<String>,
    pub position: String,
    pub completed_at: Option<i64>,
    pub source: TaskSource,
    pub created_at: i64,
    pub updated_at: i64,
    pub label_ids: Vec<i64>,
    pub checklist_total: i64,
    pub checklist_done: i64,
    pub link_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLink {
    pub id: i64,
    pub uid: String,
    pub task_id: i64,
    pub kind: LinkKind,
    pub account_email: Option<String>,
    /// Cache hint only; resolution falls back to `message_id`.
    pub thread_id: Option<i64>,
    pub message_id: Option<String>,
    pub event_uid: Option<String>,
    pub other_task_uid: Option<String>,
    pub created_at: i64,
}

/// What a link points at now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LinkTarget {
    Mail {
        account_id: i64,
        thread_id: i64,
        subject: String,
    },
    /// The mail is gone (deleted, or never synced here); the link is kept.
    MailNotFound,
    Event,
    Task {
        task_id: i64,
        title: String,
    },
    TaskNotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedLink {
    pub link: TaskLink,
    pub target: LinkTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskActivity {
    pub id: i64,
    pub task_id: i64,
    pub kind: String,
    pub text: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NewTask {
    pub project_id: i64,
    /// `None` = the project's Inbox column.
    pub column_id: Option<i64>,
    pub title: String,
    pub description: String,
    pub due_at: Option<i64>,
    pub start_at: Option<i64>,
    pub priority: i64,
    pub context_id: Option<i64>,
    pub assignee_email: Option<String>,
    pub assignee_name: Option<String>,
    pub label_ids: Vec<i64>,
    pub from_mail: bool,
}

/// Partial update; an outer `None` leaves the field alone, an inner `None`
/// clears it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub due_at: Option<Option<i64>>,
    pub start_at: Option<Option<i64>>,
    pub priority: Option<i64>,
    pub context_id: Option<Option<i64>>,
    /// `(email, display-name snapshot)`.
    pub assignee: Option<Option<(String, String)>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ColumnPatch {
    pub name: Option<String>,
    pub icon: Option<String>,
    pub colour: Option<String>,
    pub wip_limit: Option<Option<i64>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFocus {
    Inbox,
    /// Open, due on or before the end of today (includes overdue).
    Today,
    /// Open, due after today.
    Upcoming,
    WaitingFor,
    FromEmail,
    Someday,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFilter {
    pub project_id: Option<i64>,
    pub focus: Option<TaskFocus>,
    pub context_id: Option<i64>,
    pub label_id: Option<i64>,
    /// Matches title, description and the subjects of linked mail.
    pub text: Option<String>,
    pub include_done: bool,
    /// End of "today" in UTC ms, computed by the UI from the user's zone.
    pub today_end_ms: i64,
}

/// How the UI groups a task list. Grouping itself is done by the board view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGroupBy {
    Status,
    Project,
    Context,
    Label,
    Due,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCounts {
    pub inbox: i64,
    pub today: i64,
    pub upcoming: i64,
    pub waiting_for: i64,
    pub from_email: i64,
    pub someday: i64,
    /// Open tasks per project: `(project_id, count)`.
    pub per_project: Vec<(i64, i64)>,
    pub per_context: Vec<(i64, i64)>,
    pub per_label: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveOutcome {
    pub task: Task,
    /// The target column now holds more open tasks than its WIP limit.
    pub wip_exceeded: bool,
}

pub trait TaskStore {
    // Projects and columns.
    fn ensure_default_project(&mut self) -> Result<()>;
    fn list_projects(&mut self) -> Result<Vec<TaskProject>>;
    fn create_project(&mut self, name: &str, colour: &str) -> Result<TaskProject>;
    fn update_project(&mut self, id: i64, name: &str, colour: &str) -> Result<()>;
    fn delete_project(&mut self, id: i64) -> Result<()>;
    fn list_columns(&mut self, project_id: i64) -> Result<Vec<TaskColumn>>;
    fn create_column(&mut self, project_id: i64, name: &str) -> Result<TaskColumn>;
    fn update_column(&mut self, id: i64, patch: ColumnPatch) -> Result<()>;
    /// Place the column after `after` (`None` = first).
    fn reorder_column(&mut self, id: i64, after: Option<i64>) -> Result<()>;
    fn set_column_collapsed(&mut self, id: i64, collapsed: bool) -> Result<()>;
    /// Tasks move to `move_to`, which must be another column of the project.
    fn delete_column(&mut self, id: i64, move_to: i64) -> Result<()>;

    // Tasks.
    fn create_task(&mut self, new: NewTask) -> Result<Task>;
    fn get_task(&mut self, id: i64) -> Result<Task>;
    fn update_task(&mut self, id: i64, patch: TaskPatch) -> Result<Task>;
    fn delete_task(&mut self, id: i64) -> Result<()>;
    /// Move to `column_id`, after task `after` (`None` = top of the column).
    fn move_task(&mut self, id: i64, column_id: i64, after: Option<i64>) -> Result<MoveOutcome>;
    fn complete_task(&mut self, id: i64) -> Result<Task>;
    fn reopen_task(&mut self, id: i64) -> Result<Task>;
    fn set_task_labels(&mut self, id: i64, label_ids: &[i64]) -> Result<()>;
    fn list_tasks(&mut self, filter: &TaskFilter) -> Result<Vec<Task>>;
    fn counts(&mut self, today_end_ms: i64) -> Result<TaskCounts>;
    fn list_activity(&mut self, task_id: i64) -> Result<Vec<TaskActivity>>;

    // Contexts and labels.
    fn list_contexts(&mut self) -> Result<Vec<TaskContext>>;
    fn create_context(&mut self, name: &str) -> Result<TaskContext>;
    fn rename_context(&mut self, id: i64, name: &str) -> Result<()>;
    fn delete_context(&mut self, id: i64) -> Result<()>;
    fn list_labels(&mut self) -> Result<Vec<TaskLabel>>;
    fn create_label(&mut self, name: &str, colour: &str) -> Result<TaskLabel>;
    fn update_label(&mut self, id: i64, name: &str, colour: &str) -> Result<()>;
    fn delete_label(&mut self, id: i64) -> Result<()>;

    // Checklist.
    fn list_checklist(&mut self, task_id: i64) -> Result<Vec<ChecklistItem>>;
    fn add_checklist_item(&mut self, task_id: i64, text: &str) -> Result<ChecklistItem>;
    fn update_checklist_item(
        &mut self,
        id: i64,
        text: Option<&str>,
        done: Option<bool>,
    ) -> Result<()>;
    fn delete_checklist_item(&mut self, id: i64) -> Result<()>;

    // Links.
    fn add_link(&mut self, task_id: i64, link: NewLink) -> Result<TaskLink>;
    fn remove_link(&mut self, id: i64) -> Result<()>;
    fn list_links(&mut self, task_id: i64) -> Result<Vec<ResolvedLink>>;

    // Maintenance.
    /// Hard-delete tombstones older than `older_than_ms`; returns rows purged.
    fn purge_tombstones(&mut self, older_than_ms: i64) -> Result<usize>;
}

/// What the user chose in the "New task from email" dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDraft {
    /// `None` = the thread's subject.
    pub title: Option<String>,
    /// `None` = the first project.
    pub project_id: Option<i64>,
    /// The column kind to land in; a project without it uses its Inbox.
    pub status: ColumnKind,
    pub due_at: Option<i64>,
    pub context_id: Option<i64>,
    pub label_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewLink {
    pub kind: LinkKind,
    pub account_email: Option<String>,
    pub thread_id: Option<i64>,
    pub message_id: Option<String>,
    pub event_uid: Option<String>,
    pub other_task_uid: Option<String>,
}

#[cfg(test)]
mod tests;
