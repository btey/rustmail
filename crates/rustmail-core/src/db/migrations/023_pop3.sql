-- POP3 accounts. One row per message the server has offered (by UIDL) that was
-- already handled, so a message is never downloaded twice when it is left on
-- the server. `message_id` is the cached copy; `skipped` marks a message too
-- large to download (only its headers were imported); `downloaded_at` drives
-- "delete from the server after N days".
CREATE TABLE pop3_seen (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  uidl TEXT NOT NULL,
  message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
  skipped INTEGER NOT NULL DEFAULT 0,
  downloaded_at INTEGER NOT NULL,
  PRIMARY KEY (account_id, uidl)
) WITHOUT ROWID;
