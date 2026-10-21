-- Rustmail-only labels and snoozes of local-mail messages whose file vanished
-- (for example moved by another program). They are keyed by the Message-ID
-- header and restored when a message with that header is imported again.
CREATE TABLE local_kept_labels (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  header_id TEXT NOT NULL,
  label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
  kept_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, header_id, label_id)
) WITHOUT ROWID;

CREATE TABLE local_kept_snoozes (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  header_id TEXT NOT NULL,
  wake_at INTEGER NOT NULL,
  kept_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, header_id)
) WITHOUT ROWID;
