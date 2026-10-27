//! `Core` entry points for tasks. Each call runs the matching [`TaskStore`]
//! method on the mail database; mutations emit [`CoreEvent::TasksChanged`].

use super::*;
use crate::events::CoreEvent;
use crate::models::now_ms;
use crate::{Core, CoreError};

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

    /// A task's email was sent (Phase 9): link it, move the task, set the
    /// follow-up time. See `SqliteTaskStore::record_sent`.
    pub async fn task_record_sent(
        &self,
        task_id: i64,
        sent: SentMail,
        after: AfterSend,
        follow_up_at: Option<i64>,
    ) -> Result<Task> {
        let task = self
            .db
            .write(move |conn| {
                SqliteTaskStore::new(conn).record_sent(task_id, sent, after, follow_up_at)
            })
            .await?;
        self.bus.emit(CoreEvent::TasksChanged);
        Ok(task)
    }

    /// Send a draft for a task (Phase 9): queue it through the normal send
    /// path, and once delivery is confirmed link the sent message to the task
    /// (`task_record_sent`). `follow_up_days` sets the follow-up time from the
    /// moment of sending; `follow_up_event` (account id, calendar id) also
    /// books it on the calendar. The link is made by a background waiter, so it is
    /// lost if the app quits between queueing and delivery; a failed or
    /// cancelled send changes nothing.
    pub async fn task_send_and_link(
        &self,
        args: crate::models::QueueSendArgs,
        task_id: i64,
        after: AfterSend,
        follow_up_days: Option<u32>,
        follow_up_event: Option<(i64, Option<i64>)>,
    ) -> Result<crate::models::QueueSendResult> {
        self.db
            .read(move |conn| SqliteTaskStore::new(conn).get_task(task_id).map(|_| ()))
            .await?;
        let queued = self.queue_send(args).await?;
        self.task_link_after_send(&queued, task_id, after, follow_up_days, follow_up_event);
        Ok(queued)
    }

    /// The waiter half of `task_send_and_link`, for a send queued elsewhere
    /// (the compose window queues its own): once delivery of `queued` is
    /// confirmed, link it to the task and book the follow-up.
    pub fn task_link_after_send(
        &self,
        queued: &crate::models::QueueSendResult,
        task_id: i64,
        after: AfterSend,
        follow_up_days: Option<u32>,
        follow_up_event: Option<(i64, Option<i64>)>,
    ) {
        let core = self.clone();
        let (action_id, draft_id) = (queued.action_id, queued.draft_id);
        tokio::spawn(async move {
            if core.wait_for_send(action_id).await.is_err() {
                return;
            }
            match core
                .task_link_sent_draft(task_id, draft_id, after, follow_up_days)
                .await
            {
                Ok(task) => core.bus.emit(CoreEvent::TaskMailSent { title: task.title }),
                Err(e) => {
                    tracing::warn!(task_id, draft_id, "linking the sent mail failed: {e}");
                    return;
                }
            }
            if let (Some((account_id, calendar_id)), Some(_)) = (follow_up_event, follow_up_days)
                && let Err(e) = core
                    .task_create_follow_up_event(task_id, account_id, calendar_id)
                    .await
            {
                tracing::warn!(task_id, "booking the follow-up event failed: {e}");
            }
        });
    }

    pub(super) async fn task_link_sent_draft(
        &self,
        task_id: i64,
        draft_id: i64,
        after: AfterSend,
        follow_up_days: Option<u32>,
    ) -> Result<Task> {
        let sent = self
            .db
            .read(move |conn| {
                let row = crate::db::repo::messages::get_row(conn, draft_id)?
                    .ok_or_else(|| CoreError::NotFound("sent message".into()))?;
                let account_email: String = conn.query_row(
                    "SELECT email FROM accounts WHERE id = ?1",
                    [row.account_id],
                    |r| r.get(0),
                )?;
                Ok(SentMail {
                    account_email,
                    thread_id: row
                        .thread_id
                        .ok_or_else(|| CoreError::NotFound("thread of the sent message".into()))?,
                    message_id: row.message_id.unwrap_or_default(),
                })
            })
            .await?;
        let follow_up_at = follow_up_days.map(|d| now_ms() + i64::from(d) * 24 * 60 * 60 * 1000);
        self.task_record_sent(task_id, sent, after, follow_up_at)
            .await
    }

    /// Book the task's follow-up as a calendar event (Phase 9): a 30 minute
    /// event "Follow up: <title>" at `follow_up_at`, linked to the task. Needs
    /// a follow-up time on the task; `account_id` 0 keeps the event local.
    pub async fn task_create_follow_up_event(
        &self,
        task_id: i64,
        account_id: i64,
        calendar_id: Option<i64>,
    ) -> Result<Task> {
        let task = self
            .db
            .read(move |conn| SqliteTaskStore::new(conn).get_task(task_id))
            .await?;
        let starts_at = task
            .follow_up_at
            .ok_or_else(|| CoreError::Other("this task has no follow-up time".into()))?;
        let event = self
            .create_event(crate::models::CreateEventArgs {
                account_id,
                calendar_id,
                summary: format!("Follow up: {}", task.title),
                description: None,
                location: None,
                join_url: None,
                starts_at,
                ends_at: starts_at + 30 * 60 * 1000,
                all_day: false,
                attendees: Vec::new(),
            })
            .await?;
        let event_id = event.id;
        let uid: String = self
            .calendar_db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT ical_uid FROM calendar_events WHERE id = ?1",
                    [event_id],
                    |r| r.get(0),
                )?)
            })
            .await?;
        self.task_link_add(
            task_id,
            NewLink {
                kind: LinkKind::Event,
                account_email: None,
                thread_id: None,
                message_id: None,
                event_uid: Some(uid),
                other_task_uid: None,
            },
        )
        .await?;
        self.task_get(task_id).await
    }

    /// Like `task_create_from_thread`, but does nothing when the thread
    /// already has a task. The check and the insert share one database write,
    /// so two filter passes cannot both create one.
    pub async fn task_create_from_thread_once(
        &self,
        thread_id: i64,
        draft: TaskDraft,
    ) -> Result<Option<Task>> {
        let task = self
            .db
            .write(move |conn| {
                let mut store = SqliteTaskStore::new(conn);
                if !store.threads_with_tasks(Some(&[thread_id]))?.is_empty() {
                    return Ok(None);
                }
                store.create_from_thread(thread_id, draft).map(Some)
            })
            .await?;
        if task.is_some() {
            self.bus.emit(CoreEvent::TasksChanged);
        }
        Ok(task)
    }

    /// New mail arrived in these threads (see `SqliteTaskStore::note_replies`).
    pub async fn task_note_replies(&self, thread_ids: Vec<i64>) -> Result<Vec<i64>> {
        let replies = self
            .db
            .write(move |conn| SqliteTaskStore::new(conn).note_replies_detailed(&thread_ids))
            .await?;
        if !replies.is_empty() {
            self.bus.emit(CoreEvent::TasksChanged);
        }
        let mut touched = Vec::with_capacity(replies.len());
        for reply in replies {
            touched.push(reply.task_id);
            self.bus.emit(CoreEvent::TaskReplyReceived {
                task_id: reply.task_id,
                title: reply.title,
                subject: reply.subject,
                moved_back: reply.moved_back,
            });
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

    /// Announce follow-ups that came due since the last check.
    pub async fn task_check_follow_ups(&self) -> Result<Vec<i64>> {
        let now = crate::models::now_ms();
        let due = self
            .db
            .write(move |conn| SqliteTaskStore::new(conn).note_follow_ups_due(now))
            .await?;
        if !due.is_empty() {
            self.bus.emit(CoreEvent::TasksChanged);
        }
        let mut ids = Vec::with_capacity(due.len());
        for item in due {
            ids.push(item.task_id);
            self.bus.emit(CoreEvent::TaskFollowUpDue {
                task_id: item.task_id,
                title: item.title,
                days: item.days,
            });
        }
        Ok(ids)
    }

    /// Long-lived loop: check once a minute whether a follow-up came due.
    pub(crate) async fn task_follow_up_loop(&self) {
        // The first check waits a full period so start-up keeps its database
        // connections for the foreground reads.
        let period = std::time::Duration::from_secs(60);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            tick.tick().await;
            if let Err(error) = self.task_check_follow_ups().await {
                tracing::warn!("task follow-up check failed: {error}");
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
        task_no_reply_days(now_ms: i64) -> Vec<(i64, i64)>, |s| s.no_reply_days(now_ms);
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
