# Rustmail roadmap: Claws Mail feature parity

Rustmail is a standalone email client with no backend services. The goal is
to offer the account and preference options of an independent client such as
Claws Mail. The Claws option list is from memory of its preferences dialog
and should be re-checked against the current Claws Mail manual.

Status: `todo` · `in progress` · `done` · `blocked` · `optional`

## Phase 0: fork and rebrand — done
- [x] Copy from Flectar Mail, rename to Rustmail (`org.rustmail.mail`)
- [x] Replace legal documents, add origin and attribution notices
- [x] Desktop only: remove Android and iOS platform code, scripts and CI jobs
- [x] Remove the hosted Services settings page
- [x] Replace app icons (new envelope icon: SVG, PNG, ICO, tray). Store screenshots in `resources/screenshots` are still Flectar's
- [ ] Own OAuth client registrations (Google, Microsoft): needs developer accounts; until then supply client IDs via `RUSTMAIL_*_CLIENT_ID` env vars or the custom client ID setting
- [x] Credential store module renamed `accounts::credentials` → `accounts::secrets`
- [x] Regenerated `.pot`/`.po` catalogs (new strings untranslated); Flatpak `cargo-sources.json` pruned to match `Cargo.lock` (full regeneration needs flatpak-cargo-generator)
- [x] Stripped mobile cfg code (`ios` feature, `mobile` alias, android/ios platform files). A few comments and `scripts/stage-pdfium.py` still mention mobile
- [x] Audited: no Flectar/Rustmail-hosted endpoints. AI uses the user's own key and base URL (default OpenRouter, off by default); sender icons only load with remote images enabled; local embeddings feature is off by default

## Phase 1: connection and account settings — done (see notes)
| Item | Status |
|---|---|
| Dev toolchain (Rust stable, clippy, rustfmt) installed | done |
| Baseline `cargo check --workspace --all-targets` of the renamed tree | done |
| Core test suite (`cargo test -p rustmail-core --lib`, 545 tests) | done |
| Security mode "None" (plaintext) with warning and opt-in (IMAP, SMTP, UI) | done; not yet tested against a live plaintext server |
| Custom IMAP/SMTP host and port in account setup UI | done (inherited) |
| Per-account client certificate and private key (key in the system keychain; unit-tested for validation, not against a real mutual-TLS server) | done; passphrase-protected keys are rejected with instructions, because rustls cannot decrypt them |
| SMTP auth method: Auto, PLAIN, LOGIN, None (UI + core; XOAUTH2 stays for OAuth accounts) | done; CRAM-MD5 not available in the SMTP library | 
| Separate SMTP user name | done |
| Separate SMTP password (new `Slot::SmtpPassword`; falls back to the account password) | done; code-reviewed against the keychain backend (slot-generic, delete-all list updated) but not run against a real keychain |
| Connect/IO timeout per account (IMAP connect, SMTP) | done |
| Maximum simultaneous IMAP connections (per-account limit; waits are bounded by the connection timeout, so a limit of 1 slows sync) | done |
| Custom Sent/Drafts/Trash/Junk/Archive folder mapping (applied on next folder discovery; the Queue folder has no counterpart: outgoing mail is queued locally) | done |
| Proxy: SOCKS5 and HTTP CONNECT for IMAP and SMTP (unit-tested with local mock proxies) | done; no proxy login; other traffic (CalDAV, OAuth, JMAP, Gmail/Graph APIs) is not proxied; unencrypted SMTP through a proxy is refused |
| Organization header | done |
| Account display name / sender name (setup and edit) | done |
| Custom outgoing headers (validated), Message-ID domain | done |
| Date header toggle | skipped: RFC 5322 requires Date |
| IMAP auth method: Automatic (LOGIN), LOGIN, PLAIN | done; CRAM-MD5 and DIGEST-MD5 not implemented |

## Phase 2: POP3 backend — todo
- [ ] POP3 client (TLS/STARTTLS/None, USER/PASS, APOP, UIDL)
- [ ] Leave on server, delete after N days, receive size limit
- [ ] Filter on receive, include in "get all"
- [ ] POP-before-SMTP

## Phase 3: filtering, templates, actions — done
Existing before this phase: message **templates** with shortcuts (composer),
inbox **routing rules** (`SplitRule`: sender, recipient, subject, labels,
attachment, automated; they only choose an inbox tab) and AI-planned
automations (`AiAutomationRule`). Signatures were stored text only.

