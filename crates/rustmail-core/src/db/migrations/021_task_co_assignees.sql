-- More people on a task besides its first assignee (`tasks.assignee_*`). The
-- board shows one avatar for each. Local only for now: the iCalendar mapping
-- carries the first assignee.
CREATE TABLE task_co_assignees (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  email TEXT NOT NULL COLLATE NOCASE,
  name TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (task_id, email)
) WITHOUT ROWID;
