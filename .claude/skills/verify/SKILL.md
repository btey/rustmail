---
name: verify
description: Run the full Rustmail quality gate (fmt, clippy, UI-message catalog, translations, workspace tests). Use before reporting any change as done.
---

```bash
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=8
cd /home/ben/Projects/rustmail
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-ui-message-catalog.sh
bash scripts/check-translations.sh
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" 
```

Fix real failures; do not weaken tests. Typical causes: a new `UiMessage` key missing in `ui/app.slint`, stale `.pot` after editing a `@tr`, a label too wide for the `text_fit` test, or a schema test needing the new migration. Summarise passed/failed counts honestly.

## Partial runs while developing
`bash scripts/test-group.sh list` shows feature groups (local, imap, filters, ...). `bash scripts/test-group.sh local filters` runs only those groups' unit-test modules and integration files; `bash scripts/test-group.sh changed [REF]` picks the groups from files changed since REF (and says to run everything when shared files like `lib.rs` or `Cargo.toml` changed). Run the full gate above before saying a task is done after several features, and always before a release.
