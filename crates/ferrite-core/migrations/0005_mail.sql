-- Webmail: IMAP/SMTP accounts that users connect, with a local cache of folders, envelopes
-- and size-capped bodies. The IMAP server stays the source of truth: everything below except
-- the account settings can be rebuilt by syncing again.

CREATE TABLE mail_accounts (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,              -- sender name
    email         TEXT NOT NULL,
    imap_host     TEXT NOT NULL,
    imap_port     INTEGER NOT NULL,
    imap_security TEXT NOT NULL,              -- tls | starttls | none
    imap_username TEXT NOT NULL,
    smtp_host     TEXT NOT NULL,
    smtp_port     INTEGER NOT NULL,
    smtp_security TEXT NOT NULL,
    smtp_username TEXT NOT NULL,
    password      TEXT NOT NULL,              -- sealed with a key derived from the server secret
    smtp_password TEXT,                       -- sealed; NULL means the IMAP password
    save_sent     INTEGER NOT NULL DEFAULT 1, -- APPEND sent mail to Sent (off where the provider does it)
    signature     TEXT NOT NULL DEFAULT '',
    last_error    TEXT,
    synced_at     TEXT,
    created_at    TEXT NOT NULL
);
CREATE INDEX mail_accounts_user ON mail_accounts(user_id);

CREATE TABLE mail_folders (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,               -- IMAP name (modified UTF-7)
    display_name TEXT NOT NULL,               -- decoded last path segment
    delimiter    TEXT,
    role         TEXT,                        -- inbox | sent | drafts | trash | junk | archive
    selectable   INTEGER NOT NULL DEFAULT 1,
    uidvalidity  INTEGER,
    uidnext      INTEGER,
    modseq       INTEGER,                     -- HIGHESTMODSEQ (CONDSTORE servers)
    total        INTEGER NOT NULL DEFAULT 0,
    unread       INTEGER NOT NULL DEFAULT 0,
    window_uid   INTEGER,                     -- every message from this UID up is cached
    complete     INTEGER NOT NULL DEFAULT 0,  -- ... and there is nothing older on the server
    synced_at    TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE mail_messages (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    folder_id       TEXT NOT NULL REFERENCES mail_folders(id) ON DELETE CASCADE,
    uid             INTEGER NOT NULL,
    message_id      TEXT,                     -- Message-ID without angle brackets
    in_reply_to     TEXT,
    refs            TEXT NOT NULL DEFAULT '', -- References, space separated
    thread_id       TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '',
    from_name       TEXT NOT NULL DEFAULT '',
    from_addr       TEXT NOT NULL DEFAULT '',
    to_addrs        TEXT NOT NULL DEFAULT '[]', -- JSON [{name, address}]
    cc_addrs        TEXT NOT NULL DEFAULT '[]',
    date            TEXT NOT NULL,
    size            INTEGER NOT NULL DEFAULT 0,
    seen            INTEGER NOT NULL DEFAULT 0,
    flagged         INTEGER NOT NULL DEFAULT 0,
    answered        INTEGER NOT NULL DEFAULT 0,
    draft           INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    preview         TEXT NOT NULL DEFAULT '',
    fts_rowid       INTEGER,                  -- this message's row in the search index
    UNIQUE (folder_id, uid)
);
CREATE INDEX mail_messages_list ON mail_messages(folder_id, date DESC);
CREATE INDEX mail_messages_thread ON mail_messages(account_id, thread_id);
CREATE INDEX mail_messages_msgid ON mail_messages(account_id, message_id);

-- Every way a cached message disappears (expunge, UIDVALIDITY reset, folder or account
-- removal, user deletion) also drops it from the search index.
CREATE TRIGGER mail_messages_unindex AFTER DELETE ON mail_messages WHEN old.fts_rowid IS NOT NULL
BEGIN
    DELETE FROM fts WHERE rowid = old.fts_rowid;
END;

-- Bodies fetched on demand. Each account keeps a bounded cache (least recently read first out).
CREATE TABLE mail_bodies (
    message_id     TEXT PRIMARY KEY REFERENCES mail_messages(id) ON DELETE CASCADE,
    account_id     TEXT NOT NULL,
    text           TEXT NOT NULL,
    html           TEXT,                      -- original HTML; sanitized whenever it is served
    attachments    TEXT NOT NULL DEFAULT '[]',
    remote_content INTEGER NOT NULL DEFAULT 0,
    raw            BLOB,                      -- the whole message when small enough
    bytes          INTEGER NOT NULL,
    accessed_at    TEXT NOT NULL
);
CREATE INDEX mail_bodies_lru ON mail_bodies(account_id, accessed_at);

-- Files uploaded in the composer until the message is sent or the draft saved.
CREATE TABLE mail_uploads (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    mime       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    data       BLOB NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX mail_uploads_user ON mail_uploads(user_id, created_at);
