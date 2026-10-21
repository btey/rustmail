-- Local (Maildir/mbox) accounts: where each cached message lives on disk.
-- `unique_key` is the Maildir unique name (file name without the `:2,flags`
-- suffix) or, for mbox, "<byte offset>:<message-id hash>".
CREATE TABLE local_messages (
  message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  unique_key TEXT NOT NULL,
  rel_path TEXT NOT NULL,
  mbox_offset INTEGER,
  mbox_len INTEGER,
  flags TEXT NOT NULL DEFAULT '',
  UNIQUE(account_id, folder_id, unique_key)
);
CREATE INDEX idx_local_messages_folder ON local_messages(folder_id);

-- Per-folder scan fingerprint so unchanged folders are skipped cheaply.
CREATE TABLE local_folders (
  folder_id INTEGER PRIMARY KEY REFERENCES folders(id) ON DELETE CASCADE,
  fingerprint TEXT NOT NULL DEFAULT '',
  rel_path TEXT NOT NULL DEFAULT '',
  next_uid INTEGER NOT NULL DEFAULT 1
);
