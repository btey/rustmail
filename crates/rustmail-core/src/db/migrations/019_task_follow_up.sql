-- When a task sent an email and no reply has come by this time (UTC ms), it
-- shows up under Today as a follow-up.
ALTER TABLE tasks ADD COLUMN follow_up_at INTEGER;