| Item | Status |
|---|---|
| Message templates | done (inherited): name, shortcut, subject and body in the composer |
| Signature from a file or a command | done: re-read or re-run on every compose; 3 s limit, 32 KiB cap, no input; the shell command comes only from the user's own settings. Runs on the UI thread, so it can pause the composer for up to 3 s |
| Filter rules engine: conditions on From, To, Cc, Subject, size, attachment and mailing-list headers (contains, is, starts/ends with, regex, over/under); all-or-any matching; actions mark read/unread, star, archive, spam, trash, move to a folder by name, add a label by name; per-rule stop; ordered; run automatically on new Inbox mail and by hand on every Inbox | done: Settings > Filters. Rules live in settings; matching reads only stored message facts; actions use the normal queued mailbox actions so they sync like a click. Tested (matching, validation, filtering seeded mail, folder moves, no double-filtering, header/body/age/flag conditions, run-command on a stored original). Conditions also cover any named header (empty text with "contains" means "the header exists"), the plain-text body, age in days, read and starred. Further actions: copy to a folder, forward to an address, run a command (`%f` = temporary copy of the message). The editor takes up to 8 conditions and 6 actions, added and removed with buttons. Limits: move, label and flag actions apply to the whole conversation, while copy, forward and run-command act on the matched message; header and body conditions need the original message, which an IMAP account downloads on demand (one server connection per candidate message; messages that cannot match on the other conditions are not fetched, and an unreadable message simply does not match); body conditions use the stored text body when there is one, otherwise the first 4 MiB of the original, and look at 1 MiB of text at most; copy works on IMAP accounts only (it appends to the target folder; it is refused for an Inbox so a copy cannot be filtered again, and other backends log a warning and skip); forward sends the original as an attached message from the account's own SMTP settings without a Sent copy, never to the account's own address, at most 50 per pass, and a failed send is logged and not retried (mail is marked handled before its actions run); run-command has the 60 s limit of user actions and its failure is only logged; the editor can add rules but not edit a saved one; mail that arrived before the upgrade is never filtered automatically; copy, forward and run-command were tested without a server (refusals, validation and a local command), not on a live mailbox |
| Actions: run your own commands on the open message (name + command, `%f` = private temporary copy of the .eml, subject and sender as `$RUSTMAIL_SUBJECT` / `$RUSTMAIL_FROM`) | done: Settings > Actions page, an Actions button in the reader, 60 s limit, output shown in the status line. Unit and integration tests cover quoting, injection, timeout and the private copy. With checked messages in the list, the action runs once per checked conversation's newest message (up to 100, one after the other; the status line reports how many failed); otherwise on the open message. Works on messages already downloaded; not yet used against a live mailbox |
| External editor for composing | done: the command is set in Settings > Actions (`%f` = the text file), and a pencil button appears in the composer toolbar. The composer text is replaced when the editor exits; failure leaves it unchanged. Typing in the composer while the editor is open is overwritten when it closes. The composer button is not verified on screen yet |

## Phase 4: privacy — todo
- [x] S/MIME sign, encrypt, verify, decrypt via GnuPG's `gpgsm`: per-account "Use S/MIME" switch reuses the sign/require-encryption settings with certificate SHA-1 fingerprints; sends multipart/signed (detached, SHA-256) and enveloped-data (sign+encrypt nests), and the reader auto-detects S/MIME (detached, opaque signed-data, enveloped-data). Certificates are imported from .p12/.pem via gpgsm; the chain must end in a root GnuPG trusts. Limits: revocation is not checked (CRL/OCSP lookups are disabled so nothing contacts third parties); GnuPG may show its own "trust this root?" prompt while verifying an unknown root; encryption is refused when a Bcc is present (CMS lists every recipient certificate); the signer's email is not compared with From; no automatic decrypt on open; S/MIME-encrypted-only mail has no integrity protection (the reader says so); messages need the original MIME (the bare smime.p7m attachment is not read); no certificate generation or key-usage/policy editing in the UI
- [x] PGP inline: per-account "Use inline PGP" switch; clearsign, encrypt, or sign+encrypt into the plain-text body (quoted-printable), and inline blocks are decrypted/verified in the OpenPGP reader. Limits: messages with attachments or HTML-only bodies are refused when inline is on; only the first armored block is read; no automatic decrypt on open (use "Decrypt / verify OpenPGP")
- [x] Default privacy system per account; sign/encrypt on reply: one "Privacy system" selector per account (OpenPGP PGP/MIME, OpenPGP inline, S/MIME) replaces the two separate switches (stored settings are unchanged). "Sign replies to signed mail" and "Encrypt replies to encrypted mail" raise the account defaults when a reply is queued: the original's top level is classified (PGP/MIME, S/MIME, inline armor) without decrypting. Limits: the reply always uses the account's own system, not the original's; an original that is signed only inside an encrypted payload counts as encrypted (so it is signed too); the original must be stored locally (otherwise the account defaults apply); the composer's security line does not show the reply rules; a missing signing key is only reported when sending; the rules were tested on classification and policy, not against a live reply

