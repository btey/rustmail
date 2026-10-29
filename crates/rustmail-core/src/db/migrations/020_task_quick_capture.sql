-- A task typed straight into a column (the inline "Add task" and "New task"
-- boxes) is a quick capture until someone moves it out of the Inbox; the board
-- marks it with a "Quick capture" pill.
ALTER TABLE tasks ADD COLUMN quick_capture INTEGER NOT NULL DEFAULT 0;
