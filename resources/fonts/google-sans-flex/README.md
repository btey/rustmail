# Google Sans Flex

This directory contains the unmodified Google Sans Flex variable TrueType font
used for Rustmail's application UI and compose editor.

- Family: `Google Sans Flex`
- Upstream file: `GoogleSansFlex-VariableFont_GRAD,ROND,opsz,slnt,wdth,wght.ttf`
- Source: <https://fonts.google.com/specimen/Google+Sans+Flex>
- Retrieved: 2026-08-25 from the official Google Fonts asset host
- SHA-256: `c6d53424121196b81de816b8daccf200e285dd506df43766db3d7e8cdf06ee30`
- License: SIL Open Font License 1.1; see [OFL.txt](OFL.txt)

The variable font contains weight, width, optical-size, slant, grade, and
roundness axes. Rustmail currently selects the standard family design and uses
the existing UI `font-weight` values. Slint and cosmic-text continue to use
platform fonts when a required glyph is unavailable in this family.

Received email HTML is intentionally excluded. Its typography remains
sender-controlled, with the renderer's independent email fallback stack.
