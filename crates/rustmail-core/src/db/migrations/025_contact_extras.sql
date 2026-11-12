-- More values per contact (extra e-mail addresses, phones and postal
-- addresses, as a JSON object) and the contact's photo, kept apart from the
-- contact row so lists do not read the picture.
CREATE TABLE contact_extras (
  contact_id INTEGER PRIMARY KEY REFERENCES contacts(id) ON DELETE CASCADE,
  extras TEXT NOT NULL DEFAULT '{}',
  photo BLOB
);