## Phase 5: other backends — optional
| Item | Status |
|---|---|
| Local Maildir / Maildir++ accounts (`MailProtocol::Local`, UI choice "Local folders") | done: import with flags (S/R/F/D/T), nested and Maildir++ folders, folder roles from names or per-account overrides, rescan every 20 s plus on demand |
| Maildir write-back | done: read/unread and star rename the file, move/archive/trash/spam move it, delete and empty-trash unlink it, sent copies are filed in the Sent Maildir; Trash, Sent and Drafts folders are created if missing; symlinks are never followed and every write is checked to stay inside the account root |
| mbox accounts (a single file or a directory tree of mbox files, `.sbd` supported) | done, read and write: mboxrd `>From` unescaping, `Status`/`X-Status` read as initial flags; read/unread and star rewrite those two headers, move/trash/archive/spam append to the target file then cut the message from the old one, delete and empty-trash cut messages out, sent copies are appended to the Sent file; Trash, Sent and Drafts files are created in a directory tree; untouched messages keep their exact bytes; a file over 1 GiB is refused; labels and snoozes stay in Rustmail only |
| MH folders (numbered message files, nested folders) | done: read and write-back; read/starred marks stored in each folder's `.mh_sequences` (`unseen`, `flagged`; other sequences preserved); move/trash/archive/delete, sent copies and created Trash/Sent/Drafts work; symlinks never followed. Claws Mail's binary `.claws_mark` is not read, replied/draft flags are not stored, and a number freed by deleting the newest message can be reused |
| Sending from local accounts | done through the optional SMTP server of the account (all Phase 1 SMTP options apply); without a server, Send is refused with a message |
| NNTP (news) | done: own client (plain, implicit TLS, STARTTLS that never falls back to plaintext, AUTHINFO USER/PASS, proxy, per-account untrusted-certificate option); subscribed groups are folders; incremental import by article number (`OVER`, then `XOVER`, then per-article fallback), newest 300 articles on first read; threading by `References`; read/starred state kept locally; expired articles removed (starred kept); articles over 1 MiB import header-only and download when opened; posting by `POST` (new articles to `group.name@news`, follow-ups to the group they answer, honouring `Followup-To`), Sent copy kept locally. Limits: no per-article cancel/supersede, no cross-post duplicate collapsing (one copy per group), no `Followup-To: poster` private reply, no moderation/approval headers, no yEnc/binary decoding or multipart assembly, no group browser in the UI (`Core::list_news_groups` exists), SMTP is not used by news accounts, read marks are not shared with other newsreaders (no `.newsrc`), tested only against an in-process fake server |

### Phase 5 follow-ups — todo
- [x] mbox write-back (flags, move, delete; see the mbox limits below)
- [ ] News: group browser in the UI (using `Core::list_news_groups`), Newsgroups field in the composer, cancel/supersede, `Followup-To: poster` reply, `.newsrc` import/export
- [x] Local mail: labels/snoozes survive a message moved by another program (kept by Message-ID for 30 days, restored on re-import; see limits below)
- [x] Local mail: new-mail notifications and contact learning (see limits below)
- [ ] Local mail: import Claws `.claws_mark`, avoid the second cached copy
- [ ] Try local (Maildir/mbox/MH) and news accounts in the real app and against real programs and servers (Dovecot, mutt, nmh, Claws, a live news server); only fixtures and a fake server so far

## Phases 6-11: Tasks module and mail-to-task — Phase 6 done, 7-11 todo

Source: four HTML design prototypes in `/home/ben/Projects/` (reference mockups, not code to copy; replicate their values in Slint): `Tasks · Board (GTD kanban)-html`, `Tasks · Timeline (Gantt)-html`, `Mail · Create task from email-html`, `Task · Send email from task-html`. Each folder has one `*.dc.html` whose inline styles and `rmTheme()` carry the colours, sizes and spacing. Work through the phases in order; each phase should leave the app building and verified (see AGENTS.md checklist).

Product decisions taken from the designs (change them here first if they are wrong):
- A new top-level product "Tasks" next to Mail, Calendar, Files and Contacts (`active_view == "tasks"`), icon in the product rail, own tab titled "Tasks · <project>".
- GTD model: statuses Inbox, Next actions, Waiting for, Scheduled, Done, plus a collapsible Someday / Maybe column; projects, contexts (`@computer`), labels, checklist, due date, optional start date, priority flag, description, activity log. Statuses are user-editable columns (name, icon, colour, optional WIP limit); the five above are the defaults.
- Tasks are local-first and stored in Rustmail's own database. No backend service is provided. Assignees are local names or contacts (no multi-user sync); the avatars in the designs are initials of local contacts.
- A task links to mails (received or sent, by thread id and Message-ID), calendar events, and other tasks (blocks / depends on).
- The design shows "Dependabot…" and other sample data only as examples; none of it is shipped.

Decisions (confirmed by the user):
- Local-first, no backend services.
- The ⌘T shortcut in the design was only a sample from the designer. Choose a free shortcut when Phase 8 is implemented (Ctrl+T is probably taken by new tab), make it configurable and list it with the other shortcuts in settings.
- Tasks do not sync now, but the code must be sync-ready so a sync backend can be added later without reworking the model, the UI or the migrations (see "Sync-ready architecture" in Phase 6).
- Assignee is picked from the contacts (CardDAV and learned contacts), stored locally as the contact's email plus a display-name snapshot so it survives contact changes; no free text and no multi-user sync. Phase 7 builds the picker (reuse the contacts model and the composer's address autocomplete).

