-- Two-factor authentication (TOTP, RFC 6238) and OpenID Connect single sign-on.

-- One TOTP secret per user. The secret is encrypted with a key derived from the server
-- secret. `enabled = 0` is a pending enrollment, waiting for its first code.
CREATE TABLE user_totp (
    user_id    TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    secret_enc TEXT NOT NULL,
    enabled    INTEGER NOT NULL DEFAULT 0,
    last_step  INTEGER,                -- last accepted time step; older or equal steps are refused
    created_at TEXT NOT NULL,
    enabled_at TEXT
);

-- One-time recovery codes, stored as keyed hashes.
CREATE TABLE recovery_codes (
    user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    used_at   TEXT,
    PRIMARY KEY (user_id, code_hash)
);

-- Second step of a password sign-in: a short-lived, single-use challenge. `kind` is
-- 'totp' (enter a code) or 'enroll' (2FA is required: set it up first; the pending
-- secret travels with the challenge).
CREATE TABLE login_challenges (
    id_hash    TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,
    secret_enc TEXT,
    attempts   INTEGER NOT NULL DEFAULT 0,
    expires_at TEXT NOT NULL
);
CREATE INDEX login_challenges_user ON login_challenges(user_id);

-- External identities: (provider, subject) from an OpenID Connect provider → local user.
CREATE TABLE oidc_links (
    provider      TEXT NOT NULL,
    subject       TEXT NOT NULL,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email         TEXT,
    auto_created  INTEGER NOT NULL DEFAULT 0, -- the account was created by this sign-in
    created_at    TEXT NOT NULL,
    last_login_at TEXT,
    PRIMARY KEY (provider, subject)
);
CREATE INDEX oidc_links_user ON oidc_links(user_id);

-- Authorization requests in flight: state, nonce and PKCE verifier, bound to the browser
-- that started them through a cookie (stored hashed).
CREATE TABLE oidc_requests (
    state_hash   TEXT PRIMARY KEY,
    provider     TEXT NOT NULL,
    nonce        TEXT NOT NULL,
    verifier     TEXT NOT NULL,
    browser_hash TEXT NOT NULL,
    return_to    TEXT NOT NULL,
    expires_at   TEXT NOT NULL
);
