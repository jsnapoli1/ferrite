-- Mail integrations: Sieve (filters, autoresponder), the user's absence, meeting
-- invitations (iMIP) received by mail, and AI results for mail.

-- Per-account settings of the integrations. A missing row means the defaults.
CREATE TABLE mail_account_integrations (
    account_id          TEXT PRIMARY KEY REFERENCES mail_accounts(id) ON DELETE CASCADE,
    sieve_enabled       INTEGER NOT NULL DEFAULT 0,
    sieve_host          TEXT NOT NULL DEFAULT '',  -- '' = the IMAP host
    sieve_port          INTEGER NOT NULL DEFAULT 4190,
    sieve_security      TEXT NOT NULL DEFAULT 'starttls', -- starttls | tls | none
    sieve_username      TEXT,                      -- NULL = the IMAP username
    sieve_password      TEXT,                      -- sealed; NULL = the IMAP password
    ooo_follows_absence INTEGER NOT NULL DEFAULT 0, -- autoresponder follows the user's absence
    imip_auto_update    INTEGER NOT NULL DEFAULT 1, -- apply updates/cancellations to events already in the calendar
    imip_auto_create    INTEGER NOT NULL DEFAULT 0, -- put new invitations in the calendar as tentative
    followup_enabled    INTEGER NOT NULL DEFAULT 1, -- AI follow-up reminders for sent mail
    updated_at          TEXT
);

-- The user's absence (Settings → Profile): shown to others and followed by autoresponders.
CREATE TABLE user_absences (
    user_id     TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    first_day   TEXT NOT NULL,                   -- YYYY-MM-DD, inclusive
    last_day    TEXT NOT NULL,                   -- YYYY-MM-DD, inclusive
    timezone    TEXT NOT NULL DEFAULT 'UTC',     -- IANA zone the days are in
    status      TEXT NOT NULL DEFAULT '',        -- short status, e.g. "On vacation"
    message     TEXT NOT NULL DEFAULT '',        -- the out-of-office message
    replacement TEXT NOT NULL DEFAULT '',        -- who to contact instead (free text)
    updated_at  TEXT NOT NULL
);

-- iMIP messages handled (automatically or by the user), by the message's Message-ID, so a
-- message is processed once even when it is moved or synced again.
CREATE TABLE mail_imip_log (
    account_id  TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    message_id  TEXT NOT NULL,                   -- Message-ID header without brackets
    method      TEXT NOT NULL,
    uid         TEXT NOT NULL,
    status      TEXT NOT NULL,                   -- applied | ignored | outdated | rejected | answered | error
    detail      TEXT NOT NULL DEFAULT '',
    processed_at TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);

-- The newest REPLY applied per attendee and event (stale replies arriving late are ignored).
CREATE TABLE mail_imip_replies (
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    uid       TEXT NOT NULL,
    attendee  TEXT NOT NULL,
    dtstamp   TEXT NOT NULL,                     -- the REPLY's DTSTAMP (UTC, iCalendar form)
    PRIMARY KEY (user_id, uid, attendee)
);

-- AI results for mail, cached by a hash of what they were computed from.
CREATE TABLE mail_ai_cache (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,                    -- summary | replies | translate:<lang> | event | followup
    key        TEXT NOT NULL,                    -- content hash
    value      TEXT NOT NULL,                    -- JSON
    created_at TEXT NOT NULL,
    PRIMARY KEY (user_id, kind, key)
);
CREATE INDEX mail_ai_cache_age ON mail_ai_cache(created_at);

-- Sent messages checked for an expected reply.
CREATE TABLE mail_followups (
    account_id  TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    message_id  TEXT NOT NULL,                   -- Message-ID of the sent message
    subject     TEXT NOT NULL DEFAULT '',
    recipients  TEXT NOT NULL DEFAULT '[]',      -- JSON [{name, address}]
    sent_at     TEXT NOT NULL,
    status      TEXT NOT NULL,                   -- waiting | not_needed | replied | disabled | error
    checked_at  TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
CREATE INDEX mail_followups_status ON mail_followups(account_id, status);