### Phase 6: task engine (`rustmail-core`) — done (see limits)
| Item | Status |
|---|---|
| Migration `017_tasks.sql` (+ update the schema test): `task_projects`, `task_columns` (status columns: id, project_id, name, icon, colour, wip_limit, position, collapsed, kind inbox/next/waiting/scheduled/done/someday/custom), `tasks` (id, project_id, column_id, title, description, due_at, start_at, priority, context_id, assignee, position, completed_at, created_at, updated_at, source `manual`/`mail`), `task_contexts`, `task_labels` + `task_label_links`, `task_checklist_items`, `task_links` (task_id, kind mail_received/mail_sent/event/task_blocks/task_depends, account_id, thread_id, message_id, event uid, other task id, created_at), `task_activity` (task_id, text, at) | done |
| Rust types in `models.rs`/new `tasks/` module: `Task`, `TaskProject`, `TaskColumn`, `TaskLink`, `TaskFilter`, `TaskGroupBy` | done |
| `Core` API: create/update/delete/move task (column + position, WIP limit reported not enforced), complete/reopen, reorder columns, collapse column, CRUD for projects, contexts, labels, checklist items, links; activity entries written by every mutation | done |
| Queries: by project, by focus list (Inbox, Today, Upcoming, Waiting for, From email, Someday), by context, by label, text search over title/description and linked mail subjects; counts for the sidebar | done |
| Linked-mail resolution: a link survives mail moves (resolve by Message-ID when the thread id is gone); a deleted mail leaves a "mail not found" link instead of breaking the task | done |
| Events through `events.rs` so UI controllers refresh when tasks change | done |
| Seed: on first open create the default project "Personal" and the default columns; no sample data | done |
| Sync-ready architecture (no sync is built now): every synced table (projects, columns, tasks, contexts, labels, checklist items, links) gets a stable `uid` (UUID, also the future iCalendar `UID`), `created_at`, `updated_at`, a soft-delete `deleted_at` tombstone (hard purge only after a retention period) and a `sync_state` column (`local`, `dirty`, `synced`) plus nullable `remote_id` / `etag` columns; ordering uses fractional or string position keys (not dense integers) so reorders touch one row; timestamps are UTC with the user's zone applied only in the UI | done |
| Storage behind a `TaskStore` trait (SQLite implementation now) so the UI and `Core` API never touch SQL directly; mutations go through one change path that stamps `updated_at`, sets `dirty` and appends to a `task_changes` log (entity, uid, op, at) that a future sync engine can drain, as `sync::engine::spawn_account` does for mail; the activity log stays separate (it is user-visible history, not the sync log) | done |
| Links store durable identifiers (account, Message-ID, thread key, event UID) rather than local row ids, so they stay valid on another device or after a re-import | done |
| Model kept mappable to iCalendar VTODO (title→SUMMARY, description, due→DUE, start→DTSTART, status/column→STATUS + a category or `X-` property, priority→PRIORITY, checklist→sub-VTODOs or `X-` items, labels→CATEGORIES, dependencies→RELATED-TO); document in `tasks/` which fields have no standard mapping (WIP limit, column colour/icon, links) and would need `X-` properties or a Rustmail-only sidecar | done |
| Conflict policy decided and written down for the future (per-field last-writer-wins by `updated_at`, deletes lose to edits); not implemented | done |
| Tests: migration, CRUD, move/reorder, WIP count, counts query, link resolution, filter/search, tombstones and change-log entries | done |
| Limits (see below) | done |

Limits and notes:
- WIP limits are advisory: `move_task` reports `wip_exceeded`, never refuses. Counts cover open tasks only.
- Local only, no multi-user, nothing syncs. `sync_state`, `remote_id`, `etag` and `task_changes` are only written/ready; no engine drains them. `task_changes` and tombstones are purged after 90 days (`Core::tasks_open`), so a future sync must treat `sync_state = 'dirty'` rows as the source of truth after a long offline gap.
- Tasks live in the mail database (migration 017). Conflict policy is documented in `tasks/mod.rs`, not implemented.
- Position keys are base-36 strings; repeated appends grow them slowly and there is no rebalance pass.
- A task moves only within its project (no cross-project move yet). Deleting a column requires a target column for its tasks.
- Links store account e-mail + Message-ID (thread id is only a hint). `task_blocks`/`task_depends` are stored on one side only, with no cycle check or inverse listing (Phase 10). Text search matches linked mail subjects only for links that carry an account e-mail.
- Activity entries have a stable `kind` plus an English fallback `text`; the UI should localise by `kind` in Phase 7.
- `TaskGroupBy` is only the type; the board view does the grouping. "Today" uses `today_end_ms` supplied by the UI (zone applied only there).
- Default column icons/colours are placeholders; Phase 7 should match them to the designs.
- The Tasks view consumes `Core::task_*` and `CoreEvent::TasksChanged` (Phase 7).
- Tests: `tasks/tests.rs` (16 task/schema tests incl. position keys, links, search, tombstones, change log).



