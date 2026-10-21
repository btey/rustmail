-- Provider rows mirror an authoritative discovery endpoint (Gmail SendAs,
-- JMAP Identity/get, or the authenticated address). Manual rows are addresses
-- the user declared for servers without such an endpoint; a provider refresh
-- never deletes them. Delivery outcome is learned from real submissions
-- because plain SMTP offers no way to ask which From addresses are allowed.
ALTER TABLE sender_identities ADD COLUMN source TEXT NOT NULL DEFAULT 'provider'
  CHECK (source IN ('provider', 'manual'));
ALTER TABLE sender_identities ADD COLUMN delivery_status TEXT NOT NULL DEFAULT 'unknown'
  CHECK (delivery_status IN ('unknown', 'confirmed', 'rejected'));
ALTER TABLE sender_identities ADD COLUMN delivery_error TEXT;
