<div align="center">
  <img src="resources/app-icon/rustmail-masked.png" width="88" alt="Rustmail logo">
  <h1 align="center">
    Rustmail
  </h1>
  <p>
    <strong>English</strong> · <a href="README.zh_Hans.md">简体中文</a>
  </p>
  <div align="center">
    <h3>Email, made fast again</h3>
    <p>Built from the ground up for speed. Rustmail delivers native performance, instant startup, and as little as 80 MB of RAM.</p>
  </div>
  <p>
    <a href="https://rustmail.org">Website</a> ·
    <a href="https://github.com/btey/rustmail/issues">Report an issue</a> ·
    <a href="CONTRIBUTING.md">Contribute</a>
  </p>
  <p>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/btey/rustmail?style=flat&label=license&color=2563eb" alt="License: AGPL-3.0"></a>
    <a href="https://github.com/btey/rustmail/releases"><img src="https://img.shields.io/github/downloads/btey/rustmail/total?style=flat&label=downloads&color=16a34a" alt="Downloads across all releases"></a>
    <a href="https://github.com/btey/rustmail/releases"><img src="https://img.shields.io/github/v/release/btey/rustmail?display_name=tag&include_prereleases&sort=semver&style=flat&label=release&color=0ea5e9" alt="Latest release"></a>
    <a href="https://translate.rustmail.org/engage/rustmail/"><img src="https://translate.rustmail.org/widget/rustmail/svg-badge.svg" alt="Translation status"></a>
  </p>
  <p>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/language-Rust%201.92%2B-dea584?style=flat" alt="Language: Rust 1.92+"></a>
    <img src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20%7C%20macOS-475569?style=flat" alt="Platforms: Linux, Windows, macOS">
  </p>
</div>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-light.png">
  <img src="resources/screenshots/desktop-light.png" alt="Rustmail on macOS showing the unified inbox and message view">
</picture>

Rustmail is a lightweight, native home for your email, calendars, and
contacts. It is engineered to open instantly, stay responsive, and use a
fraction of the memory of a typical web-based mail client.

## ❓ Why Rustmail?

- **Fast from the first click.** A native interface and local-first data path
  get you to your inbox without waiting on a browser runtime.
- **As little as 80 MB of RAM.** Rustmail is deliberately designed to keep
  memory use low, even with a full-featured inbox at your fingertips.
- **Everything in one place.** Move between mail, calendars, and contacts
  without stitching together separate apps.
- **Offline by design.** Your mailbox and calendar are stored locally, so your
  synced data remains useful without a connection.
- **Works with your accounts.** Connect Gmail, Outlook and Microsoft 365, or
  standards-based IMAP/SMTP, JMAP, CalDAV, and CardDAV services.
- **Privacy-conscious defaults.** Remote images are blocked until you allow
  them, helping prevent tracking pixels from reporting when you read a message.
- **Security by architecture.** Email content is never opened in a WebView.
  Rustmail renders HTML and CSS through its own Rust-native pipeline, does
  not execute email scripts, and blocks remote images by default. This avoids
  the embedded-browser attack surface by design.
- **An experimental Rust renderer.** Building an email renderer without a
  browser engine is new territory. Rendering issues are expected, especially
  in complex messages, while compatibility continues to improve.
- **Made for every screen.** Spacious and minimal desktop layouts share the
  same experience as the touch-friendly compact interface.
- **Native and open source.** Built from the ground up with Rust. It is not a
  browser wrapped in a window, and it is released under the AGPLv3.

## 📁 Files and attachments

Browse JMAP/WebDAV storage, search mail attachments, keep files offline, and
preview PDFs, images and text.

## ✍🏻 Account signatures and OpenPGP

Settings → Accounts → Signatures & OpenPGP provides named signatures, separate
new-message and reply defaults, and a composer signature selector. Desktop
OpenPGP/MIME signing and encryption use installed GnuPG 2.x with pinentry for
private-key passphrases. Required encryption blocks delivery when recipient keys
are missing or invalid; protected drafts remain local until Send.

S/MIME and mobile OpenPGP are not currently supported.

## 🧪 Experimental HTML rendering

> [!WARNING]
> HTML email rendering is currently the most experimental part of Rustmail.
> Some messages, especially those with complex or unusual markup and CSS, may
> not render correctly yet.

