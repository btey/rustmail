---
name: add-backend
description: Checklist for adding a mail backend (like local Maildir/MH or NNTP) to Rustmail.
---

Use `crates/rustmail-core/src/local/` and `nntp/` as templates.

1. Module in `rustmail-core` with an actor started from `sync::engine::spawn_account` (branch on protocol/settings marker). Poll per the account's sync interval.
2. Persist via a new `db/migrations/NNN_*.sql`; update the schema test.
3. Reuse message-insert code (`local/scan.rs`), MIME parsing and threading; mutations via `Core::perform_action` / the action queue.
4. Sending: hook into the existing send dispatcher; keep a Sent copy.
5. Account test/add/edit in `lib.rs`, `src/account_controller.rs`, form in `ui/components/account-setup-form.slint` (short labels for phone width).
6. UI messages in `ui/app.slint`; run `update-translations`.
7. Tests: unit tests plus integration tests against fixtures or an in-process fake server (see `crates/rustmail-core/tests/`).
8. Safety: bounded reads/timeouts, no symlink following, no plaintext fallback, credentials only through the keychain slots.
9. Update `ROADMAP.md` including limits and what was not tested against real servers; then run `verify`.
