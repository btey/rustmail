-- NNTP (news) accounts. Each subscribed newsgroup is a folder; `last_seen` is
-- the highest article number already imported, so a sync only fetches newer
-- articles. Read state lives in `messages.is_read` (the server keeps none).
CREATE TABLE nntp_groups (
  folder_id INTEGER PRIMARY KEY REFERENCES folders(id) ON DELETE CASCADE,
  last_seen INTEGER NOT NULL DEFAULT 0,
  low INTEGER NOT NULL DEFAULT 0,
  high INTEGER NOT NULL DEFAULT 0,
  posting_allowed INTEGER NOT NULL DEFAULT 1
);

-- Which article number each cached message came from. The same article
-- cross-posted to several groups is one row per group.
CREATE TABLE nntp_articles (
  message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
  folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  article_no INTEGER NOT NULL,
  -- 1 while only the headers were imported (very large article).
  partial INTEGER NOT NULL DEFAULT 0,
  UNIQUE(folder_id, article_no)
);
