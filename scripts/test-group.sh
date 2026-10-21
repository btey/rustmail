#!/usr/bin/env bash
# Run only the tests for one or more feature groups, or map changed files to
# groups. The full gate (`cargo test --workspace`, see the verify skill) is still
# the check to run before a release or after several features.
#
#   scripts/test-group.sh list
#   scripts/test-group.sh local filters      # named groups
#   scripts/test-group.sh changed [REF]      # groups touched since REF (default HEAD)
#   scripts/test-group.sh all                # cargo test --workspace
#
# Each group = unit-test module prefixes in rustmail-core (`--lib <prefix>`)
# plus integration test files (`--test <name>`).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"

# name | source path prefixes (for `changed`) | lib filters | integration tests
GROUPS_TABLE='
local|crates/rustmail-core/src/local/|local::|local_mail local_mh
imap|crates/rustmail-core/src/imap/|imap::|first_sync imap_probe offline dovecot_e2e
smtp|crates/rustmail-core/src/smtp/|smtp::|send_e2e
sync|crates/rustmail-core/src/sync/|sync::|first_sync offline
jmap|crates/rustmail-core/src/jmap/|jmap::|
nntp|crates/rustmail-core/src/nntp/|nntp::|nntp
db|crates/rustmail-core/src/db/|db::|
actions|crates/rustmail-core/src/actions.rs|actions::|message_actions trash labels draft_lifecycle
filters|crates/rustmail-core/src/filters.rs crates/rustmail-core/src/autolabel/|filters:: autolabel::|filters
search|crates/rustmail-core/src/search/ crates/rustmail-core/src/embed/|search:: embed::|search_ranking semantic_search
accounts|crates/rustmail-core/src/accounts/ crates/rustmail-core/src/oauth/ crates/rustmail-core/src/sender_identities.rs|accounts:: oauth:: sender_identities::|account_mail_preferences mail_connections mail_profiles oauth_settings sender_identities
mime|crates/rustmail-core/src/mime/ crates/rustmail-core/src/signatures.rs|mime:: signatures::|attachment_staging
calendar|crates/rustmail-core/src/calendar/ crates/rustmail-core/src/caldav/ crates/rustmail-core/src/carddav/ crates/rustmail-core/src/googlecal.rs crates/rustmail-core/src/graphcal.rs|calendar:: caldav:: carddav:: googlecal:: graphcal::|
files|crates/rustmail-core/src/files/|files::|files_stalwart
ai|crates/rustmail-core/src/ai/|ai::|voice_corpus
ui|src/ ui/ tests/|--root|caldav_settings calendar_agenda_interactions focus_visibility mail_deletion_interactions
'

rows() { printf '%s' "$GROUPS_TABLE" | grep -v '^$'; }
row_of() { rows | awk -F'|' -v n="$1" '$1==n'; }

run_group() {
  local row; row=$(row_of "$1")
  [ -n "$row" ] || { echo "unknown group: $1 (try: list)" >&2; exit 2; }
  IFS='|' read -r name _paths libs tests <<<"$row"
  echo "== $name =="
  if [ "$libs" = "--root" ]; then
    for t in $tests; do cargo test -p rustmail --test "$t"; done
    return
  fi
  for f in $libs; do cargo test -p rustmail-core --lib "$f"; done
  for t in $tests; do cargo test -p rustmail-core --test "$t"; done
}

changed_groups() {
  local ref=${1:-HEAD} files
  files=$( { git diff --name-only "$ref" 2>/dev/null; git ls-files -o --exclude-standard 2>/dev/null; } | sort -u ) || true
  if ! git rev-parse --verify -q "$ref" >/dev/null; then
    echo "no git ref '$ref' yet; name the groups explicitly" >&2; exit 2
  fi
  rows | while IFS='|' read -r name paths _ _; do
    for p in $paths; do
      if printf '%s\n' "$files" | grep -q "^$p"; then echo "$name"; break; fi
    done
  done
}

case "${1:-list}" in
  list) rows | awk -F'|' '{printf "%-10s %s\n", $1, $2}' ;;
  all) cargo test --workspace ;;
  changed)
    groups=$(changed_groups "${2:-HEAD}")
    if printf '%s\n' "$(git diff --name-only "${2:-HEAD}")" | grep -qE '^(Cargo\.(toml|lock)|crates/rustmail-core/src/(lib|models|config|error)\.rs)$'; then
      echo "shared files changed: run the full suite (scripts/test-group.sh all)"; exit 0
    fi
    [ -n "$groups" ] || { echo "no mapped group changed"; exit 0; }
    for g in $groups; do run_group "$g"; done ;;
  *) for g in "$@"; do run_group "$g"; done ;;
esac
