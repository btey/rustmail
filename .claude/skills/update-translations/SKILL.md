---
name: update-translations
description: Regenerate the gettext catalogs after changing any @tr string in ui/*.slint.
---

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd /home/ben/Projects/rustmail
find ui -type f -name '*.slint' ! -name '.*' -print0 | sort -z \
  | xargs -0 slint-tr-extractor --no-default-translation-context \
      --package-name rustmail --package-version 0.1.0 -o lang/rustmail.pot
for l in de es fr tr; do
  msgmerge --quiet --no-fuzzy-matching --update --backup=none lang/$l/LC_MESSAGES/rustmail.po lang/rustmail.pot
done
bash scripts/check-translations.sh
```

Duplicate msgids: `msguniq --use-first`. New strings stay untranslated unless you translate them; say so in the report.
