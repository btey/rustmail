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
- [ ] S/MIME sign, encrypt, verify, decrypt
- [x] PGP inline: per-account "Use inline PGP" switch; clearsign, encrypt, or sign+encrypt into the plain-text body (quoted-printable), and inline blocks are decrypted/verified in the OpenPGP reader. Limits: messages with attachments or HTML-only bodies are refused when inline is on; only the first armored block is read; no automatic decrypt on open (use "Decrypt / verify OpenPGP")
- [ ] Default privacy system per account; sign/encrypt on reply

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

## Other pending items
- [ ] Setting "Reverse scroll direction" (global, window-level wheel hook) for natural-scrolling devices such as the Apple Magic Mouse
- [ ] IMAP CRAM-MD5/DIGEST-MD5, proxy login, proxying of non-mail traffic
- [x] Filters: arbitrary-header/body/age/flag conditions, copy/forward/run-command actions, more than 3 conditions/2 actions in the editor; actions on several messages (see the Phase 3 table for limits)
- [ ] Run the signature command off the UI thread
- [ ] Re-check the Claws Mail option list against the current manual
- [ ] Translate the new strings in the six non-English catalogs

Limits of mbox write-back: every change rewrites the whole file (a temp file renamed over it, so the file's inode changes and hard links or other programs holding it open keep the old copy) and each queued action rewrites it again, so bulk changes in a very large mailbox are slow; the file is guarded by a `<file>.lock` dot-lock (a lock older than 5 minutes is taken over) and the write fails and is retried if the file changed meanwhile, but a program that ignores the dot-lock can still lose a message appended during the rewrite; `Status` is written as `R` (plus any existing `O`) and `X-Status` as `A F D T`, `Content-Length` is not updated or used; a move appends first and cuts second, so a crash or failure in between leaves a duplicate, not a loss; a single-file mbox account has no Trash folder, so Trash and delete do nothing there (delete only works on mail in Trash, as for IMAP); other mbox accounts get empty Trash, Sent and Drafts files created in the folder tree; the mbox write-back is tested with generated files only, not against mutt, Thunderbird or procmail.

Limits of the local backend: drafts stay in Rustmail's database (as for IMAP accounts) rather than in the Maildir; labels, snoozes and keywords are not written to the files; every imported message is also cached in Rustmail's data folder (a second copy of the mail); a message moved by another program shows up as a new message, and its Rustmail-only labels and snooze are restored from a copy kept by Message-ID for 30 days (messages without a Message-ID header, or a snooze of a thread that still has other messages, are not handled specially; a snooze is kept per thread); Maildir, MH and mbox folders can be created (also as sub-folders), renamed and deleted from Rustmail (deleting is permanent and removes the folder, its sub-folders and every message in them; for MH it removes the whole directory, including any non-mail files in it; INBOX and role folders are protected; a rename keeps the cached messages; new mbox folders are empty files, sub-folders go in a `<name>.sbd` directory, and an account that is a single mbox file has no folder tree to change; mbox contents are rewritten as described above); new-mail notifications (desktop notification outbox and chime) fire only for unread mail that appears in an INBOX folder after that folder's first scan and whose file is under 24 hours old, so the initial import and mail moved in from elsewhere in the first scan stay silent, and the check runs at the poll interval, not instantly; contact learning follows the IMAP rules (Sent recipients; incoming senders only when enabled, never automated, spam or trash mail, and only mail dated after the account was added); tested with generated fixtures only, not against Dovecot, mutt or other clients.

