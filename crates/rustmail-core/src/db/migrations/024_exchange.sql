-- Exchange Online (Microsoft Graph) accounts. `exchange_folders` maps a Graph
-- mail folder to a local folder and keeps the resumable delta link (a
-- `nextLink` while a sync is paged, the final `deltaLink` afterwards).
-- `exchange_messages` maps a Graph message id to the cached local message and
-- the local folder it was last seen in.
CREATE TABLE exchange_folders (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  graph_id TEXT NOT NULL,
  folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  delta_link TEXT,
  PRIMARY KEY (account_id, graph_id)
) WITHOUT ROWID;
CREATE INDEX idx_exchange_folders_folder ON exchange_folders(folder_id);

CREATE TABLE exchange_messages (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  graph_id TEXT NOT NULL,
  message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  PRIMARY KEY (account_id, graph_id)
) WITHOUT ROWID;
CREATE INDEX idx_exchange_messages_message ON exchange_messages(message_id);
