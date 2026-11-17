#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
command -v slint-tr-extractor >/dev/null || {
  echo "slint-tr-extractor 1.18.1 is required" >&2
  exit 1
}
command -v msgfmt >/dev/null && command -v msgattrib >/dev/null && command -v msgmerge >/dev/null || {
  echo "GNU gettext is required" >&2
  exit 1
}

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT
cd "$repo_root"

# Preview and screenshot scripts use temporary hidden .slint files.
find ui -type f -name '*.slint' ! -name '.*' -print0 \
  | sort -z \
  | xargs -0 slint-tr-extractor --no-default-translation-context \
      --package-name rustmail --package-version 0.1.0 \
      -o "$work_dir/rustmail.pot"

# Extraction timestamps are intentionally volatile; compare every semantic
# catalog line while ignoring only that generated header field.
diff -u \
  <(sed '/^"POT-Creation-Date:/d' lang/rustmail.pot) \
  <(sed '/^"POT-Creation-Date:/d' "$work_dir/rustmail.pot")

for language in de es fr tr; do
  msgmerge --quiet --no-fuzzy-matching \
    --output-file "$work_dir/rustmail-$language.po" \
    "lang/$language/LC_MESSAGES/rustmail.po" "$work_dir/rustmail.pot"
  diff -u \
    <(sed '/^"POT-Creation-Date:/d' "lang/$language/LC_MESSAGES/rustmail.po") \
    <(sed '/^"POT-Creation-Date:/d' "$work_dir/rustmail-$language.po")
done

# Same extra checks CI used to run on its own, so a local run cannot pass while
# CI fails: valid catalogs and format strings, and nothing untranslated, fuzzy
# or obsolete (reworded strings leave obsolete entries behind).
for language in de es fr tr; do
  catalog="lang/$language/LC_MESSAGES/rustmail.po"
  msgfmt --check --check-header --check-format \
    -o "$work_dir/rustmail-$language.mo" "$catalog"
  for kind in --untranslated --only-fuzzy --only-obsolete; do
    found=$(msgattrib "$kind" --no-obsolete "$catalog")
    [[ $kind == --only-obsolete ]] && found=$(msgattrib --only-obsolete "$catalog")
    if [[ -n $found ]]; then
      echo "$catalog has ${kind#--} entries:" >&2
      printf '%s\n' "$found" | grep -E '^(#~ )?msgid' | head -10 >&2
      exit 1
    fi
  done
done

bash scripts/check-ui-message-catalog.sh
