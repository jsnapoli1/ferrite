-- Web Push: one row per browser/device subscription (RFC 8030 push resource plus the RFC 8291
-- keys), and per-user preferences. An endpoint belongs to whoever subscribed it last, so a
-- shared browser only receives the signed-in user's notifications.
CREATE TABLE push_subscriptions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint     TEXT NOT NULL UNIQUE,
    p256dh       TEXT NOT NULL,           -- base64url, uncompressed P-256 point (65 bytes)
    auth         TEXT NOT NULL,           -- base64url, 16-byte authentication secret
    label        TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    last_used_at TEXT
);
CREATE INDEX push_subscriptions_user ON push_subscriptions(user_id);

-- Missing row = everything on.
CREATE TABLE push_settings (
    user_id  TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    enabled  INTEGER NOT NULL DEFAULT 1,
    chat     INTEGER NOT NULL DEFAULT 1,
    calls    INTEGER NOT NULL DEFAULT 1,
    calendar INTEGER NOT NULL DEFAULT 1,
    files    INTEGER NOT NULL DEFAULT 1,
    boards   INTEGER NOT NULL DEFAULT 1
);