### Phase 7: Tasks module and board view — in progress (first slice done)
Design: `Tasks · Board (GTD kanban)-html/Main.dc.html`.
| Item | Status |
|---|---|
| Product rail: add Tasks item (icon, label, `product-changed("tasks")`), `active_view == "tasks"` plumbing, Escape/back handling like files/contacts, compact (phone-width) layout without the sidebar | done (rail item, compact nav, Escape/back; not yet checked on a real window) |
| New `ui/views/tasks-view.slint` plus a `src/tasks_controller.rs` (own `TasksUi` global like `FilesUi`); register the `ui-message-*` callbacks it uses | done (`TasksUi` global, one `command` callback; no `ui-message-*` callbacks needed) |
| Tab: "Tasks · <project>" tab in the title bar, reuse `tabs.rs` | done (live title "Tasks · <project>"; inactive tab shows "Tasks") |
| Sidebar (230 px): primary "New task" button; search "Find list, project or context"; sections FOCUS (Inbox, Today, Upcoming, Waiting for, From email, Someday / Maybe) with count badges (Inbox badge in accent colour), PROJECTS (colour dot, count, "New project"), CONTEXTS (`@computer`, ...); selected row uses the accent-soft background | partial: New task, FOCUS with counts, PROJECTS with New project, CONTEXTS with New context. Sidebar search filters projects and contexts by name (the Focus rows always show; new string untranslated). Missing: accent-coloured Inbox badge |
| Toolbar: sidebar toggle, project title + task count, view switch Board / Timeline / List (Timeline = Phase 10, List = simple sortable list), search "Search tasks and linked mail", "Group: Status" (also by project, context, label, due), "Display" menu, overflow menu | partial: sidebar toggle, title + task count, search, and a Board / List button in the chip row for projects (List = one flat list of open tasks with each card's column name; no sorting, no table, not on screen yet, new strings untranslated). A Group menu on flat lists (List mode, Today, context, search): none, status (projects only) or project, context, label, due date (Overdue / Today / Next 7 days / Later / No due date). Sections are shown as read-only columns (no drag, no column menu); a task with several labels shows in each label's section; section names are English; not on screen yet. The board itself cannot be grouped. Missing: Timeline (Phase 10), Display and overflow menus |
| Filter chip row (Assignee, Label, Linked to ...), "Add filter", removable chips, date + "GTD workflow" caption on the right | partial: "Add filter" menu and removable chips for Label and Context (engine filters, reset when the list changes; not on screen yet, new strings untranslated). No Assignee / Linked-to chips, no date + caption |
| Columns: 272 px wide, header with icon, name, count badge, "limit N / M" when a WIP limit is set, add-task (+) and column options (rename, colour, icon, WIP limit, collapse, delete); horizontal scroll; collapsed column (46 px, vertical title, count) for Someday / Maybe; "Add column" | partial: 272 px columns, name, count (`n / limit` with a WIP limit), collapse/expand, horizontal scroll. Add column works. Column options menu (⋯): rename, task limit, move left/right, five preset colours, delete (custom columns only; its tasks go to the first other column). Not on screen yet; no icon choice, no free colour picker; new strings untranslated |
| Cards: label chips, title (struck through when done, 75 % opacity), linked-item rows (Mail / Sent / Event icon + kind + text), status pill (e.g. "No reply · 2 days" warn, "Reply received" ok, "Quick capture" neutral), due date (warn colour when today or overdue), checklist count, High-priority flag, avatars; selected state with accent border and ring | partial: complete toggle, title, labels, due (warn colour when due today/overdue), checklist and link counts, High flag, context, delete. Click opens the drawer; the open card has an accent border. Assignee avatar (initials) and a "From email" mark are shown (new string untranslated). Missing: linked-item rows, status pills, strike-through (Slint 1.18 `Text` has no strike-through; done cards are dimmed and show the filled tick) |
| Drag and drop of cards between and inside columns (updates status, position, `completed_at`); keyboard alternative (move to column menu) because Slint DnD is limited | partial: a card can be dragged onto another column (Slint `DragArea`/`DropArea`, payload built by host callbacks) and lands at the end of that column; the card menu stays as the keyboard path. Not tried on screen yet (compiles only); no drop position inside a column, no reordering within a column, no drop highlight |
| Inline "Add task" at the bottom of a column and in the column header; Enter creates, Esc cancels | partial: works in the column header and from New task; Enter creates, Esc cancels |
| Detail drawer (392 px, overlay at phone width): breadcrumb "Project / Status", expand-to-tab, more actions, close; editable title; labels with add; property grid (Status, Due, Project, Context, Assignee) each opening a picker (date picker reuses the calendar one); description; LINKED ITEMS list with kind, title, sub-line, optional status pill and a "Link" button (link a mail via the mail search, link an event, link a task); "Move back to Next actions when a reply arrives" checkbox in Waiting for; checklist with progress; activity log; footer with primary action ("Reply from task" / "Email from task" / "Follow up by email"), "Schedule" and "Complete" | partial: 392 px side panel (full width at phone width) with breadcrumb, move-to-column menu, close, editable title, due date (typed `YYYY-MM-DD`, plus a ⋯ quick menu: Today, Tomorrow, In a week, No date; no calendar picker), description, Save, Complete/Reopen, High priority toggle, checklist (add/check/remove), LINKED ITEMS ("Not found" for lost mail; each link can be removed; clicking a linked task opens it; a task link is added by typing part of the other task's title, first open match, with a Depends on / Blocks switch) and activity log, Delete. Assignee is set by typing a name or email (matched against contacts; empty clears; no dropdown; untranslated). Missing: calendar date picker, linking mail and events, opening a linked mail thread or event, "move back when a reply arrives", footer email actions (Phase 9) |
| Clicking a linked mail opens that thread in Mail; a linked event opens the calendar event; a linked task selects it | todo |
| Theme: use the existing Rustmail theme tokens; the designs' dark/light and teal/green/purple presets map to the app's own theme setting (do not add a second theme system) | done for this slice (only `AppTheme` tokens; the design's colours are not matched) |
| Translations: all strings in `@tr`, regenerate catalogs (`update-translations` skill) | partial: new `@tr` strings are in the catalogs; Spanish and Russian are translated (CI requires both complete), German, French, Turkish and Chinese are untranslated |
| Tests: `text_fit` at phone width, controller tests for create/move/complete, on-screen smoke test | partial: controller tests run the commands against a throwaway core (columns: rename, limit, colour, move, delete rules; card move as sent by a drop; quick due date; assignee parsing; label chips filtering the board). `text_fit` renders a "tasks board" and a "tasks drawer" state (long names, desktop and phone, every translation); it caught and led to a fix of the card's clipped bottom row. Missing: create/complete/checklist commands, an on-screen smoke test, `text_fit` states for the list view, empty board and the column menu |

First-slice limits:
- Cards move between columns by dragging onto a column (untested on screen, always dropped at the end) or through the card menu; no reordering inside a column.
- Focus, context and search views show one flat list instead of columns.
- Done tasks are all listed in the Done column (no paging).
- Dates are shown as `YYYY-MM-DD`; default column names are stored in English and not translated.
- The board reloads after each command and when `CoreEvent::TasksChanged` arrives while the Tasks view is open (a change made in the view causes one extra reload; not exercised by a test, nothing outside the view writes tasks yet). `tasks_open` no longer emits the event, so a reload cannot trigger itself.
- Filter chips cover Label and Context only; the drawer edits context and labels (⋯ pickers; a new label is created by name, no colour choice; not on screen yet; new strings untranslated), but the assignee cannot be edited in the UI yet. Activity entries are shown in English as stored.

### Phase 8: create a task from an email (Mail module) — todo
Design: `Mail · Create task from email-html/MailToTask.dc.html`.
| Item | Status |
|---|---|
| "Create task" toolbar button in the reading pane (accent-highlighted while the dialog is open), shortcut from the open question above, entry in the message context menu and in the multi-select toolbar (one task per thread) | todo |
| New dialog `ui/dialogs/task-from-mail-dialog.slint` "New task from email": title prefilled from the subject and editable, linked-mail chip (sender · subject, "Linked to the whole thread · later replies show on the task", remove-link button), "Add to" segmented choice Inbox / Next actions / Waiting for / Scheduled, field rows Due, Project, Context (pickers, defaults from a settings rule), Labels (copied from the mail's labels / folder, editable), checkbox "Block time in Calendar" with a date/time slot and target calendar, checkbox "Archive email once the task exists", footer hint "Enter to create · Esc to cancel", Cancel / Create task; at phone width the dialog uses 12 px side margins | todo |
| Create path: `Core::create_task_from_thread(thread_id, TaskDraft)` creating the task, the mail link by thread id + Message-ID, an activity entry "Created from email", optionally a calendar event (through the calendar module) and an Archive action via `Core::perform_action` | partial: `Core::task_create_from_thread(thread_id, TaskDraft)` creates the task (title defaults to the subject; project, status column kind, due, context, labels), links the newest message by Message-ID with account and thread id, and logs "Created from email"; a thread whose message has no Message-ID is refused and leaves no task. Not done: calendar event, archive-on-create, undo as one step, any UI (the caller archives or books time itself) |
| After creation: inline banner under the mail header "Linked task: <title> · <status> · <due>" with "Open on board" and "Undo" (undo deletes the task, its event and un-archives); the thread keeps working with later replies shown on the task | todo |
| Message list: per-row task chip (status name: Inbox, Next actions, Done ...) with the amber task icon | partial: engine only, `Core::task_threads(only)` returns, per linked thread, the newest task with its column name, kind and due date (found through the Message-ID, so re-threaded mail keeps it). No chip in the list yet |
| Mail sidebar smart folders "With tasks" and "Waiting on reply" with counts (virtual folders over the task links; "Waiting on reply" = threads linked to a Waiting-for task with no reply yet) | partial: engine only, `task_threads(None)` lists every linked thread, with `waiting_on_reply` set for a Waiting-for task when no incoming message is newer than the link. No sidebar folders yet; a reply sent from another client counts once it syncs |
| Reading-pane tool strip aligned with the design (Archive, Star, Mark unread, Report spam, Delete, Move to folder, Create task) | todo |
| Rules and filters: filter action "Create task" (Phase 3 filters) with project/status/context parameters; limit: runs on receive only | todo |
| Reply tracking: when a new message arrives in a linked thread, record it in the task activity and, if the task is in Waiting for with "move back when a reply arrives" on, move it to Next actions and update the status pill ("Reply received") | todo |
| Tests: dialog default values, create/undo round trip, archive-on-create, reply-tracking against the local backend fixtures, text_fit | todo |

### Phase 9: send email from a task — todo
Design: `Task · Send email from task-html/TaskCompose.dc.html`.
| Item | Status |
|---|---|
| Task page (opens in its own tab from the drawer's "Email from task" / "Reply from task" / "Follow up by email" or the expand button): left column with breadcrumb "Project / Status", "Show on timeline", title, property list (Status, Due, Project, Context, Assignee, Labels), description, LINK TRAIL (vertical timeline: task created, dependency added, draft in progress, email sent, follow-up event, reply from X with pending items greyed out), checklist | todo |
| Right column composer "New email from task" (becomes "Email from task" after sending): From (account/identity picker, reuse sender identities), To, Subject (prefilled from the task title, editable), body editor reusing the compose editor and signature, note "This email and every reply in its thread will be linked to the task."; pop-out into the normal compose window | todo |
| Options row: "After sending, move task to" (Waiting for / Keep in Next actions / Done) and "If there's no reply, add a follow-up in" (4 days / 2 days / 1 week / Never) with the computed date shown | todo |
| Actions: "Send and link", schedule send (reuse the existing send-later), attach file, discard draft, "Draft saved locally" indicator (draft stored in Rustmail's database like other drafts) | todo |
| Send path: send through the existing compose pipeline (`ActionKind::Send`), then link the sent message (thread id + Message-ID) to the task, move the task, create the follow-up calendar event if requested, and write activity entries | todo |
| Result card "Sent and linked" listing what happened (email, task moved, follow-up event), "Back to board", "Undo send" (only while the send is still in the undo window; afterwards it is hidden) | todo |
| Follow-up behaviour: when the follow-up date passes with no reply, the board card shows "No reply · N days" (warn) and the task is listed under Today; optional desktop notification | todo |
| Reply handling: a reply to the linked sent thread links the received mail, shows "Reply received" and moves the task as configured (shares the Phase 8 reply tracker) | todo |
| Checklist items can be ticked automatically for "Send to ..." steps only if the user links them; no text parsing | todo |
| Tests: send/link/move round trip with the local SMTP stub, follow-up date maths, undo window, text_fit | todo |

### Phase 10: timeline (Gantt) view — todo
Design: `Tasks · Timeline (Gantt)-html/Timeline.dc.html`.
| Item | Status |
|---|---|
| Timeline view in the Tasks module (the toolbar switch), same sidebar and filters as the board; toolbar with title (month and year), project + task count, ISO week badge, previous / next period, zoom Day / Week / Month, Today, toggles "Calendar events" and "Dependencies" | todo |
| Left grid: columns TASK, START, DUE, LINKS (link icons per mail / sent / event); hidden meta columns at phone width; group rows (project sections or task groups) with summary bars, indented child rows, status icon and strikethrough for done; row selection syncs with the chart | todo |
| Chart: scrollable day columns with weekend shading, week header (W36 ...), day letters and numbers (today circled), "Today" line with label, vertical grid lines per week | todo |
| Bars: coloured per group, progress fill (from the checklist or manual %), percentage or status tag at the bar end ("No reply · 2 days" warn), hatched section for the waiting-on-reply span, milestones (diamond) for zero-length tasks, link pins (mail / sent / event) at the day they happened | todo |
| Dependencies: finish-to-start arrows between bars (blocks / depends on links from Phase 6); cycle check on creation; arrows drawn with Slint `Path`/`Rectangle` segments | todo |
| Calendar events lane: events of the chosen calendars shown above the tasks (from the calendar module, read only) | todo |
| Interaction: drag a bar to reschedule, drag an edge to change start or due, drop updates the task and writes activity; click a bar opens the summary popover (status pill, date range and day count, links, "Open on board", "Email from task"); keyboard nudge with arrow keys | todo |
| Legend row: done portion, waiting on a reply, milestone, link pin kinds, depends on, hint "Drag a bar to reschedule · drag its edge to change the due date" | todo |
| Performance: virtualise rows and day columns; cap at a visible window; test with 1000 tasks | todo |
| Tests: date/pixel mapping, zoom levels, dependency routing, drag rescheduling, text_fit | todo |

### Phase 11: polish and optional extras — todo
- [ ] Task List view (flat table with sort and grouping), reached from the same view switch
- [ ] Settings page "Tasks": default project, default columns and WIP limits, default context, default calendar for blocked time, follow-up delay, create-task shortcut, auto-archive-on-create default
- [ ] Notifications: due-today and follow-up reminders (reuse the notification outbox)
- [ ] Tray / title badge with the number of tasks due today
- [ ] Import / export tasks (JSON; optional iCalendar VTODO file)
- [ ] Optional task sync (`optional`): first backend CalDAV VTODO (reuse `caldav/`), draining the Phase 6 `task_changes` log; deadlines, status and checklist mapping, links via `X-` properties; a Settings switch per account; needs the conflict policy from Phase 6
- [ ] Optional: attach a task from the calendar event dialog ("Create task from event")
- [ ] Data controller: count task data in storage usage and in account removal tests (removing a mail account must not delete tasks; links to its mail become "mail not found")
- [ ] Accessibility pass: keyboard navigation of the board, screen-reader labels for cards and bars
- [ ] Update `README`/docs and the `add-backend`-style skill note for the tasks module; record limits in this file
- [ ] Try the whole flow in the real app with a throwaway profile (`run-app` skill) against a real IMAP account copy; only unit tests until then

## Other pending items
- [ ] Setting "Reverse scroll direction" (global, window-level wheel hook) for natural-scrolling devices such as the Apple Magic Mouse
- [ ] IMAP CRAM-MD5/DIGEST-MD5, proxy login, proxying of non-mail traffic
- [x] Filters: arbitrary-header/body/age/flag conditions, copy/forward/run-command actions, more than 3 conditions/2 actions in the editor; actions on several messages (see the Phase 3 table for limits)
- [ ] Run the signature command off the UI thread
- [ ] Re-check the Claws Mail option list against the current manual
- [ ] Translate the new strings in the six non-English catalogs
- [ ] Mail list feedback: after start or a manual refresh the list can stay empty for a while with no sign that mail is being fetched (reported while trying the refresh button repeatedly; mail then arrived all at once). Show a visible "fetching mail" indicator (per account or in the list header) while a sync or refresh is running, disable or debounce repeated refresh clicks, and show when the last sync finished or failed

Limits of mbox write-back: every change rewrites the whole file (a temp file renamed over it, so the file's inode changes and hard links or other programs holding it open keep the old copy) and each queued action rewrites it again, so bulk changes in a very large mailbox are slow; the file is guarded by a `<file>.lock` dot-lock (a lock older than 5 minutes is taken over) and the write fails and is retried if the file changed meanwhile, but a program that ignores the dot-lock can still lose a message appended during the rewrite; `Status` is written as `R` (plus any existing `O`) and `X-Status` as `A F D T`, `Content-Length` is not updated or used; a move appends first and cuts second, so a crash or failure in between leaves a duplicate, not a loss; a single-file mbox account has no Trash folder, so Trash and delete do nothing there (delete only works on mail in Trash, as for IMAP); other mbox accounts get empty Trash, Sent and Drafts files created in the folder tree; the mbox write-back is tested with generated files only, not against mutt, Thunderbird or procmail.

Limits of the local backend: drafts stay in Rustmail's database (as for IMAP accounts) rather than in the Maildir; labels, snoozes and keywords are not written to the files; every imported message is also cached in Rustmail's data folder (a second copy of the mail); a message moved by another program shows up as a new message, and its Rustmail-only labels and snooze are restored from a copy kept by Message-ID for 30 days (messages without a Message-ID header, or a snooze of a thread that still has other messages, are not handled specially; a snooze is kept per thread); Maildir, MH and mbox folders can be created (also as sub-folders), renamed and deleted from Rustmail (deleting is permanent and removes the folder, its sub-folders and every message in them; for MH it removes the whole directory, including any non-mail files in it; INBOX and role folders are protected; a rename keeps the cached messages; new mbox folders are empty files, sub-folders go in a `<name>.sbd` directory, and an account that is a single mbox file has no folder tree to change; mbox contents are rewritten as described above); new-mail notifications (desktop notification outbox and chime) fire only for unread mail that appears in an INBOX folder after that folder's first scan and whose file is under 24 hours old, so the initial import and mail moved in from elsewhere in the first scan stay silent, and the check runs at the poll interval, not instantly; contact learning follows the IMAP rules (Sent recipients; incoming senders only when enabled, never automated, spam or trash mail, and only mail dated after the account was added); tested with generated fixtures only, not against Dovecot, mutt or other clients.

