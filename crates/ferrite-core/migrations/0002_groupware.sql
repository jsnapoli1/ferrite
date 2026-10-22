-- Calendars (CalDAV) and address books (CardDAV). Objects keep their raw iCalendar/vCard
-- text, so clients round-trip every property they wrote; a few fields are extracted for
-- queries, the web UI and AI tools.

CREATE TABLE dav_collections (
    id          TEXT PRIMARY KEY,
    owner_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,            -- 'calendar' | 'addressbook'
    uri         TEXT NOT NULL,            -- URL segment, e.g. 'personal'
    name        TEXT NOT NULL,
    color       TEXT NOT NULL DEFAULT '#c2410c',
    components  TEXT NOT NULL DEFAULT 'VEVENT,VTODO',
    ctag        TEXT NOT NULL,            -- changes on every modification (also the sync token)
    created_at  TEXT NOT NULL,
    UNIQUE (owner_id, kind, uri)
);

CREATE TABLE dav_objects (
    id            TEXT PRIMARY KEY,
    collection_id TEXT NOT NULL REFERENCES dav_collections(id) ON DELETE CASCADE,
    uri           TEXT NOT NULL,          -- e.g. '<uid>.ics'
    uid           TEXT NOT NULL,
    etag          TEXT NOT NULL,
    component     TEXT NOT NULL,          -- VEVENT | VTODO | VJOURNAL | VCARD
    summary       TEXT NOT NULL DEFAULT '',
    starts_at     TEXT,                   -- UTC, for time-range queries
    ends_at       TEXT,
    all_day       INTEGER NOT NULL DEFAULT 0,
    recurring     INTEGER NOT NULL DEFAULT 0,
    data          TEXT NOT NULL,
    modified_at   TEXT NOT NULL,
    UNIQUE (collection_id, uri)
);
CREATE INDEX dav_objects_time ON dav_objects(collection_id, starts_at);
CREATE INDEX dav_objects_uid ON dav_objects(collection_id, uid);
