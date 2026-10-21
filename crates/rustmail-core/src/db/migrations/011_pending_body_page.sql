-- Only rows still eligible for body fetch need this newest-first planning path.
CREATE INDEX idx_messages_pending_body_page
  ON messages(folder_id, date DESC, id DESC)
  WHERE uid IS NOT NULL AND body_state = 'none';
