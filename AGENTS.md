# Rustmail: guide for coding agents

Rustmail is a desktop email client (Rust + Slint UI, Blitz HTML renderer),
forked from Flectar Mail. Goal: the account/preference features of Claws Mail.
No backend services are provided or hosted. Linux first; Windows/macOS kept;
Android/iOS removed. App ID `org.rustmail.mail`.

`ROADMAP.md` is the source of truth for phases and pending work. Update it
whenever you finish or add an item, and state limits honestly.

## Layout
- `src/`, `ui/`: root crate `rustmail` (controllers, Slint UI). `ui/app.slint` is the root.
- `crates/rustmail-core`: mail engine (IMAP `imap/`, SMTP `smtp/`, JMAP, Gmail, `local/` Maildir/mbox/MH, `nntp/`, `sync/engine.rs`, `db/` + `migrations/*.sql`, `actions.rs`, `filters.rs`, `signatures.rs`, `accounts/`).
- `crates/blitz-*`, `parley*`, `candle-core`: vendored and patched. Never edit directly; use `scripts/blitz-patches.sh` / `patches/`.
- `lang/`: gettext catalogs (de es fr tr). `scripts/`: checks, packaging, benchmarks.

## Environment
- Toolchain via mise: `export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"`.
- `.cargo/config.toml` sets `jobs = 1`; override with `CARGO_BUILD_JOBS=8` on a big machine.
- Release build (thin LTO) takes 12-19 min; run it in the background. Dev profile is much slower at runtime.
- `slint-tr-extractor` 1.18.1 and GNU gettext are needed for translations.

## Verify before saying "done"
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `bash scripts/check-ui-message-catalog.sh`
4. `bash scripts/check-translations.sh`
5. `cargo test --workspace` (about 1060 tests, 0 failures expected)
See the `verify` skill. Report failures with output; never claim untested things work.

## Conventions and gotchas
- `UiMessage::plain/detail/arguments/three_arguments` keys must be string literals AND exist as `ui-message-*` callbacks in `ui/app.slint`.
- Any change to a `@tr(...)` string means regenerating `lang/rustmail.pot` and merging the `.po` files (see `update-translations` skill).
- Slint layout: give nested boxes explicit `x`/`y`/height; avoid `alignment: start` on a ScrollView inner box; buttons `width: max(120px, self.content-width + 28px)`; keep labels short, because the `text_fit` test checks phone width.
- Mail list rows: `MailMessage.id` is the thread id; `message_id` is the real message id.
- Mutations go through `Core::perform_action(PerformActionArgs{kind: ActionKind, thread_ids, params})`.
- Non-IMAP backends (Gmail, JMAP, Local, News) plug into `sync::engine::spawn_account`; local/news accounts are stored with protocol `imap` plus a settings marker. See the `add-backend` skill.
- New DB schema = new numbered migration; update the schema test.
- Security: no plaintext fallback after a failed STARTTLS; never follow symlinks or escape account roots in local backends; bound all reads from servers/files; shell commands come only from the user's own settings.
- Credentials live in `crates/rustmail-core/src/accounts/secrets.rs` + `system.rs` (keychain slots). Touch only when the task requires it. If a permission rule denies access, stop and ask; do not work around it.
- Do not make commits unless asked. Never send data to external services.

## Running the app safely
Always use a throwaway profile so real mail data is untouched (see `run-app` skill). Do not point tests or manual runs at real accounts.
