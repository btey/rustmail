-- A hand-set percentage complete (iCalendar PERCENT-COMPLETE). NULL means the
-- timeline works it out from the checklist.
ALTER TABLE tasks ADD COLUMN percent INTEGER CHECK (percent IS NULL OR percent BETWEEN 0 AND 100);
