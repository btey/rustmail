-- A task waiting for a reply can move back to Next actions by itself when
-- mail arrives in a linked thread.
ALTER TABLE tasks ADD COLUMN move_back_on_reply INTEGER NOT NULL DEFAULT 0;
