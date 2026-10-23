# Mail: Nextcloud Mail parity checklist

The target is the feature set of Nextcloud Mail as documented in the
[user manual](https://docs.nextcloud.com/server/latest/user_manual/en/groupware/mail.html) and
[admin manual](https://docs.nextcloud.com/server/latest/admin_manual/groupware/mail.html), and
as implemented in [nextcloud/mail](https://github.com/nextcloud/mail) (`appinfo/routes.php`,
`src/components`, `lib/Service`; main branch inspected 2026-10-23, 6.0.0-alpha.1).

Left out: the JMAP service (in 6.0-alpha only internally, with no routes or UI), and Nextcloud's
user-migration export (an occ tool of the Nextcloud ecosystem).

Status: ✅ in Ferrite · 🔧 being built (workstream in brackets).

## Layout, lists and preferences — [inbox]

| Feature | Status |
|---|---|
| Layouts: list, vertical split, horizontal split | ✅ |
| Compact mode (no avatars or previews, checkboxes always shown) | ✅ |
| Threaded or one-message-per-row display | ✅ |
| Sort newest or oldest first (all accounts) | ✅ |
| Favorites (starred) shown in their own section at the top; Starred view | ✅ |
| All inboxes (unified across accounts) | ✅ |
| Priority inbox: Important / Others sections, importance learned per account from the user's behaviour (read, replied, sent, marked important, subject), can be switched off; admin default | ✅ |
| Tags: IMAP keywords with the defaults `$label1`–`$label5` (Important, Work, Personal, To Do, Later; renamable), custom tags with colours, rename/recolour/delete | ✅ |
| Quick actions: user-defined one-click sequences (mark read/unread, favourite, important, spam, apply tag, move, delete), per account | ✅ |
| Snooze a message or thread until a chosen time (Snoozed folder, automatic return), unsnooze | ✅ |
| Single-click envelope actions, drag and drop to folders, move dialog | ✅ |
| Dashboard widgets: unread mail, important mail (Ferrite's Home) | ✅ |
| Deep links to a message; `mailto:` handler | ✅ |
| Preferences stored per user; admin UI defaults | ✅ |

## Folders (mailboxes) — [inbox]

| Feature | Status |
|---|---|
| Special-use detection (Inbox, Sent, Drafts, Trash, Archive, Junk) | ✅ |
| Create, rename, delete, move folders and subfolders; subscribe/unsubscribe | ✅ |
| Mark all as read, empty (clear) a folder, folder statistics, repair/clear cache, manual refresh | ✅ |
| Shared folders (IMAP namespaces/ACL) shown with a shared marker | ✅ |
| Per-account default folders (Drafts, Sent, Trash, Archive, Junk, Snoozed) | ✅ |
| Trash retention: delete messages in Trash after N days | ✅ |
| Move junk-marked messages to the Junk folder automatically | ✅ |
| Unread counts, live new mail (IDLE) | ✅ |

## Search — [inbox]

| Feature | Status |
|---|---|
| Global (unified) search including mail | ✅ |
| Quick search in a folder (subject, sender, recipient) | ✅ |
| Advanced search: from, to, cc, subject, body, date range, flags, tags, attachments | ✅ |
| Body search opt-in, also for unified folders | ✅ |

## Reading — [security] and [integrations]

| Feature | Status |
|---|---|
| Plain text and sanitized HTML, sandboxed, remote content blocked | ✅ |
| Blocked-content warning; trusted senders (address or domain) list and management | ✅ |
| Remote images through a server-side proxy (sender never sees the reader's IP) | ✅ |
| Sender avatars: contacts' photos, then site favicon (privacy-preserving, cached) | ✅ |
| Phishing warnings: sender name/address mismatch with contacts, custom-address mismatch, future date, misleading links, Reply-To mismatch, server phishing flag | ✅ |
| Internal addresses: warn about senders/recipients outside an allowed list of addresses or domains | ✅ |
| DKIM verification result | ✅ |
| View message source; export as .eml; download all attachments as .zip | ✅ |
| Attachments: download, save to Files | ✅ |
| Encrypted mail: S/MIME (verify signatures, decrypt) | ✅ |
| Encrypted mail: PGP through the Mailvelope browser extension | ✅ |
| Meeting invitations (iMIP): show the event, accept / tentative / decline, import .ics; replies from external guests update Ferrite events; optional automatic tentative entries and updates | ✅ (REQUEST, CANCEL, REPLY, COUNTER; times in the viewer's zone, conflicts; replies sent from the user's account; propose a new time) |
| Travel itineraries (schema.org reservations in mail) shown as cards, add to calendar | ✅ (JSON-LD and microdata: flight, lodging, train, bus, event, rental car, restaurant) |
| Create a calendar event or a task from a message (AI-suggested when AI is configured) | ✅ (tasks are VTODO) |
| Add sender to contacts / new contact; recipient info pane | ✅ (contact match in the reader, `GET /api/mail/contact-info`, a Ferrite user's absence) |
| One-click unsubscribe (List-Unsubscribe, RFC 8058) | ✅ (HTTPS POST that never reaches private addresses, or mailto sent from the account) |
| Read receipts (MDN): ask the user to send one when a sender requests it | 🔧 [compose] |
| Print a message | 🔧 [compose] |
| AI: thread summary (3+ messages), smart replies, translation, follow-up reminders for unanswered sent mail | ✅ (needs the admin's AI processing switch and the user's consent; cached, never blocks reading) |

## Composing and sending — [compose]

| Feature | Status |
|---|---|
| Plain text compose; reply, reply-all, forward with quoting and threading; drafts on the server | ✅ |
| Rich text (HTML) editor with formatting, links, lists, quotes, inline images; plain-text mode; default editor mode per account | 🔧 |
| Aliases (identities) per account; send as a server alias | 🔧 |
| Signatures per account and per alias (HTML), above or below the quote | 🔧 |
| Text blocks: reusable snippets inserted with `!` or from the menu, shareable with users and groups | 🔧 |
| `@`-mention a contact to add them as a recipient | 🔧 |
| Recipient info side pane | 🔧 |
| Minimizable composer with a "resume" indicator; drafts saved automatically | ✅ autosave · 🔧 minimize |
| Outbox: queued and scheduled messages, retries, reopen before sending | 🔧 |
| Send later (scheduled send); undo send | 🔧 |
| Request a read receipt | 🔧 |
| Attachments from upload or Files | ✅ |
| Large attachments as Files share links (admin attachment size limit) | 🔧 |
| Forward as attachment | 🔧 |
| Sending to a Ferrite group address expands to its members | 🔧 |
| Internal-address warnings when sending | ✅ |
| S/MIME: certificate management (PKCS#12 / PEM import), sign and/or encrypt | ✅ |
| PGP via Mailvelope (compose) | ✅ |

## Filters and autoresponder — [integrations]

| Feature | Status |
|---|---|
| Sieve: enable per account (ManageSieve host, port, credentials) | ✅ (STARTTLS or TLS, SASL PLAIN or XOAUTH2; defaults to the IMAP server and login, or the provisioning template's server) |
| Sieve script editor | ✅ (the server's CHECKSCRIPT errors shown) |
| Visual filters: conditions on subject, sender, recipient (is, contains, matches); actions file into folder, add flag/tag, stop; create a filter from a message | ✅ (Nextcloud's script format, so hand-written parts and Nextcloud's own filters survive) |
| Autoresponder (out of office) through Sieve vacation; manual or following a user-level absence setting | ✅ (absence in Settings → Profile, shown to other users) |

## Accounts, delegation and admin — [accounts]

| Feature | Status |
|---|---|
| IMAP/SMTP accounts, encrypted passwords, connection test, provider database autoconfig | ✅ |
| Autoconfig from MX records | ✅ (also the provider's autoconfig/autodiscover URLs; connection test) |
| Google and Microsoft accounts with OAuth 2.0 (XOAUTH2), admin-configured client | ✅ |
| Account order; quota display | ✅ |
| Account delegation: share an account with other users (read, send, delete; not remove or re-authenticate), revocable | ✅ |
| Provisioning: admin-defined account templates created for users automatically | ✅ |
| Admin settings: allow users to add accounts, anti-spam report addresses (report spam/not spam), attachment size limit, anti-abuse alerts (recipient count, message rate), background sync interval, AI processing on/off, importance classification default, UI defaults | ✅ (settings and spam actions; the other features read them through `mail/settings.rs`) |
| Background sync of all accounts (not only while online) | ✅ |
| Assistant (MCP) tools for the new features | ✅ accounts (delegation, report spam, provisioning) · ✅ integrations (filters, out of office, invitations, thread summary, unsubscribe) · 🔧 (each) |
