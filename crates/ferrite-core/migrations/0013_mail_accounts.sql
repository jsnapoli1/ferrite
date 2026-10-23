-- Mail accounts beyond passwords: OAuth 2.0 (XOAUTH2) sign-in, per-user account order, quota,
-- delegation to other users, admin provisioning templates, captured login passwords for
-- provisioning, OAuth authorization requests, and the anti-abuse send log.

-- Admin-defined templates that create a mail account for every matching user.
CREATE TABLE mail_provisionings (
    id                     TEXT PRIMARY KEY,
    name                   TEXT NOT NULL,
    match_kind             TEXT NOT NULL,              -- domain | group | all
    match_value            TEXT NOT NULL DEFAULT '',   -- an email domain ('*' = any) or a group id
    email_pattern          TEXT NOT NULL,              -- e.g. %USERID%@example.org or %EMAIL%
    name_pattern           TEXT NOT NULL DEFAULT '',   -- sender name; empty = the user's display name
    imap_host              TEXT NOT NULL,
    imap_port              INTEGER NOT NULL,
    imap_security          TEXT NOT NULL,
    imap_username_pattern  TEXT NOT NULL,
    smtp_host              TEXT NOT NULL,
    smtp_port              INTEGER NOT NULL,
    smtp_security          TEXT NOT NULL,
    smtp_username_pattern  TEXT NOT NULL,              -- empty: the SMTP server takes mail without a login
    sieve_enabled          INTEGER NOT NULL DEFAULT 0,
    sieve_host             TEXT NOT NULL DEFAULT '',
    sieve_port             INTEGER NOT NULL DEFAULT 4190,
    sieve_security         TEXT NOT NULL DEFAULT 'starttls',
    password_mode          TEXT NOT NULL,              -- master | login
    master_password        TEXT,                       -- sealed; master mode only
    enabled                INTEGER NOT NULL DEFAULT 1,
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL
);

ALTER TABLE mail_accounts ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'password'; -- password | xoauth2
ALTER TABLE mail_accounts ADD COLUMN oauth_provider TEXT;                          -- google | microsoft
ALTER TABLE mail_accounts ADD COLUMN oauth_refresh TEXT;                           -- sealed refresh token
ALTER TABLE mail_accounts ADD COLUMN oauth_expires_at TEXT;                        -- expiry of the access token (sealed in `password`)
ALTER TABLE mail_accounts ADD COLUMN provisioning_id TEXT REFERENCES mail_provisionings(id) ON DELETE SET NULL;
ALTER TABLE mail_accounts ADD COLUMN quota_used INTEGER;                           -- bytes, IMAP QUOTA (RFC 9208) STORAGE
ALTER TABLE mail_accounts ADD COLUMN quota_limit INTEGER;
CREATE INDEX mail_accounts_provisioning ON mail_accounts(provisioning_id);

-- Each user's order of the accounts they see (own and delegated). Missing rows sort last.
CREATE TABLE mail_account_order (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    PRIMARY KEY (user_id, account_id)
);

-- An account shared with another user: they read, send, move and delete its mail, but cannot
-- remove it, change how it signs in, or share it further.
CREATE TABLE mail_delegations (
    id         TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (account_id, user_id)
);
CREATE INDEX mail_delegations_user ON mail_delegations(user_id);

-- Sign-in passwords kept (sealed) for provisioning templates in "login password" mode. Only
-- users matched by such a template have a row; it goes when no template needs it any more.
CREATE TABLE mail_login_secrets (
    user_id    TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    password   TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- OAuth authorization requests in flight (Authorization Code + PKCE), bound to the browser
-- session that started them; each state works once.
CREATE TABLE mail_oauth_requests (
    state_hash TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    provider   TEXT NOT NULL,
    verifier   TEXT NOT NULL,
    email      TEXT NOT NULL,
    name       TEXT NOT NULL,
    account_id TEXT,                                   -- reconnecting an existing account
    expires_at TEXT NOT NULL
);

-- Sent messages per user (one day kept), for the anti-abuse alerts.
CREATE TABLE mail_send_log (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sent_at    TEXT NOT NULL,
    recipients INTEGER NOT NULL
);
CREATE INDEX mail_send_log_user ON mail_send_log(user_id, sent_at);

CREATE TABLE mail_abuse_alerts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    rule       TEXT NOT NULL,                          -- recipients | 15m | 1h | 1d
    observed   INTEGER NOT NULL,
    threshold  INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX mail_abuse_alerts_user ON mail_abuse_alerts(user_id, rule, created_at);

-- Servers on local addresses are refused by default from now on; installations that already
-- have mail accounts keep accepting them (they may well use a mail server on the LAN) until an
-- administrator turns it off.
INSERT INTO kv (key, value)
SELECT 'mail_settings', '{"allow_local_servers":true}'
WHERE EXISTS (SELECT 1 FROM mail_accounts) AND NOT EXISTS (SELECT 1 FROM kv WHERE key = 'mail_settings');
