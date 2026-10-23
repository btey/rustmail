//! `Core` entry points for tasks. Each call runs the matching [`TaskStore`]
//! method on the mail database; mutations emit [`CoreEvent::TasksChanged`].

use super::*;
use crate::Core;
use crate::events::CoreEvent;
use crate::models::now_ms;

const TOMBSTONE_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;

macro_rules! reads {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty, |$s:ident| $body:expr;)*) => {$(
        pub async fn $name(&self, $($arg: $ty),*) -> Result<$ret> {
            self.db
                .read(move |conn| {
                    let mut $s = SqliteTaskStore::new(conn);
                    $body
                })
                .await
        }
    )*};
}

macro_rules! writes {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty, |$s:ident| $body:expr;)*) => {$(
        pub async fn $name(&self, $($arg: $ty),*) -> Result<$ret> {
            let out = self
                .db
                .write(move |conn| {
                    let mut $s = SqliteTaskStore::new(conn);
                    $body
                })
                .await?;
            self.bus.emit(CoreEvent::TasksChanged);
            Ok(out)
        }
    )*};
}

impl Core {
    /// Seed the default project on first use and purge old tombstones.
    pub async fn tasks_open(&self) -> Result<()> {
        self.db
            .write(|conn| {
                let mut s = SqliteTaskStore::new(conn);
                s.ensure_default_project()?;
                s.purge_tombstones(now_ms() - TOMBSTONE_RETENTION_MS)?;
                Ok(())
            })
            .await?;
        // No event here: the UI calls this on every reload, so announcing it
        // would make each refresh trigger the next one.
        Ok(())
    }

    /// Create a task from a mail thread (Phase 8). The caller archives the
    /// mail or books calendar time afterwards through their own modules.
    pub async fn task_create_from_thread(&self, thread_id: i64, draft: TaskDraft) -> Result<Task> {
        let task = self
            .db
            .write(move |conn| SqliteTaskStore::new(conn).create_from_thread(thread_id, draft))
            .await?;
        self.bus.emit(CoreEvent::TasksChanged);
        Ok(task)
    }

    /// New mail arrived in these threads (see `SqliteTaskStore::note_replies`).
    pub async fn task_note_replies(&self, thread_ids: Vec<i64>) -> Result<Vec<i64>> {
        let touched = self
            .db
            .write(move |conn| SqliteTaskStore::new(conn).note_replies(&thread_ids))
            .await?;
        if !touched.is_empty() {
            self.bus.emit(CoreEvent::TasksChanged);
        }
        Ok(touched)
    }

    /// Long-lived loop: record replies on linked tasks whenever mail arrives.
    pub(crate) async fn task_reply_loop(&self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(CoreEvent::MailNew { thread_ids, .. }) => {
                    if let Err(error) = self.task_note_replies(thread_ids).await {
                        tracing::warn!("task reply tracking failed: {error}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    }

    /// Tasks linked to the given mail threads (all linked threads for `None`).
    pub async fn task_threads(&self, only: Option<Vec<i64>>) -> Result<Vec<ThreadTask>> {
        self.db
            .read(move |conn| SqliteTaskStore::new(conn).threads_with_tasks(only.as_deref()))
            .await
    }

    reads! {
        task_projects() -> Vec<TaskProject>, |s| s.list_projects();
        task_columns(project_id: i64) -> Vec<TaskColumn>, |s| s.list_columns(project_id);
        task_get(id: i64) -> Task, |s| s.get_task(id);
        task_list(filter: TaskFilter) -> Vec<Task>, |s| s.list_tasks(&filter);
        task_counts(today_end_ms: i64) -> TaskCounts, |s| s.counts(today_end_ms);
        task_activity(task_id: i64) -> Vec<TaskActivity>, |s| s.list_activity(task_id);
        task_contexts() -> Vec<TaskContext>, |s| s.list_contexts();
        task_labels() -> Vec<TaskLabel>, |s| s.list_labels();
        task_checklist(task_id: i64) -> Vec<ChecklistItem>, |s| s.list_checklist(task_id);
        task_links(task_id: i64) -> Vec<ResolvedLink>, |s| s.list_links(task_id);
    }

    writes! {
        task_project_create(name: String, colour: String) -> TaskProject,
            |s| s.create_project(&name, &colour);
        task_project_update(id: i64, name: String, colour: String) -> (),
            |s| s.update_project(id, &name, &colour);
        task_project_delete(id: i64) -> (), |s| s.delete_project(id);
        task_column_create(project_id: i64, name: String) -> TaskColumn,
            |s| s.create_column(project_id, &name);
        task_column_update(id: i64, patch: ColumnPatch) -> (), |s| s.update_column(id, patch);
        task_column_reorder(id: i64, after: Option<i64>) -> (), |s| s.reorder_column(id, after);
        task_column_collapse(id: i64, collapsed: bool) -> (),
            |s| s.set_column_collapsed(id, collapsed);
        task_column_delete(id: i64, move_to: i64) -> (), |s| s.delete_column(id, move_to);
        task_create(new: NewTask) -> Task, |s| s.create_task(new);
        task_update(id: i64, patch: TaskPatch) -> Task, |s| s.update_task(id, patch);
        task_delete(id: i64) -> (), |s| s.delete_task(id);
        task_move(id: i64, column_id: i64, after: Option<i64>) -> MoveOutcome,
            |s| s.move_task(id, column_id, after);
        task_complete(id: i64) -> Task, |s| s.complete_task(id);
        task_reopen(id: i64) -> Task, |s| s.reopen_task(id);
        task_set_labels(id: i64, label_ids: Vec<i64>) -> (), |s| s.set_task_labels(id, &label_ids);
        task_context_create(name: String) -> TaskContext, |s| s.create_context(&name);
        task_context_rename(id: i64, name: String) -> (), |s| s.rename_context(id, &name);
        task_context_delete(id: i64) -> (), |s| s.delete_context(id);
        task_label_create(name: String, colour: String) -> TaskLabel,
            |s| s.create_label(&name, &colour);
        task_label_update(id: i64, name: String, colour: String) -> (),
            |s| s.update_label(id, &name, &colour);
        task_label_delete(id: i64) -> (), |s| s.delete_label(id);
        task_checklist_add(task_id: i64, text: String) -> ChecklistItem,
            |s| s.add_checklist_item(task_id, &text);
        task_checklist_update(id: i64, text: Option<String>, done: Option<bool>) -> (),
            |s| s.update_checklist_item(id, text.as_deref(), done);
        task_checklist_delete(id: i64) -> (), |s| s.delete_checklist_item(id);
        task_link_add(task_id: i64, link: NewLink) -> TaskLink, |s| s.add_link(task_id, link);
        task_link_remove(id: i64) -> (), |s| s.remove_link(id);
    }
}
