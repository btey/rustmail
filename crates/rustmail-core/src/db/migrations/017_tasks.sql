-- Local-first task engine (GTD). Every synced table carries a stable `uid`
-- (UUID, the future iCalendar UID), `created_at`/`updated_at` (UTC ms), a
-- `deleted_at` tombstone and sync bookkeeping, so a sync backend can be added
-- later without reworking the schema. Ordering uses string position keys.
CREATE TABLE task_projects (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  colour TEXT NOT NULL DEFAULT '',
  position TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);

CREATE TABLE task_columns (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  project_id INTEGER NOT NULL REFERENCES task_projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  icon TEXT NOT NULL DEFAULT '',
  colour TEXT NOT NULL DEFAULT '',
  wip_limit INTEGER CHECK (wip_limit IS NULL OR wip_limit > 0),
  position TEXT NOT NULL,
  collapsed INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL CHECK (kind IN
    ('inbox','next','waiting','scheduled','done','someday','custom')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE INDEX idx_task_columns_project ON task_columns(project_id, position);

CREATE TABLE task_contexts (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE UNIQUE INDEX idx_task_contexts_name
  ON task_contexts(name COLLATE NOCASE) WHERE deleted_at IS NULL;

CREATE TABLE task_labels (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  colour TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE UNIQUE INDEX idx_task_labels_name
  ON task_labels(name COLLATE NOCASE) WHERE deleted_at IS NULL;

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  project_id INTEGER NOT NULL REFERENCES task_projects(id) ON DELETE CASCADE,
  column_id INTEGER NOT NULL REFERENCES task_columns(id),
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  due_at INTEGER,
  start_at INTEGER,
  priority INTEGER NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 3),
  context_id INTEGER REFERENCES task_contexts(id) ON DELETE SET NULL,
  assignee_email TEXT,
  assignee_name TEXT,
  position TEXT NOT NULL,
  completed_at INTEGER,
  source TEXT NOT NULL DEFAULT 'manual' CHECK (source IN ('manual','mail')),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE INDEX idx_tasks_column ON tasks(column_id, position);
CREATE INDEX idx_tasks_project ON tasks(project_id);
CREATE INDEX idx_tasks_due ON tasks(due_at) WHERE due_at IS NOT NULL AND deleted_at IS NULL;

CREATE TABLE task_label_links (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  label_id INTEGER NOT NULL REFERENCES task_labels(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, label_id)
) WITHOUT ROWID;
CREATE INDEX idx_task_label_links_label ON task_label_links(label_id);

CREATE TABLE task_checklist_items (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  text TEXT NOT NULL,
  done INTEGER NOT NULL DEFAULT 0,
  position TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE INDEX idx_task_checklist_task ON task_checklist_items(task_id, position);

-- Links hold durable identifiers (account e-mail, Message-ID, event UID, other
-- task uid), never local row ids. `thread_id` is only a cache hint.
CREATE TABLE task_links (
  id INTEGER PRIMARY KEY,
  uid TEXT NOT NULL UNIQUE,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  kind TEXT NOT NULL CHECK (kind IN
    ('mail_received','mail_sent','event','task_blocks','task_depends')),
  account_email TEXT,
  thread_id INTEGER,
  message_id TEXT,
  event_uid TEXT,
  other_task_uid TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  deleted_at INTEGER,
  sync_state TEXT NOT NULL DEFAULT 'dirty' CHECK (sync_state IN ('local','dirty','synced')),
  remote_id TEXT,
  etag TEXT
);
CREATE INDEX idx_task_links_task ON task_links(task_id);
CREATE INDEX idx_task_links_message ON task_links(message_id) WHERE message_id IS NOT NULL;

-- User-visible history (not the sync log). `kind` is a stable token so the UI
-- can localise; `text` is an English fallback.
CREATE TABLE task_activity (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  at INTEGER NOT NULL
);
CREATE INDEX idx_task_activity_task ON task_activity(task_id, at);

-- Change log a future sync engine drains.
CREATE TABLE task_changes (
  id INTEGER PRIMARY KEY,
  entity TEXT NOT NULL,
  uid TEXT NOT NULL,
  op TEXT NOT NULL CHECK (op IN ('create','update','delete')),
  at INTEGER NOT NULL
);
