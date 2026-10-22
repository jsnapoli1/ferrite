-- Ferrite schema. SQLite in WAL mode; ids are UUIDv7 text (time ordered).

CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name  TEXT NOT NULL,
    email         TEXT,
    password_hash TEXT NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    quota_bytes   INTEGER,                -- NULL = unlimited
    ai_consent    INTEGER NOT NULL DEFAULT 0,
    disabled      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);

CREATE TABLE groups (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
    created_at TEXT NOT NULL
);

CREATE TABLE group_members (
    group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX group_members_user ON group_members(user_id);

-- Browser sessions and API/app tokens. Only SHA-256 of the secret is stored.
CREATE TABLE tokens (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,           -- 'session' | 'app'
    name         TEXT NOT NULL,
    secret_hash  TEXT NOT NULL UNIQUE,
    scopes       TEXT NOT NULL DEFAULT '*',
    created_at   TEXT NOT NULL,
    last_used_at TEXT,
    expires_at   TEXT
);
CREATE INDEX tokens_user ON tokens(user_id);

-- Content-addressed blob store bookkeeping (BLAKE3, refcounted).
CREATE TABLE blobs (
    hash      TEXT PRIMARY KEY,
    size      INTEGER NOT NULL,
    refcount  INTEGER NOT NULL DEFAULT 0
);

-- The file tree. Every user has one root directory (parent_id NULL).
CREATE TABLE nodes (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    parent_id   TEXT REFERENCES nodes(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,            -- 'dir' | 'file'
    blob_hash   TEXT REFERENCES blobs(hash),
    size        INTEGER NOT NULL DEFAULT 0,
    mime        TEXT NOT NULL DEFAULT 'application/octet-stream',
    etag        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    modified_at TEXT NOT NULL,
    trashed_at  TEXT,
    trash_origin_parent TEXT,
    favorite    INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX nodes_unique_name ON nodes(parent_id, name) WHERE trashed_at IS NULL AND parent_id IS NOT NULL;
CREATE UNIQUE INDEX nodes_one_root ON nodes(owner_id) WHERE parent_id IS NULL;
CREATE INDEX nodes_parent ON nodes(parent_id);
CREATE INDEX nodes_trash ON nodes(owner_id, trashed_at);

CREATE TABLE versions (
    id         TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    blob_hash  TEXT NOT NULL REFERENCES blobs(hash),
    size       INTEGER NOT NULL,
    author_id  TEXT,
    label      TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX versions_node ON versions(node_id, created_at);

-- Exclusive edit locks (e.g. while a Craft desktop app has a file open).
CREATE TABLE locks (
    node_id    TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    owner_app  TEXT NOT NULL,
    token      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

-- Shares. permissions bitmask follows Nextcloud: 1 read, 2 update, 4 create, 8 delete, 16 share.
CREATE TABLE shares (
    id            TEXT PRIMARY KEY,
    node_id       TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    owner_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,          -- 'user' | 'group' | 'room' | 'link'
    target_user   TEXT REFERENCES users(id) ON DELETE CASCADE,
    target_group  TEXT REFERENCES groups(id) ON DELETE CASCADE,
    target_room   TEXT,                   -- room id (rooms table is defined below)
    token         TEXT UNIQUE,
    password_hash TEXT,
    permissions   INTEGER NOT NULL,
    label         TEXT,
    expires_at    TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX shares_node ON shares(node_id);
CREATE INDEX shares_target_user ON shares(target_user);
CREATE INDEX shares_target_group ON shares(target_group);
CREATE INDEX shares_target_room ON shares(target_room);

-- Talk -----------------------------------------------------------------------
CREATE TABLE rooms (
    id          TEXT PRIMARY KEY,
    token       TEXT NOT NULL UNIQUE,     -- short public identifier used in URLs
    kind        TEXT NOT NULL,            -- 'one_to_one' | 'group' | 'public' | 'note_to_self'
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at  TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    read_only   INTEGER NOT NULL DEFAULT 0,
    ai_enabled  INTEGER NOT NULL DEFAULT 1,
    pair_key    TEXT UNIQUE               -- one_to_one dedupe: sorted user ids
);

CREATE TABLE participants (
    room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role         TEXT NOT NULL DEFAULT 'member', -- 'owner' | 'moderator' | 'member'
    last_read_message_id INTEGER NOT NULL DEFAULT 0,
    notify       TEXT NOT NULL DEFAULT 'mentions',  -- 'all' | 'mentions' | 'none'
    favorite     INTEGER NOT NULL DEFAULT 0,
    joined_at    TEXT NOT NULL,
    PRIMARY KEY (room_id, user_id)
);
CREATE INDEX participants_user ON participants(user_id);

-- Integer ids keep pagination and read markers cheap.
CREATE TABLE messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    actor_kind   TEXT NOT NULL,           -- 'user' | 'guest' | 'bot' | 'system'
    actor_id     TEXT NOT NULL,
    actor_name   TEXT NOT NULL,
    kind         TEXT NOT NULL DEFAULT 'text', -- 'text' | 'system' | 'file' | 'poll'
    body         TEXT NOT NULL,
    reply_to     INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    meta         TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL,
    edited_at    TEXT,
    deleted      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX messages_room ON messages(room_id, id);

CREATE TABLE reactions (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    actor_id   TEXT NOT NULL,
    emoji      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (message_id, actor_id, emoji)
);

CREATE TABLE guests (
    id           TEXT PRIMARY KEY,
    room_id      TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    secret_hash  TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL
);

-- Boards (kanban) -----------------------------------------------------------
CREATE TABLE boards (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    color      TEXT NOT NULL DEFAULT '#3b82f6',
    archived   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE TABLE board_members (
    board_id TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    can_edit INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (board_id, user_id)
);
CREATE INDEX board_members_user ON board_members(user_id);

CREATE TABLE board_labels (
    id       TEXT PRIMARY KEY,
    board_id TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    title    TEXT NOT NULL,
    color    TEXT NOT NULL
);

CREATE TABLE stacks (
    id       TEXT PRIMARY KEY,
    board_id TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    title    TEXT NOT NULL,
    position REAL NOT NULL
);
CREATE INDEX stacks_board ON stacks(board_id, position);

CREATE TABLE cards (
    id          TEXT PRIMARY KEY,
    stack_id    TEXT NOT NULL REFERENCES stacks(id) ON DELETE CASCADE,
    board_id    TEXT NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    position    REAL NOT NULL,
    due_at      TEXT,
    done        INTEGER NOT NULL DEFAULT 0,
    archived    INTEGER NOT NULL DEFAULT 0,
    created_by  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX cards_stack ON cards(stack_id, position);
CREATE INDEX cards_board ON cards(board_id);

CREATE TABLE card_labels (
    card_id  TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES board_labels(id) ON DELETE CASCADE,
    PRIMARY KEY (card_id, label_id)
);

CREATE TABLE card_assignees (
    card_id TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (card_id, user_id)
);

-- Cards link to anything: files, mind maps, canvases, rooms, URLs.
CREATE TABLE card_links (
    id        TEXT PRIMARY KEY,
    card_id   TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL,             -- 'file' | 'room' | 'url'
    target    TEXT NOT NULL,
    title     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE card_comments (
    id         TEXT PRIMARY KEY,
    card_id    TEXT NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    author_id  TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Activity & notifications --------------------------------------------------
CREATE TABLE activity (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT NOT NULL,            -- whose stream
    actor_id   TEXT,
    verb       TEXT NOT NULL,
    object_kind TEXT NOT NULL,
    object_id  TEXT NOT NULL,
    summary    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX activity_user ON activity(user_id, id);

CREATE TABLE notifications (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    link       TEXT NOT NULL DEFAULT '',
    read       INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE INDEX notifications_user ON notifications(user_id, read, id);

-- AI & MCP ---------------------------------------------------------------
CREATE TABLE ai_providers (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,            -- 'openai' (any OpenAI-compatible) | 'anthropic'
    base_url   TEXT NOT NULL,
    api_key    TEXT,
    models     TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL
);

-- Which provider+model serves which role ('chat', 'summarize', 'embed', ...).
CREATE TABLE ai_roles (
    role        TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL REFERENCES ai_providers(id) ON DELETE CASCADE,
    model       TEXT NOT NULL
);

CREATE TABLE ai_jobs (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    status      TEXT NOT NULL,           -- 'queued' | 'running' | 'done' | 'failed'
    input       TEXT NOT NULL,
    output      TEXT,
    error       TEXT,
    created_at  TEXT NOT NULL,
    finished_at TEXT
);
CREATE INDEX ai_jobs_user ON ai_jobs(user_id, created_at);

-- External MCP servers the assistant may use (admin: global; user: personal).
CREATE TABLE mcp_servers (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT REFERENCES users(id) ON DELETE CASCADE, -- NULL = global
    name       TEXT NOT NULL,
    transport  TEXT NOT NULL,            -- 'http' | 'stdio'
    url        TEXT,
    command    TEXT,
    args       TEXT NOT NULL DEFAULT '[]',
    headers    TEXT NOT NULL DEFAULT '{}',
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);

-- Semantic search vectors (f32 little endian) for files/messages/cards.
CREATE TABLE embeddings (
    object_kind TEXT NOT NULL,
    object_id   TEXT NOT NULL,
    owner_id    TEXT NOT NULL,
    model       TEXT NOT NULL,
    vector      BLOB NOT NULL,
    content_hash TEXT NOT NULL,
    PRIMARY KEY (object_kind, object_id)
);

-- Full text search ---------------------------------------------------------
CREATE VIRTUAL TABLE fts USING fts5(
    object_kind UNINDEXED,
    object_id UNINDEXED,
    scope UNINDEXED,                     -- room id / board id / owner id used for ACL filtering
    title,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
