-- Counts the local edits of a contact's server object. A push clears the
-- dirty flag only when the row still has the revision it sent, so an edit made
-- while the push was on its way is not lost.
ALTER TABLE carddav_objects ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
