-- Composing and sending mail: per-account composer settings, aliases (identities) with their
-- own signatures, text blocks shared with users and groups, per-user sending preferences,
-- and the outbox (messages queued, scheduled or failed, sent by a background sender).

-- Composer settings of an account. The text signature stays in mail_accounts.signature;
-- this keeps its rich (HTML) version and where it goes.
CREATE TABLE mail_compose_prefs (
    account_id            TEXT PRIMARY KEY REFERENCES mail_accounts(id) ON DELETE CASCADE,
    editor_mode           TEXT NOT NULL DEFAULT 'plaintext', -- plaintext | richtext
    signature_html        TEXT,                              -- sanitized; NULL: the text signature
    signature_above_quote INTEGER NOT NULL DEFAULT 1
);

-- Other addresses an account sends as (aliases the mail server allows).
CREATE TABLE mail_aliases (
    id             TEXT PRIMARY KEY,
    account_id     TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    email          TEXT NOT NULL,
    signature      TEXT,                                     -- text; NULL: the account's signature
    signature_html TEXT,
    created_at     TEXT NOT NULL,
    UNIQUE (account_id, email)
);

-- Reusable snippets, inserted with "!" or from the menu.
CREATE TABLE mail_text_blocks (
    id         TEXT PRIMARY KEY,
    owner_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    html       TEXT NOT NULL,                                -- sanitized
    text       TEXT NOT NULL,                                -- plain rendering
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX mail_text_blocks_owner ON mail_text_blocks(owner_id);

-- Text blocks shared (read-only) with a user or a group.
CREATE TABLE mail_text_block_shares (
    block_id  TEXT NOT NULL REFERENCES mail_text_blocks(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL,                                 -- user | group
    target_id TEXT NOT NULL,
    PRIMARY KEY (block_id, kind, target_id)
);
CREATE INDEX mail_text_block_shares_target ON mail_text_block_shares(kind, target_id);

-- Sending preferences of a user.
CREATE TABLE mail_send_prefs (
    user_id           TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    undo_send_seconds INTEGER NOT NULL DEFAULT 10
);

-- Messages waiting to be sent. `compose` is the composer's message (JSON); its attachments
-- are copied into mail_outbox_files when queued, so they outlive composer uploads.
CREATE TABLE mail_outbox (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_id      TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    compose         TEXT NOT NULL,
    subject         TEXT NOT NULL DEFAULT '',
    recipients      TEXT NOT NULL DEFAULT '[]',             -- JSON, for the list
    send_at         TEXT,                                    -- NULL: held (not scheduled)
    status          TEXT NOT NULL DEFAULT 'queued',          -- queued | sending | failed
    attempts        INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at TEXT,                                    -- retry time after a failure
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX mail_outbox_user ON mail_outbox(user_id, created_at);
CREATE INDEX mail_outbox_due ON mail_outbox(status, send_at);

CREATE TABLE mail_outbox_files (
    id        TEXT PRIMARY KEY,
    outbox_id TEXT NOT NULL REFERENCES mail_outbox(id) ON DELETE CASCADE,
    name      TEXT NOT NULL,
    mime      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    data      BLOB NOT NULL
);
CREATE INDEX mail_outbox_files_outbox ON mail_outbox_files(outbox_id);
