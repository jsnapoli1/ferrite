-- Talk moderation: lobby, room passwords, call permissions and breakout rooms.

-- Lobby: while on (and before `lobby_timer`, when set), only moderators can read, write and
-- call; everyone else waits.
ALTER TABLE rooms ADD COLUMN lobby INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rooms ADD COLUMN lobby_timer TEXT;
-- argon2 hash; guests and users joining a public room by link must know the password.
ALTER TABLE rooms ADD COLUMN password_hash TEXT;
-- Who may start a call: 'everyone' | 'moderators'.
ALTER TABLE rooms ADD COLUMN call_permission TEXT NOT NULL DEFAULT 'everyone';

-- Breakout rooms are rooms of kind 'breakout' whose parent is the main conversation, so chat,
-- files and calls work exactly as in any room.
ALTER TABLE rooms ADD COLUMN parent_id TEXT REFERENCES rooms(id) ON DELETE CASCADE;
-- On a parent: how people are assigned ('auto' | 'manual' | 'free'), NULL when there are no
-- breakout rooms.
ALTER TABLE rooms ADD COLUMN breakout_mode TEXT;
-- On a parent: 'stopped' | 'running'.
ALTER TABLE rooms ADD COLUMN breakout_status TEXT NOT NULL DEFAULT 'stopped';
-- On a parent: when running breakout rooms end by themselves.
ALTER TABLE rooms ADD COLUMN breakout_ends_at TEXT;
-- On a breakout room: when someone in it asked the moderators for help.
ALTER TABLE rooms ADD COLUMN help_requested_at TEXT;
CREATE INDEX rooms_parent ON rooms(parent_id);

-- Who goes to which breakout room. Users are also participants of their breakout room; guests
-- (who belong to the parent) reach it through this table.
CREATE TABLE breakout_assignments (
    parent_id  TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    actor_kind TEXT NOT NULL,                -- 'user' | 'guest'
    actor_id   TEXT NOT NULL,
    room_id    TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    PRIMARY KEY (parent_id, actor_kind, actor_id)
);
CREATE INDEX breakout_assignments_room ON breakout_assignments(room_id);
