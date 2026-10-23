-- Inbox management for webmail: tags (IMAP keywords), preferences, per-account inbox
-- settings, the importance classifier, snoozing, quick actions, subscriptions and shared
-- folders, trash retention.

-- IMAP keywords of a cached message, lowercased, sorted and space-padded (" $label1 work "),
-- so one keyword matches with LIKE '% kw %'.
ALTER TABLE mail_messages ADD COLUMN keywords TEXT NOT NULL DEFAULT '';
-- In the subscription list (LSUB). Unsubscribed folders are hidden unless asked for.
ALTER TABLE mail_folders ADD COLUMN subscribed INTEGER NOT NULL DEFAULT 1;
-- In another user's or a shared namespace (RFC 2342).
ALTER TABLE mail_folders ADD COLUMN shared INTEGER NOT NULL DEFAULT 0;

-- Mail preferences per user (layout, sorting, …) as JSON; missing keys take the admin
-- defaults (kv 'mail.defaults'), then the built-in ones.
CREATE TABLE mail_prefs (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    prefs   TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE mail_account_prefs (
    account_id           TEXT PRIMARY KEY REFERENCES mail_accounts(id) ON DELETE CASCADE,
    classify             INTEGER,                    -- importance classification; NULL: admin default
    move_junk            INTEGER NOT NULL DEFAULT 1, -- marking as junk moves to the Junk folder
    trash_retention_days INTEGER NOT NULL DEFAULT 0, -- 0: keep
    roles                TEXT NOT NULL DEFAULT '{}', -- role -> IMAP folder name chosen by the user
    namespaces           TEXT                        -- NAMESPACE response as JSON; NULL until asked
);

-- Tags are per user (shared by their accounts) and stored on messages as IMAP keywords.
CREATE TABLE mail_tags (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    keyword    TEXT NOT NULL,                         -- lowercased IMAP keyword
    name       TEXT NOT NULL,
    color      TEXT NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0,            -- $label1..$label5: renamable, not deletable
    created_at TEXT NOT NULL,
    UNIQUE (user_id, keyword)
);

-- One-click sequences of actions, per account.
CREATE TABLE mail_quick_actions (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    steps      TEXT NOT NULL,                         -- JSON [{type, keyword?, folder?}]
    position   INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
CREATE INDEX mail_quick_actions_account ON mail_quick_actions(account_id, position);

-- Snoozed messages wait in the account's Snoozed folder and go back to `src_folder` at
-- `until`. They are found again by Message-ID (their UID changes with every move).
CREATE TABLE mail_snoozed (
    id         TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    subject    TEXT NOT NULL DEFAULT '',
    src_folder TEXT NOT NULL,                         -- IMAP name
    until      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (account_id, message_id)
);
CREATE INDEX mail_snoozed_until ON mail_snoozed(until);

-- The importance model of an account, retrained in the background.
CREATE TABLE mail_classifiers (
    account_id TEXT PRIMARY KEY REFERENCES mail_accounts(id) ON DELETE CASCADE,
    model      TEXT,                                  -- JSON; NULL: rules until enough examples
    samples    INTEGER NOT NULL DEFAULT 0,
    important  INTEGER NOT NULL DEFAULT 0,
    accuracy   REAL,
    trained_at TEXT,
    dirty      INTEGER NOT NULL DEFAULT 1             -- the user taught it something since
);

-- When a message was first seen in Trash (for trash retention).
CREATE TABLE mail_trash_seen (
    account_id  TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    uidvalidity INTEGER NOT NULL,
    uid         INTEGER NOT NULL,
    seen_at     TEXT NOT NULL,
    PRIMARY KEY (account_id, uidvalidity, uid)
);
