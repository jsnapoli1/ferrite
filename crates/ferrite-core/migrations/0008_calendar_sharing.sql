-- Calendar sharing (with users and groups, read-only or read-write), public read-only
-- subscription links, and who created or last changed each calendar object (sharees can
-- write to calendars they don't own).

CREATE TABLE calendar_shares (
    id            TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL REFERENCES dav_collections(id) ON DELETE CASCADE,
    target_kind   TEXT NOT NULL,          -- 'user' | 'group'
    target_id     TEXT NOT NULL,          -- users.id or groups.id
    writable      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    UNIQUE (collection_id, target_kind, target_id)
);
CREATE INDEX calendar_shares_target ON calendar_shares(target_kind, target_id);

-- A sharee's own name and colour for a calendar shared with them; `removed` hides a calendar
-- that reached them through a group share after they removed it from their list.
CREATE TABLE calendar_share_prefs (
    collection_id TEXT NOT NULL REFERENCES dav_collections(id) ON DELETE CASCADE,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name          TEXT,
    color         TEXT,
    removed       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (collection_id, user_id)
);

-- Public read-only ICS feeds: one unguessable token per published calendar.
CREATE TABLE calendar_publications (
    collection_id TEXT PRIMARY KEY REFERENCES dav_collections(id) ON DELETE CASCADE,
    token         TEXT NOT NULL UNIQUE,
    created_at    TEXT NOT NULL
);

CREATE TABLE calendar_object_authors (
    object_id   TEXT PRIMARY KEY REFERENCES dav_objects(id) ON DELETE CASCADE,
    created_by  TEXT REFERENCES users(id) ON DELETE SET NULL,
    modified_by TEXT REFERENCES users(id) ON DELETE SET NULL
);
