# Noto Emoji

This directory contains the unmodified Noto Emoji variable TrueType font used
as Rustmail's deterministic monochrome emoji fallback.

- Family: `Noto Emoji`
- Upstream file: `NotoEmoji[wght].ttf`
- Source: <https://github.com/google/fonts/tree/main/ofl/notoemoji>
- SHA-256: `de6c18832938afc99caf132b39d6a30a19bac7f2e812e28db2535b4608d27551`

The font remains separate from Google Sans Flex. At startup, Rustmail
registers it as Fontique's generic emoji family, which lets Slint and Parley
select it only for emoji clusters in otherwise normal UI text. The outline
font is used because it works with Slint's software renderer on every target.

The font is distributed under the SIL Open Font License 1.1 in `OFL.txt`.
