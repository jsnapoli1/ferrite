-- Mail security and privacy: trusted senders, internal addresses, per-user settings, DKIM
-- results, avatar cache, S/MIME certificates and the local index of decrypted mail.

-- Senders (an address or a whole domain) whose remote images load without asking.
CREATE TABLE mail_trusted_senders (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,                -- address | domain
    value      TEXT NOT NULL,                -- lower case
    created_at TEXT NOT NULL,
    UNIQUE (user_id, kind, value)
);

-- Addresses and domains the user considers internal (no "external" warnings for them).
CREATE TABLE mail_internal_addresses (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,                -- address | domain
    value      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (user_id, kind, value)
);

CREATE TABLE mail_security_settings (
    user_id              TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    internal_enabled     INTEGER NOT NULL DEFAULT 0, -- warn about senders/recipients outside the list
    index_decrypted      INTEGER NOT NULL DEFAULT 0, -- decrypted S/MIME text goes into Mail search
    assistant_decrypted  INTEGER NOT NULL DEFAULT 0, -- the assistant (MCP) may read decrypted S/MIME text
    updated_at           TEXT NOT NULL
);

-- DKIM verification of a cached message (recomputed after temporary DNS failures).
CREATE TABLE mail_dkim_results (
    message_id TEXT PRIMARY KEY REFERENCES mail_messages(id) ON DELETE CASCADE,
    status     TEXT NOT NULL,                -- pass | fail | none | temperror | permerror | neutral
    domain     TEXT,
    selector   TEXT,
    detail     TEXT,
    checked_at TEXT NOT NULL
);

-- Sender pictures fetched from the network (favicons, opt-in Gravatar), with misses cached
-- too (data NULL).
CREATE TABLE mail_avatar_cache (
    key        TEXT PRIMARY KEY,             -- favicon:<domain> | gravatar:<sha256 of the address>
    mime       TEXT,
    data       BLOB,
    fetched_at TEXT NOT NULL
);

-- S/MIME certificates: the user's own (with the private key sealed with a key derived from
-- the server secret), correspondents' (collected from signed mail or imported) and trusted
-- issuers the user added.
CREATE TABLE mail_smime_certs (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,               -- own | contact | authority
    source      TEXT NOT NULL,               -- pkcs12 | pem | collected | imported
    emails      TEXT NOT NULL DEFAULT '[]',  -- JSON, lower case
    subject     TEXT NOT NULL,
    issuer      TEXT NOT NULL,
    serial      TEXT NOT NULL,               -- hex
    not_before  TEXT NOT NULL,
    not_after   TEXT NOT NULL,
    fingerprint TEXT NOT NULL,               -- SHA-256 of the DER, hex
    cert_der    BLOB NOT NULL,
    chain_der   BLOB,                        -- concatenated DER of intermediates (own certificates)
    key_sealed  TEXT,                        -- sealed PKCS#8 private key (own certificates)
    created_at  TEXT NOT NULL,
    UNIQUE (user_id, fingerprint)
);
CREATE INDEX mail_smime_certs_user ON mail_smime_certs(user_id, kind);

-- Which own certificate an account (or one of its sending addresses) signs and decrypts with.
CREATE TABLE mail_smime_identities (
    account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    email      TEXT NOT NULL,                -- the account address or an alias, lower case
    cert_id    TEXT NOT NULL REFERENCES mail_smime_certs(id) ON DELETE CASCADE,
    sign_default    INTEGER NOT NULL DEFAULT 0,
    encrypt_default INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, email)
);

-- Cached bodies whose message was S/MIME encrypted (decrypted text is never put into the
-- shared search index, previews or the semantic index).
ALTER TABLE mail_bodies ADD COLUMN encrypted INTEGER NOT NULL DEFAULT 0;

-- Decrypted text of encrypted mail, searchable from Mail when the user opted in. Separate
-- from `fts` so that global search, MCP search and embeddings never see it.
CREATE VIRTUAL TABLE mail_decrypted_fts USING fts5(
    message_id UNINDEXED,
    user_id UNINDEXED,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TABLE mail_decrypted_indexed (
    message_id TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL
);
CREATE INDEX mail_decrypted_indexed_user ON mail_decrypted_indexed(user_id);

CREATE TRIGGER mail_messages_drop_decrypted AFTER DELETE ON mail_messages
WHEN old.id IN (SELECT message_id FROM mail_decrypted_indexed)
BEGIN
    DELETE FROM mail_decrypted_fts WHERE message_id = old.id;
    DELETE FROM mail_decrypted_indexed WHERE message_id = old.id;
END;
