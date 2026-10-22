# Ferrite

Ferrite is a self-hosted collaboration server written in Rust: file sync and sharing, Talk
(chat, video calls, guests), boards, live documents, mind maps, canvases, calendars and
contacts. It integrates the [ArtCraft Crafting Apps](#artcraft-crafting-apps), and it is
built around **MCP and AI**. It covers much of what Nextcloud does. It is designed so that a
small machine (a mini PC, a NAS, a Raspberry-class board) runs it comfortably, including calls.

It is one binary with one data directory. No PHP, web server, database server, Redis or
separate signaling stack is needed.

```
cargo build --release
./target/release/ferrite serve          # http://127.0.0.1:8080 — the first visit creates the admin
```

- [What's inside](#whats-inside)
- [Efficiency: measured](#efficiency-measured)
- [Talk and calls](#talk-and-calls)
- [ArtCraft Crafting Apps](#artcraft-crafting-apps)
- [MCP and AI](#mcp-and-ai)
- [Sync clients and interoperability](#sync-clients-and-interoperability)
- [Configuration](#configuration)
- [Security model](#security-model)
- [Nextcloud parity](#nextcloud-parity)
- [Development and tests](#development-and-tests)

## What's inside

| Area | What you get |
|---|---|
| **Files** | Upload/download (drag and drop, multi-file progress), folders, rename/move/copy, favorites, recent, quotas, **versions** (restore any), **trash** (30-day retention, restore), **locks**, content-addressed storage with **deduplication** (BLAKE3), full-text search inside text files, previews for images, PDF, text, audio and video. |
| **Sharing** | Share with users, groups or a Talk conversation (read / edit / create / delete / reshare bits). Public links can have a password, an expiry, and an "upload" mode (file drop). |
| **Talk** | Group, public, one-to-one and note-to-self conversations. Markdown, replies, reactions, edits, deletes, @mentions with unread badges, typing indicators, polls, file sharing (from Files or by upload, drop or paste). **Guests** join public conversations by link. **Video/audio calls** with screen sharing, raise hand, speaker detection, and mute/camera indicators; routed through the built-in **SFU**, with peer-to-peer as the fallback. |
| **Boards** | Kanban boards with columns, cards (drag or keyboard), labels, due dates, assignees, comments, links to files/folders/conversations/URLs, read-only members and archive. |
| **Live documents** | Real-time co-editing (CRDT, Yjs-compatible) of **Markdown/text**, **spreadsheets** (formulas: arithmetic, refs, SUM/AVERAGE/MIN/MAX/COUNT), **mind maps** (`.fmind`) and **canvases** (`.fcanvas`: notes, frames, files, embedded mind maps and boards, arrows). They show peers' presence and cursors, and save automatically, with versions. |
| **Calendar and contacts** | CalDAV and CardDAV at Nextcloud-compatible URLs. Month/week/list views. **Recurring events are expanded** (RRULE, EXDATE, RDATE, overridden instances), DST-correct with IANA time zones. Changes from any client or tab appear live. Contacts can be edited. |
| **Search** | One search across files, messages, cards, events and contacts. Full text always; **semantic** (by meaning) when an embedding model is configured. |
| **MCP** | Ferrite is an **MCP server**: 59 tools, resources and prompts over Streamable HTTP and stdio, for both protocol eras. It is also an **MCP client**: it uses the Crafting Apps' MCP servers and any MCP servers you add. |
| **AI** | Bring your own model: any **OpenAI-compatible** server (Ollama, llama.cpp, LM Studio, vLLM, …) or **Anthropic's Claude API**. The assistant uses the same MCP tools with the user's own permissions, and is **read-only unless the user allows changes**. Typing `@assistant` in any conversation brings in the Talk bot. Per-user consent. |
| **Admin** | Users, groups, quotas, disabling, password reset, app tokens with scopes, server stats (memory, dedup savings, SFU traffic). |

The web app is embedded in the binary: no build step, no CDN, light and dark themes, and
usable at phone width.

## Efficiency: measured

The numbers below come from `crates/ferrite-server/examples/bench.rs`, run against a release
build. The raw output is in [`docs/bench/`](docs/bench). The machine was 4 vCPU (Intel Xeon @
2.10 GHz), Linux 6.18, rustc 1.97.0. The load generator ran **on the same 4 cores** as the
server, so latencies at high connection counts include its own CPU contention.

| Measurement | Result |
|---|---|
| Release binary (web app, Yjs and the IANA time-zone database included) | 31 MB |
| Resident memory, idle after start | **19 MB** |
| Memory per live WebSocket (Talk/event connection) | **13.5 KiB** |
| 1,000 live sockets (50 users × 20): total RSS | 34 MB |
| 5,000 live sockets (100 users × 50): total RSS | 87 MB |
| Chat fan-out, 1,000 sockets, 200 messages at ~50/s: deliveries | 200,000 of 200,000 · ~29,600/s |
| Fan-out latency, post → every socket (1,000 sockets) | p50 10 ms · p95 16 ms · p99 20 ms |
| Fan-out latency (5,000 sockets, 500,000 deliveries) | p50 38 ms · p95 61 ms · p99 71 ms |
| Message history page (50 messages, auth + permission checks), 32 concurrent clients | ~1,150–1,190 requests/s |
| WebDAV, 64 MiB file | upload ~270 MiB/s · download ~500 MiB/s |
| 3-person video call through the SFU (headless Chromium, fake camera) | each participant decodes ~20 fps from both others over **one** connection |

Reproduce:

```
cargo build --release --bin ferrite --example bench
FERRITE_DATA_DIR=/tmp/fe-bench ./target/release/ferrite serve &
./target/release/examples/bench --url http://127.0.0.1:8080 --pid $! --users 50 --conns 20 --messages 200
```

**How it stays small:**
- A single async process (Tokio/axum) rather than a pool of PHP workers.
- SQLite in WAL mode with FTS5, so there's no database server.
- An in-process event hub, so there's no Redis or NATS, and an in-process SFU (str0m) on one UDP port, so there's no Janus.
- Content-addressed blobs that are stored once.
- 4 KiB WebSocket read buffers.
- Batched queries on hot paths.
- glibc tuning so that argon2's 19 MiB password-hashing buffers go back to the OS.

**For context (not a benchmark of Nextcloud):**
- Nextcloud's system requirements say: "Nextcloud needs a minimum of 128MB RAM per process, and we recommend a minimum of 512MB RAM per process", and they list SQLite as "only recommended for testing and minimal-instances" ([Nextcloud admin manual: system requirements](https://docs.nextcloud.com/server/latest/admin_manual/installation/system_requirements.html)).
- For Talk, Nextcloud's documentation says the integrated signaling "works just fine for small and medium installations" and that Talk uses "long polling ... for chat and signaling messages" ([Talk system requirements](https://nextcloud-talk.readthedocs.io/en/latest/system-requirements/)).
- Larger calls use the High-performance Backend, "composed by two elements: the signaling server and the WebRTC gateway" (Janus) ([Nextcloud portal](https://portal.nextcloud.com/article/Nextcloud-Talk/High-Performance-Backend/Installation-of-Nextcloud-Talk-High-Performance-Backend)), and that signaling server "connects to a NATS server" ([nextcloud-spreed-signaling](https://github.com/strukturag/nextcloud-spreed-signaling)).

Ferrite ships the equivalent of that whole stack (persistent sockets, signaling and an SFU) in
the same 19 MB process. I have not run Nextcloud on this machine, so this README makes no
side-by-side performance claims.

## Talk and calls

- **Chat** runs over one persistent WebSocket per browser tab. Messages, reactions, typing, read state, call membership and live-document notifications all share it.
- **Calls** use the built-in **SFU** by default:
  - Each participant sends one upstream and receives everyone else over a **single** peer connection.
  - Forwarding stays inside a call.
  - The first offer/answer goes over the WebSocket; later renegotiation runs over a WebRTC data channel.
  - Open **UDP 10000** (configurable) on the server.
  - When the SFU is disabled or its port is busy, calls fall back to a peer-to-peer mesh automatically. The server tells all participants which transport to use, so they always agree.
- Capture is capped at 640×360 / 24 fps, which keeps calls light on small machines and links.
- **TURN:** on a LAN nothing extra is needed. Across NATs, add a TURN server under `ice_servers` (see [Configuration](#configuration)).
- **Guests** open `https://your-server/join/<token>`, pick a name, and can chat, react, vote and join calls. They cannot share files or administer the room.
- **Assistant in Talk:** in rooms where it is enabled, mentioning `@assistant` makes the AI answer in the thread, using the asker's permissions.

## ArtCraft Crafting Apps

The **ArtCraft** suite ([storytold/artcraft](https://github.com/storytold/artcraft)) is a family
of open-source desktop "Crafting Apps". Ferrite knows all 13 by their file formats, opens their
files from Files ("Open with…"), and can drive them **headlessly through each app's own MCP
server**. So the assistant, or any MCP client connected to Ferrite, can work inside a Word,
Excel, PowerPoint, SVG, PSD, PDF, IDML, DXF, audio, Figma, video or RAW document stored in
Ferrite.

| App | Native format | Also opens (examples) | Source |
|---|---|---|---|
| WordCraft | `.docx` | doc, odt, rtf, md, tex, html | [storytold/wordcraft](https://github.com/storytold/wordcraft) |
| GridCraft | `.xlsx` | xlsm, xlsb, ods, csv, tsv | [storytold/gridcraft](https://github.com/storytold/gridcraft) |
| DeckCraft | `.deckcraft` | pptx, potx, ppsx | [storytold/deckcraft](https://github.com/storytold/deckcraft) |
| VectorCraft | `.vectorcraft` (also `.drawcraft`, `.vctemplate`) | svg, ai, eps, emf, afdesign, pdf | [storytold/vectorcraft](https://github.com/storytold/vectorcraft) |
| PhotoCraft | `.pcraft` | psd, png, jpg, tiff, webp, … | [storytold/photocraft](https://github.com/storytold/photocraft) |
| PdfCraft | `.pdf` | | [storytold/pdfcraft](https://github.com/storytold/pdfcraft) |
| DesignCraft | `.designcraft` | idml | [storytold/designcraft](https://github.com/storytold/designcraft) |
| EffectCraft | `.ecproj` | lottie | [storytold/effectcraft](https://github.com/storytold/effectcraft) |
| CADCraft | `.dxf`, `.dwg` | | [storytold/cadcraft](https://github.com/storytold/cadcraft) |
| SoundCraft | `.scraft` | wav, aiff, flac, mp3, midi | [storytold/soundcraft](https://github.com/storytold/soundcraft) |
| FigCraft | `.figcraft` | fig, svg, png, jpg | [storytold/figcraft](https://github.com/storytold/figcraft) |
| FilmCraft | `.fcproj` | common video formats | [storytold/filmcraft](https://github.com/storytold/filmcraft) |
| LightCraft | `.lcpreset` | dng, cr2/cr3, nef, arw, raf, heic, … | [storytold/lightcraft](https://github.com/storytold/lightcraft) |

The full extension lists are in `crates/ferrite-core/src/craft.rs`.

**Two ways to use them:**
1. **Desktop:** the apps open files from Ferrite through WebDAV (see [Sync clients](#sync-clients-and-interoperability)). Several people can work on the same files, and Ferrite keeps versions.
2. **On the server, through MCP:** put the apps' `<app>-cli` binaries on the server's `PATH` or in `craft.bin_dir`, then use the `craft_open`, `craft_call` and `craft_close` tools.
   - `craft_open` copies the document into a private work folder, **locks the original** so nobody overwrites it meanwhile, and starts `<app>-cli mcp` confined to that folder.
   - `craft_call` invokes the app's own MCP tools (the bridge speaks the apps' MCP protocol version).
   - `craft_close` writes every changed or new file back next to the original **as a new version** and releases the lock.
   - Launch arguments are handled per app (automation roots, `--in FILE`, or a directory), and can be overridden in `[craft.commands]`.

## MCP and AI

### Ferrite as an MCP server

- **Endpoint:** `POST https://your-server/mcp` (Streamable HTTP). Authenticate with an **app token** created in *Settings → Tokens & MCP*, or with `ferrite token <user>`.
- **Protocol:** it speaks the stateless **2026-07-28** revision and the legacy `initialize` handshake (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05), so older and newer clients both work. The 2026 revision brings `server/discover`, per-request `_meta`, the `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name` headers, and cache hints.
- **stdio:** `ferrite mcp --user alice` serves MCP on stdin/stdout for clients that launch a command.

HTTP client configuration:

```json
{ "mcpServers": { "ferrite": { "url": "https://your-server/mcp", "headers": { "Authorization": "Bearer <app token>" } } } }
```

**Tools (59)**, each annotated read-only or destructive, and each limited by the token's scopes:

| Group | Tools |
|---|---|
| Files | `files_list` `files_info` `files_read` `files_write` `files_mkdir` `files_move` `files_copy` `files_delete` `files_search` `files_recent` `files_versions` `files_restore_version` `files_share` `files_shares` `trash_list` `trash_restore` |
| Talk | `talk_rooms` `talk_read` `talk_post` `talk_create_room` `talk_direct` `talk_share_file` `talk_react` `talk_poll` `talk_search` |
| Boards | `boards_list` `boards_get` `boards_create` `boards_add_card` `boards_update_card` `boards_move_card` `boards_assign` `boards_comment` `boards_link` `boards_my_cards` |
| Live documents | `docs_create` `docs_read` `docs_append` `docs_replace` `sheet_set` `mindmap_add` `mindmap_update` `canvas_add` `canvas_connect` |
| Calendar and contacts | `calendar_events` `calendar_create_event` `calendar_update_event` `calendar_delete_event` `contacts_search` `contacts_create` `contacts_update` |
| Everything else | `search` (hybrid full-text + semantic) `notifications` `activity` `users_search` |
| Crafting Apps | `craft_apps` `craft_open` `craft_call` `craft_close` |

**Resources:**
- `ferrite://files/{+path}` (file contents)
- `ferrite://talk/{room}` (recent conversation)

**Prompts:**
- `summarize_conversation`
- `board_standup`
- `mindmap_from_document`

Edits made through live-document tools reach people editing the same document **in real time**.

**Token scopes:** `files`, `talk`, `boards`, `docs`, `groupware`, `craft`, `ai`, `mcp`, `admin` (or `*`).

### Ferrite as an MCP client

- Admins can register MCP servers for everyone (Streamable HTTP, or stdio commands on the server), and users can add their own HTTP servers.
- The assistant can use their tools alongside Ferrite's own, still read-only unless the user allows changes.
- *Settings → MCP servers* probes a server and lists its tools.

### AI providers

Under *Settings → AI*, an admin adds providers and picks which model serves each role:

| Role | Used for |
|---|---|
| `chat` | The assistant page and the Talk bot (tool use, multi-step) |
| `summarize` | Summaries and quick actions (falls back to `chat`) |
| `embed` | **Semantic search** (OpenAI-compatible embedding endpoints) |

- **Local models:** presets cover Ollama, llama.cpp, LM Studio and vLLM. Nothing leaves your network.
- **Claude:** the native Messages API, with `claude-opus-5-5` as the default model.
- **Consent:** each user turns the assistant on for themselves.
- **Agent loop:** at most 12 steps per request. Every tool call is shown live, and recorded in the user's job history.
- **Semantic search:** when an `embed` model is set, a background indexer embeds the search index (re-embedding only changed text). Search then fuses full-text and vector results, so *"unpaid bill"* finds a file that says *"outstanding invoice"*. Vector hits go through the same permission checks as everything else.

## Sync clients and interoperability

| Protocol | URL | Tested with |
|---|---|---|
| WebDAV (files) | `https://your-server/remote.php/dav/files/<user>/` (also `/remote.php/webdav/`) | **rclone 1.68.2** (`vendor=nextcloud`): sync, check, round-trip, move, purge and quota (`about`), with Unicode and space-containing names. Also the test suite. |
| CalDAV | `https://your-server/remote.php/dav/` (discovery via `/.well-known/caldav`) | **python-caldav 3.5.0**: principal discovery, MKCALENDAR, saving and searching events (including a TZID weekly series with EXDATE), editing, deleting. Also the test suite. |
| CardDAV | `https://your-server/remote.php/dav/addressbooks/users/<user>/contacts/` (discovery via `/.well-known/carddav`) | curl PUT/PROPFIND, and the test suite (sync-collection, multiget, addressbook-query) |

- Use your account password (Basic auth, limited to the `files` and `groupware` scopes), or better, an app token, as the password.
- The **Nextcloud desktop and mobile apps are not supported.** They rely on Nextcloud's OCS APIs and login flow, which Ferrite does not implement. Use any standard WebDAV/CalDAV/CardDAV client instead: rclone, Cyberduck, davfs2, your OS's file manager, DAVx⁵, Thunderbird, Apple Calendar/Contacts.

## Configuration

Settings come from `ferrite.toml` (or `--config`/`FERRITE_CONFIG`), and `FERRITE_*` environment variables override them.

```toml
listen = "0.0.0.0:8080"
data_dir = "/var/lib/ferrite"              # database, blobs, scratch space
public_url = "https://cloud.example.org"    # used in share links, guest links, MCP metadata
open_registration = false                   # true: anyone can sign up
default_quota = 10737418240                 # bytes for new users; omit for unlimited
max_upload = 17179869184
trash_retention_days = 30
max_versions = 50
secure_cookies = true                       # set when served over HTTPS (behind a TLS proxy)

[sfu]
enabled = true
udp_bind = "0.0.0.0:10000"                  # open this UDP port
advertise = ["203.0.113.7"]                 # public IP(s) browsers should reach; default: this machine's IP

[[ice_servers]]                             # only needed across NATs
urls = ["turn:turn.example.org:3478"]
username = "ferrite"
credential = "secret"

[craft]
bin_dir = "/opt/craft/bin"                  # where <app>-cli binaries live (PATH is also searched)
[craft.commands.gridcraft]                  # optional per-app override of the MCP launch command
command = "/opt/craft/bin/gridcraft-cli"
args = ["mcp"]
```

**Environment variables:**
- `FERRITE_LISTEN`, `FERRITE_DATA_DIR`, `FERRITE_PUBLIC_URL`
- `FERRITE_OPEN_REGISTRATION`
- `FERRITE_SFU` (`0`/`false`/`off` disables it), `FERRITE_SFU_UDP`
- `FERRITE_CRAFT_BIN_DIR`

**Command line:**

```
ferrite serve                                   # default
ferrite user-add bob --password … [--admin] [--display-name "Bob B"]
ferrite passwd bob --password …
ferrite token bob --name laptop --scopes files,groupware   # prints an app token
ferrite mcp --user bob                          # MCP over stdio
```

**Deployment:**
- Put Ferrite behind any TLS-terminating reverse proxy (Caddy, nginx, Traefik) with WebSocket upgrade enabled for `/api/ws` and `/api/collab/`.
- Set `public_url` and `secure_cookies`.
- Open UDP 10000 for calls.
- **Backup** is the `data_dir`: SQLite (back up with `sqlite3 ferrite.db ".backup …"` or while stopped) plus the `blobs/` folder.

## Security model

- **Passwords:** argon2id; login throttling (8 failures per 10 minutes per account, for web and Basic auth logins); constant-time handling of unknown users.
- **Sessions and tokens:**
  - Browser sessions use an HttpOnly, SameSite cookie, and every mutating request must carry a custom `X-Ferrite-Request` header (CSRF).
  - App tokens are stored hashed and carry scopes. Tokens can only be minted from an interactive session.
- **Sockets:** the WebSocket checks `Origin`. Call signaling only relays between sessions the server issued to that connection.
- **User files** are served with `Content-Security-Policy: sandbox`, and only safe types render inline. Everything else downloads.
- **AI:** the assistant and the Talk bot act through the same tools with the **requesting user's** permissions, and are read-only unless the user allows changes for that request. Stdio MCP servers (which run commands) can only be added by admins.
- **Crafting Apps** run confined to a per-session work folder, and the original file is locked while they work.

## Nextcloud parity

This is an honest map against Nextcloud's core feature set: ✅ done, 🟡 partial, ❌ not implemented.

| Feature | Status | Notes |
|---|---|---|
| Web file manager, upload, folders, move/copy/rename | ✅ | |
| Versions, trash, favorites, recent | ✅ | |
| Quotas | ✅ | |
| File locking | ✅ | WebDAV LOCK and API locks; Craft sessions lock automatically |
| Share with users and groups | ✅ | |
| Share with a Talk conversation | ✅ | |
| Public links: password, expiry, upload ("file drop") | ✅ | |
| Federated sharing between servers | ❌ | |
| WebDAV | ✅ | Tested with rclone |
| Nextcloud desktop and mobile clients | ❌ | OCS APIs and login flow not implemented; use standard clients |
| Full-text search | ✅ | Built in (no Elasticsearch) |
| Semantic search | ✅ | With an embedding model |
| Talk: chat, replies, reactions, edits, mentions, polls, file sharing | ✅ | |
| Talk: guests via link | ✅ | |
| Talk: audio/video calls, screen sharing | ✅ | Built-in SFU (no separate High-performance Backend) |
| Talk: recording, SIP dial-in, breakout rooms, lobby, federation, Matterbridge | ❌ | |
| Mobile push notifications | ❌ | In-app live notifications only |
| Deck (kanban boards) | ✅ | |
| Collaborative text | ✅ | Markdown |
| Collaborative office (Collabora/OnlyOffice) | 🟡 | Live spreadsheets and text built in. Word/Excel/PowerPoint and more through the Crafting Apps (desktop over WebDAV, or headless via MCP), but no in-browser rendering of .docx/.xlsx/.pptx |
| Mind maps and whiteboards | ✅ | Canvases with embedded files, mind maps and boards |
| Calendar (CalDAV) | ✅ | Recurrence expansion. VTODO tasks are read-only in the web UI (editable by CalDAV clients). Calendar sharing between users and invitations/scheduling (iTIP) not implemented |
| Contacts (CardDAV) | ✅ | |
| Mail | ❌ | |
| Activity stream, notifications | ✅ | |
| Users and groups admin | ✅ | |
| LDAP/AD, SAML/OIDC single sign-on | ❌ | |
| Two-factor authentication | ❌ | |
| Server-side encryption | ❌ | |
| End-to-end encryption | ❌ | |
| App store, third-party apps | ❌ | Extension happens through **MCP** instead: any MCP server's tools become available to the assistant |
| AI assistant | ✅ | MCP-native, local or Claude models, tool use with user permissions, Talk bot |
| External storage (S3, SMB, …) | ❌ | |
| Antivirus, workflow engine, retention policies | ❌ | Trash retention only |

## Development and tests

```
crates/
  ferrite-core     domain logic: auth, files, blobs, shares, Talk, boards, live docs (yrs),
                   groupware + recurrence, search, Craft registry, SQLite migrations
  ferrite-mcp      MCP protocol: server (both eras, HTTP + stdio) and client
  ferrite-ai       providers (OpenAI-compatible, Anthropic) and the tool-using agent loop
  ferrite-sfu      selective forwarding unit (str0m)
  ferrite-server   the `ferrite` binary: REST, WebSockets, WebDAV/CalDAV/CardDAV, MCP endpoint,
                   AI service, semantic indexer, Craft bridge, embedded web app (web/)
e2e/               Playwright browser tests
docs/bench/        benchmark results
```

**Rust tests:** `cargo test --workspace` runs 59 tests. They include end-to-end tests that
start real servers and cover:
- login/CSRF and tokens
- files, sharing, WebDAV, CalDAV/CardDAV
- Talk with call signaling
- live co-editing over WebSockets
- MCP in both protocol eras
- the assistant and Talk bot against a mock model
- semantic search against a mock embedder
- a Craft app driven over MCP

CI-style checks are `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`.

**Browser tests** use headless Chromium with fake camera and microphone. Start a server on a
fresh data dir, then run a test from `e2e/` (Node 22+; `npm install` there fetches
Playwright, and `npx playwright install chromium` fetches the browser):

```
FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8821 FERRITE_PUBLIC_URL=http://127.0.0.1:8821 ./target/debug/ferrite serve &
cd e2e && FERRITE_URL=http://127.0.0.1:8821 node files.mjs
```

| Test | Covers |
|---|---|
| `smoke.mjs` | App shell, navigation, no console errors |
| `files.mjs` | Files, sharing dialog, public links (password, upload), versions, trash, Settings (profile, AI, tokens/MCP, MCP servers, Craft, admin), the assistant against a mock model, mobile layouts |
| `talk.mjs` | Two-user chat (mentions, unread, replies, reactions, edit/delete, typing, polls, files), a video call (connect, mute/hand/camera/screen-share indicators, leave), guest join, deep links, narrow layouts |
| `sfu.mjs` | Three-person call through the SFU: one connection each, ~20 fps decoded from both others, server forwarding counters, clean leave |
| `boards.mjs` | Boards, card modal, drag and keyboard moves, read-only members, and the four live editors with two users co-editing |
| `calendar.mjs` | Month/week/list, create/edit/move events, recurring series across DST (Europe/Berlin), EXDATE, series edits, live refresh across tabs, contacts create/edit/search/delete, phone layouts |
| `search.mjs` | Search results for files, events and contacts open the right views |

## License

AGPL-3.0-or-later (the same license family as Nextcloud). The vendored Yjs bundle is MIT, see
`crates/ferrite-server/web/vendor/LICENSES.txt`.
