-- +goose Up

-- A playlist is a named ordering of shows. Today there's exactly one ("nelson")
-- mirroring the legacy nelson.json. The table exists so the API surface and
-- schema stay agnostic to that fact — adding a second playlist later is
-- additive, not a migration.
CREATE TABLE playlists (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- A show is a directory on disk plus a queue of episodes. `root_path` is the
-- Windows absolute path the legacy scripts used (e.g.
-- `D:\Downloads\Group-Nelson\Dr. Katz, Professional Therapist`). `removed_at`
-- tombstones a show when its queue is exhausted — we don't hard-delete so
-- `watch_history` (which has a per-show join through `episodes`) stays
-- queryable for the "this show took N days to watch" reveal.
CREATE TABLE shows (
    id           BIGSERIAL PRIMARY KEY,
    playlist_id  BIGINT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    root_path    TEXT NOT NULL,
    date_added   TIMESTAMPTZ NOT NULL,
    removed_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Partial index for the hot path "what shows are still active in this playlist?".
-- The round-selection query hits this on every /next-round call.
CREATE INDEX shows_playlist_active_idx
    ON shows(playlist_id)
    WHERE removed_at IS NULL;

-- One row per episode file. `relative_path` is the on-disk path relative to
-- the parent show's `root_path` (joined with backslash) and matches the
-- format the legacy per-show JSONs used. `position` is the FIFO order the
-- legacy `Episodes` array implied — the unwatched episode with the lowest
-- position is the show's "next episode".
CREATE TABLE episodes (
    id            BIGSERIAL PRIMARY KEY,
    show_id       BIGINT NOT NULL REFERENCES shows(id) ON DELETE CASCADE,
    relative_path TEXT NOT NULL,
    position      INTEGER NOT NULL,
    watched_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(show_id, position)
);

-- Partial index for the next-episode lookup. `LIMIT 1` per show ordered by
-- position uses this directly.
CREATE INDEX episodes_show_unwatched_idx
    ON episodes(show_id, position)
    WHERE watched_at IS NULL;

-- Append-only log of episode plays. Each /advance call appends one row per
-- episode in the round. Source of truth for any "how long did show X take?"
-- or "what did I watch last Tuesday?" queries.
CREATE TABLE watch_history (
    id          BIGSERIAL PRIMARY KEY,
    episode_id  BIGINT NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    played_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX watch_history_played_at_idx
    ON watch_history(played_at);

-- Seed the default playlist. Idempotent so re-running the migration on a
-- fresh DB doesn't fight ON CONFLICT.
INSERT INTO playlists (name) VALUES ('nelson')
    ON CONFLICT (name) DO NOTHING;

-- +goose Down

DROP TABLE IF EXISTS watch_history;
DROP TABLE IF EXISTS episodes;
DROP TABLE IF EXISTS shows;
DROP TABLE IF EXISTS playlists;
