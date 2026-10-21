-- Filter rules apply to mail that arrives after they exist. Mail already in
-- the profile is marked as handled so adding a rule never rewrites old mail;
-- the manual "run filters" command can still be used on it.
ALTER TABLE messages ADD COLUMN filter_state INTEGER NOT NULL DEFAULT 0;
UPDATE messages SET filter_state = 1;
CREATE INDEX idx_messages_filter_pending ON messages(id) WHERE filter_state = 0;