To keep the client fully native and memory usage around 80 MB, Rustmail
renders email HTML with [Blitz](https://github.com/DioxusLabs/blitz), a Rust
HTML/CSS renderer from the Dioxus team, instead of embedding a browser or
WebView.

Desktop builds include **CPU — Low Memory** and **GPU — WGPU** under
Settings → General → Renderer. CPU is selected initially and does not initialize
WGPU. The GPU option uses Slint and Vello on a shared WGPU 30 device; changing
the setting takes effect after restarting the app. A failed GPU startup falls
back to CPU automatically.

As far as we know, Rustmail is one of the first projects using Blitz for
arbitrary, real-world email HTML. Email markup contains plenty of unusual HTML
and CSS, so this pushes the renderer into demanding territory. We currently
carry several patches on top of Blitz and hope to upstream as much of that work
as possible over time.

If you find an email that renders incorrectly, please
[report it](https://github.com/btey/rustmail/issues). This approach is still
experimental, but it is also a major reason Rustmail can remain so
lightweight compared with WebView-based clients built with frameworks such as
Tauri or Wails.

## 🎨 Make it yours

Choose the workspace that fits the way you handle email. Keep the detailed
three-pane layout, switch to a streamlined minimal view, choose a light or dark
theme, select a built-in color palette or create a custom one, and show or hide
sender avatars.

Message appearance can follow the app theme or stay light or dark independently.
It uses your selected color palette for HTML, plain text, and Reader mode.
To temporarily show the sender's original colors, use the contrast icon between
Find and Reader mode in the desktop message toolbar, or choose **Show original
colors** from the message's three-dot menu on any screen size.

The full workspace keeps your folders, message list, and selected email visible
together. The minimal layout reduces visual noise and gives each part of your
inbox more room when you need it.

### 👥 Profiles and account colors

Group related accounts into named profiles such as Work or Personal, then give
each profile its own color. In Settings → Accounts → Profiles, you can assign
accounts, override an individual account's color, and show the effective color
along the left edge of every message. The markers make accounts easy to tell
apart while working in the unified inbox; the example below uses purple for Work
and orange for Support.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-profiles-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-profiles-light.png">
  <img src="resources/screenshots/desktop-profiles-light.png" alt="Rustmail unified inbox with purple and orange profile color markers">
</picture>

### 🧵 Conversation threads

Replies stay grouped in chronological order, with the active message expanded
inside the reading pane.

Use **Up/Down** to navigate messages, **Home/End** to jump to the first or last
loaded message, and **Page Up/Page Down** to move a page at a time. **Enter**
opens the current message and focuses its content; **Space** checks or unchecks
it for bulk actions. **Tab/Shift+Tab** move between controls. In the full
workspace, **F6** moves focus between the message list and reader. In the minimal
layout, arrow keys highlight messages and **Enter** opens them; **Escape** or
**F6** returns to the list.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-thread-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-thread-light.png">
  <img src="resources/screenshots/desktop-thread-light.png" alt="Rustmail conversation with incoming and replied messages">
</picture>

### 🗂️ Tabs

Keep several places open at once with browser-style tabs. Each tab remembers
its own location: a mailbox, search, or open conversation, or the calendar,
contacts, or files. Middle-click or Ctrl/⌘-click a message to open it in a
background tab (add Shift to switch to it), or choose **Open in new tab** from
its menu. Drag tabs to reorder them, middle-click a tab to close it, and pick
up where you left off the next time you open Rustmail.

**Ctrl/⌘+T** opens a new tab and **Ctrl/⌘+W** closes the current one.
**Ctrl+Tab** and **Ctrl+Shift+Tab** cycle through tabs, **Ctrl/⌘+1–8** jump to
a tab, and **Ctrl/⌘+9** jumps to the last.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-tabs-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-tabs-light.png">
  <img src="resources/screenshots/desktop-tabs-light.png" alt="Rustmail with tabs for several conversations, the calendar, contacts, and files">
</picture>

### Full and minimal workspaces

<table>
  <thead>
    <tr>
      <th width="50%">Full workspace</th>
      <th width="50%">Minimal workspace</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-light.png">
          <img src="resources/screenshots/desktop-light.png" alt="Rustmail full desktop workspace" width="100%">
        </picture>
      </td>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-minimal-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-minimal-light.png">
          <img src="resources/screenshots/desktop-minimal-light.png" alt="Rustmail minimal desktop workspace" width="100%">
        </picture>
      </td>
    </tr>
  </tbody>
</table>

### Color palettes

<table>
  <thead>
    <tr>
      <th width="25%">Teal</th>
      <th width="25%">Green</th>
      <th width="25%">Purple</th>
      <th width="25%">Custom</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td width="25%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-teal-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-teal-light.png">
          <img src="resources/screenshots/desktop-teal-light.png" alt="Rustmail teal palette" width="100%">
        </picture>
      </td>
      <td width="25%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-green-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-green-light.png">
          <img src="resources/screenshots/desktop-green-light.png" alt="Rustmail green palette" width="100%">
        </picture>
      </td>
      <td width="25%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-purple-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-purple-light.png">
          <img src="resources/screenshots/desktop-purple-light.png" alt="Rustmail purple palette" width="100%">
        </picture>
      </td>
      <td width="25%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-light.png">
          <img src="resources/screenshots/desktop-light.png" alt="Rustmail custom palette" width="100%">
        </picture>
      </td>
    </tr>
  </tbody>
</table>

### 🪟 Frameless window

Turn on **Frameless window** in Settings → General → Interface to replace the
system title bar with window controls in the toolbar. Each platform keeps its
own behavior: macOS keeps the native traffic lights, Windows keeps Snap Layouts
and the system menu, and Linux follows the button order and title-bar actions
configured for GNOME or KDE.

<table>
  <tbody>
    <tr>
      <th width="50%">macOS</th>
      <th width="50%">Windows</th>
    </tr>
    <tr>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/frameless/macos-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/frameless/macos-light.png">
          <img src="resources/screenshots/frameless/macos-light.png" alt="Rustmail frameless window on macOS" width="100%">
        </picture>
      </td>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/frameless/windows-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/frameless/windows-light.png">
          <img src="resources/screenshots/frameless/windows-light.png" alt="Rustmail frameless window on Windows" width="100%">
        </picture>
      </td>
    </tr>
    <tr>
      <th width="50%">Linux (GNOME)</th>
      <th width="50%">Linux (KDE)</th>
    </tr>
    <tr>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/frameless/linux-gnome-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/frameless/linux-gnome-light.png">
          <img src="resources/screenshots/frameless/linux-gnome-light.png" alt="Rustmail frameless window on GNOME" width="100%">
        </picture>
      </td>
      <td width="50%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/frameless/linux-kde-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/frameless/linux-kde-light.png">
          <img src="resources/screenshots/frameless/linux-kde-light.png" alt="Rustmail frameless window on KDE Plasma" width="100%">
        </picture>
      </td>
    </tr>
  </tbody>
</table>

### 📅 Calendar, contacts, and files

<table>
  <thead>
    <tr>
      <th width="33.33%">Calendar</th>
      <th width="33.33%">Contacts</th>
      <th width="33.33%">Files (WebDAV/JMAP)</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td width="33.33%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-calendar-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-calendar-light.png">
          <img src="resources/screenshots/desktop-calendar-light.png" alt="Rustmail calendar" width="100%">
        </picture>
      </td>
      <td width="33.33%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-contacts-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-contacts-light.png">
          <img src="resources/screenshots/desktop-contacts-light.png" alt="Rustmail contacts" width="100%">
        </picture>
      </td>
      <td width="33.33%">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-files-dark.png">
          <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-files-light.png">
          <img src="resources/screenshots/desktop-files-light.png" alt="Rustmail files" width="100%">
        </picture>
      </td>
    </tr>
  </tbody>
</table>

### 💾 Storage and backups

Settings → Storage shows how much space mail, attachments, offline files, and
databases use on the device. From the same page, you can export verified database
snapshots or transfer connected-account setup and preferences through a backup.
Passwords and OAuth tokens stay in the system keyring, so a restored device asks
you to sign in again.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="resources/screenshots/desktop-storage-dark.png">
  <source media="(prefers-color-scheme: light)" srcset="resources/screenshots/desktop-storage-light.png">
  <img src="resources/screenshots/desktop-storage-light.png" alt="Rustmail Storage settings with local usage, database snapshot, and account backup controls">
</picture>

### 📱 Made for smaller screens

The compact interface keeps the important actions within reach while giving
messages, events, and contacts the full screen when they need it.

| Light | Dark |
| --- | --- |
| ![Rustmail mobile inbox in light mode](resources/screenshots/mobile-light.png) | ![Rustmail mobile inbox in dark mode](resources/screenshots/mobile-dark.png) |

## 📥 Get Rustmail

Rustmail is currently in development and is not yet stable.

> [!WARNING]
> **Google OAuth verification requires an annual CASA security assessment.**
> Rustmail is a community open-source project, so completing this assessment
> depends on sponsor support. Until the required verification is funded and
> complete, a stable release with Gmail OAuth configured by default will not be
> possible. If you want to help make built-in Gmail sign-in available, please
> [sponsor Rustmail](https://github.com/sponsors/rustmail).
>
> Google and Microsoft OAuth verification is still in progress. Earlier GitHub
> prereleases are intended for testers using their own OAuth registrations or
> IMAP/JMAP accounts.

Builds without OAuth app keys keep Gmail and Outlook sign-in disabled. Use the
**Sign-in settings** cog on the welcome screen to save your own Google or
Microsoft app registration, or connect an IMAP/JMAP account. Custom keys are
shared with Settings and take precedence over bundled keys; clearing a custom
client ID restores the defaults when available. Each provider enables separately
once its configuration is saved.

When preview builds are published, download them from
[GitHub Releases](https://github.com/btey/rustmail/releases) and look for the
**Pre-release** badge:

- **Linux x64:** AppImage, Debian/Ubuntu `.deb`, Fedora 43/44 `.rpm`, or a
  sideloaded Flatpak preview bundle
- **Windows x64:** Setup `.exe` or portable ZIP
- **macOS Apple silicon or Intel (macOS 14+):** DMG or application ZIP

Windows previews are unsigned; macOS previews are ad-hoc signed and not
notarized, so operating-system security prompts are expected. Install updates
manually.

The Flatpak preview is provided as a standalone test bundle. It is not yet a
Flathub package and therefore does not receive automatic Flathub updates. Its
close-to-tray option is disabled until the tray backend can use a sandbox-safe
D-Bus name. Starting automatically at sign-in goes through the desktop's
Background portal and opens the window, because there is no tray to start into.

Install the downloaded Linux package with either
`sudo dnf install ./rustmail-<version>-linux-x64.rpm` or
`flatpak install --user ./rustmail-<version>-linux-x64.flatpak`. Launch the
Flatpak with `flatpak run org.rustmail.mail`.

The RPM targets Fedora 43 and 44 on x86_64. Release builds use Fedora 43 as
their compatibility baseline; the same RPM must pass installation, PDF
rendering, X11 and Wayland startup, and removal checks in clean containers for
both versions before publication. Other RPM-based distributions are not yet
tested. If Discover reports a generic conflict, run the `dnf install` command
above and include its dependency error in your bug report.

The older `0.1.0-alpha.7` RPM was built on Fedora 44 and cannot install on
Fedora 43 because it requires glibc 2.43. Its Flatpak bundle is an alternative
while waiting for an RPM release built with the compatibility checks above.

Each GitHub release includes `SHA256SUMS` and signed build-provenance
attestations. With GitHub CLI installed, verify a download with
`gh attestation verify <download> --repo btey/rustmail`.

You can also build Rustmail from source with the
[Rust toolchain](https://rustup.rs/):

```bash
cargo run --bin rustmail
```

Linux desktop OAuth uses the system browser through the desktop portal and
stores refresh credentials through the freedesktop Secret Service. A normal
desktop session therefore needs an `xdg-desktop-portal` backend and a Secret
Service provider such as GNOME Keyring. Release builds stop before opening the
OAuth page when secure credential storage is unavailable, so an authorization
grant can never be completed without a safe place to persist it.

Podman and Docker development shells commonly have neither the host session
D-Bus nor the host browser's loopback network. Debug builds support that setup
with a clearly marked, owner-only development credential file and an accordion
for pasting the final loopback callback URL. The file backend is excluded from
release builds.

## 🌍 Help translate Rustmail

Rustmail is built for everyone, and we'd love your help making it available in more languages!

We're using [Weblate](https://translate.rustmail.org/) to manage community translations. Whether you'd like to translate Rustmail into your native language, improve an existing translation, or help review translated strings, every contribution is welcome.

**[Start translating Rustmail →](https://translate.rustmail.org/)**

Getting started is easy:

1. Create an account on our translation platform.
2. Select your language, or start a new translation if it isn't available yet.
3. Translate strings directly through Weblate, no programming or GitHub experience required.

Translations are synchronized with our GitHub repository and submitted as pull requests for review. Contributors can also receive GitHub attribution for their work.

## 🌐 Open source, for everyone

Rustmail is one open-source application. There is no separate community
edition. The client is licensed under the
[GNU Affero General Public License v3](LICENSE). Read the
[licensing overview](LICENSING.md) for the practical details, or see
[CONTRIBUTING.md](CONTRIBUTING.md) to help shape the project.

## 🫡 Acknowledgements

Rustmail is made possible by the work of these projects and their
contributors:

- [Slint](https://slint.dev/), the native UI toolkit that powers the Rustmail
  Mail interface.
- [Blitz](https://github.com/DioxusLabs/blitz), the Rust HTML/CSS renderer from
  the Dioxus team that powers the email reading experience.

---

## Origin

Rustmail is a fork of [Flectar Mail](https://github.com/flectar/mail) and is licensed
under the AGPL-3.0-only. It is not produced, sponsored, or supported by Flectar.
See [LICENSING.md](LICENSING.md) and [TRADEMARKS.md](TRADEMARKS.md).
