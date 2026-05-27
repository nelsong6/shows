// Package store wraps the Postgres queries the API needs. The schema lives
// in internal/store/migrations and is applied by Migrate() on startup.
package store

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

type Store struct {
	pool *pgxpool.Pool
}

func New(ctx context.Context, dsn string) (*Store, error) {
	cfg, err := pgxpool.ParseConfig(dsn)
	if err != nil {
		return nil, fmt.Errorf("store: parse dsn: %w", err)
	}
	pool, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, fmt.Errorf("store: connect: %w", err)
	}
	if err := pool.Ping(ctx); err != nil {
		pool.Close()
		return nil, fmt.Errorf("store: ping: %w", err)
	}
	return &Store{pool: pool}, nil
}

func (s *Store) Pool() *pgxpool.Pool { return s.pool }
func (s *Store) Close()              { s.pool.Close() }

// ─── types ─────────────────────────────────────────────────────────

type Playlist struct {
	ID        int64     `json:"id"`
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
}

type Show struct {
	ID         int64      `json:"id"`
	PlaylistID int64      `json:"playlist_id"`
	Name       string     `json:"name"`
	RootPath   string     `json:"root_path"`
	DateAdded  time.Time  `json:"date_added"`
	RemovedAt  *time.Time `json:"removed_at,omitempty"`
}

// NextEpisode is one row of the round-selection query: a show plus its
// first unwatched episode. Used by the round-ordering logic on the API
// side; the same struct is JSON-serialized back to the client.
type NextEpisode struct {
	ShowID       int64  `json:"show_id"`
	ShowName     string `json:"show_name"`
	EpisodeID    int64  `json:"episode_id"`
	RootPath     string `json:"root_path"`
	RelativePath string `json:"relative_path"`
}

type HistoryEvent struct {
	EpisodeID    int64     `json:"episode_id"`
	RelativePath string    `json:"relative_path"`
	PlayedAt     time.Time `json:"played_at"`
}

// AdvanceResult summarizes a /advance call: how many episodes were marked
// watched, and which shows hit their last episode and were closed.
type AdvanceResult struct {
	AdvancedCount   int     `json:"advanced_count"`
	RemovedShowIDs  []int64 `json:"removed_show_ids"`
}

// ─── playlists ─────────────────────────────────────────────────────

func (s *Store) ListPlaylists(ctx context.Context) ([]Playlist, error) {
	rows, err := s.pool.Query(ctx, `SELECT id, name, created_at FROM playlists ORDER BY id`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Playlist
	for rows.Next() {
		var p Playlist
		if err := rows.Scan(&p.ID, &p.Name, &p.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, p)
	}
	return out, rows.Err()
}

var ErrPlaylistNotFound = errors.New("playlist not found")

func (s *Store) GetPlaylistByName(ctx context.Context, name string) (*Playlist, error) {
	var p Playlist
	err := s.pool.QueryRow(ctx,
		`SELECT id, name, created_at FROM playlists WHERE name = $1`, name,
	).Scan(&p.ID, &p.Name, &p.CreatedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, ErrPlaylistNotFound
	}
	if err != nil {
		return nil, err
	}
	return &p, nil
}

// ─── shows ─────────────────────────────────────────────────────────

func (s *Store) ListActiveShows(ctx context.Context, playlistID int64) ([]Show, error) {
	rows, err := s.pool.Query(ctx, `
		SELECT id, playlist_id, name, root_path, date_added, removed_at
		FROM shows
		WHERE playlist_id = $1 AND removed_at IS NULL
		ORDER BY id`, playlistID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Show
	for rows.Next() {
		var sh Show
		if err := rows.Scan(&sh.ID, &sh.PlaylistID, &sh.Name, &sh.RootPath, &sh.DateAdded, &sh.RemovedAt); err != nil {
			return nil, err
		}
		out = append(out, sh)
	}
	return out, rows.Err()
}

var ErrShowNotFound = errors.New("show not found")

func (s *Store) GetShow(ctx context.Context, id int64) (*Show, error) {
	var sh Show
	err := s.pool.QueryRow(ctx, `
		SELECT id, playlist_id, name, root_path, date_added, removed_at
		FROM shows WHERE id = $1`, id,
	).Scan(&sh.ID, &sh.PlaylistID, &sh.Name, &sh.RootPath, &sh.DateAdded, &sh.RemovedAt)
	if errors.Is(err, pgx.ErrNoRows) {
		return nil, ErrShowNotFound
	}
	if err != nil {
		return nil, err
	}
	return &sh, nil
}

// CreateShow inserts a new show plus its initial episode queue in a single
// transaction. `episodes` is the ordered list of relative paths; positions
// start at 0 and increment in input order.
func (s *Store) CreateShow(
	ctx context.Context,
	playlistID int64,
	name, rootPath string,
	dateAdded time.Time,
	episodes []string,
) (*Show, error) {
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Rollback(ctx) // safe to call after Commit, becomes no-op

	var sh Show
	err = tx.QueryRow(ctx, `
		INSERT INTO shows (playlist_id, name, root_path, date_added)
		VALUES ($1, $2, $3, $4)
		RETURNING id, playlist_id, name, root_path, date_added, removed_at`,
		playlistID, name, rootPath, dateAdded,
	).Scan(&sh.ID, &sh.PlaylistID, &sh.Name, &sh.RootPath, &sh.DateAdded, &sh.RemovedAt)
	if err != nil {
		return nil, fmt.Errorf("insert show: %w", err)
	}

	if err := insertEpisodes(ctx, tx, sh.ID, 0, episodes); err != nil {
		return nil, err
	}

	if err := tx.Commit(ctx); err != nil {
		return nil, err
	}
	return &sh, nil
}

// AppendEpisodes adds episodes to an existing show's queue, continuing the
// position sequence after the show's current MAX(position). Duplicate
// relative_paths are silently skipped (UNIQUE constraint isn't on
// relative_path because the legacy data has none, but in practice the
// client filters dupes before calling — see cmd/shows-client/add).
func (s *Store) AppendEpisodes(ctx context.Context, showID int64, episodes []string) (int, error) {
	if len(episodes) == 0 {
		return 0, nil
	}
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return 0, err
	}
	defer tx.Rollback(ctx)

	var maxPos *int
	err = tx.QueryRow(ctx,
		`SELECT MAX(position) FROM episodes WHERE show_id = $1`, showID,
	).Scan(&maxPos)
	if err != nil {
		return 0, err
	}
	start := 0
	if maxPos != nil {
		start = *maxPos + 1
	}

	if err := insertEpisodes(ctx, tx, showID, start, episodes); err != nil {
		return 0, err
	}
	if err := tx.Commit(ctx); err != nil {
		return 0, err
	}
	return len(episodes), nil
}

func insertEpisodes(ctx context.Context, tx pgx.Tx, showID int64, startPos int, episodes []string) error {
	if len(episodes) == 0 {
		return nil
	}
	// Multi-row INSERT — one round trip for the whole queue.
	var b strings.Builder
	b.WriteString(`INSERT INTO episodes (show_id, relative_path, position) VALUES `)
	args := make([]any, 0, len(episodes)*3)
	for i, ep := range episodes {
		if i > 0 {
			b.WriteString(", ")
		}
		off := i * 3
		fmt.Fprintf(&b, "($%d, $%d, $%d)", off+1, off+2, off+3)
		args = append(args, showID, ep, startPos+i)
	}
	if _, err := tx.Exec(ctx, b.String(), args...); err != nil {
		return fmt.Errorf("insert episodes: %w", err)
	}
	return nil
}

// ─── rounds ────────────────────────────────────────────────────────

// NextEpisodes returns one row per active show in the playlist, each
// pointing at the show's first unwatched episode. Returns an empty slice
// if no shows are active (e.g., all queues drained).
func (s *Store) NextEpisodes(ctx context.Context, playlistID int64) ([]NextEpisode, error) {
	// DISTINCT ON gives us one row per show — the unwatched episode with
	// the lowest position. ORDER BY show, position lets DISTINCT ON pick
	// the right episode deterministically.
	rows, err := s.pool.Query(ctx, `
		SELECT DISTINCT ON (s.id)
			s.id,
			s.name,
			e.id,
			s.root_path,
			e.relative_path
		FROM shows s
		JOIN episodes e ON e.show_id = s.id
		WHERE s.playlist_id = $1
		  AND s.removed_at IS NULL
		  AND e.watched_at IS NULL
		ORDER BY s.id, e.position`, playlistID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []NextEpisode
	for rows.Next() {
		var n NextEpisode
		if err := rows.Scan(&n.ShowID, &n.ShowName, &n.EpisodeID, &n.RootPath, &n.RelativePath); err != nil {
			return nil, err
		}
		out = append(out, n)
	}
	return out, rows.Err()
}

// Advance marks the given episode IDs as watched, appends to watch_history,
// and tombstones any show whose queue is now empty.
//
// The episode IDs must all be valid "first unwatched" episodes — the API
// computes them from NextEpisodes immediately before calling Advance. We
// don't re-validate here; the caller's freshly-computed list is the
// authority. Passing stale IDs would just no-op (the UPDATE finds nothing
// to update for already-watched rows).
func (s *Store) Advance(ctx context.Context, episodeIDs []int64) (*AdvanceResult, error) {
	if len(episodeIDs) == 0 {
		return &AdvanceResult{}, nil
	}
	tx, err := s.pool.Begin(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Rollback(ctx)

	now := time.Now().UTC()

	// Mark watched.
	tag, err := tx.Exec(ctx,
		`UPDATE episodes SET watched_at = $1 WHERE id = ANY($2::bigint[]) AND watched_at IS NULL`,
		now, episodeIDs,
	)
	if err != nil {
		return nil, fmt.Errorf("update episodes: %w", err)
	}
	advanced := int(tag.RowsAffected())

	// Record history.
	if _, err := tx.Exec(ctx, `
		INSERT INTO watch_history (episode_id, played_at)
		SELECT e.id, $1 FROM episodes e WHERE e.id = ANY($2::bigint[])`,
		now, episodeIDs,
	); err != nil {
		return nil, fmt.Errorf("insert history: %w", err)
	}

	// Tombstone any show whose queue is now empty. The subquery finds
	// shows touched by this advance that no longer have any unwatched
	// episodes. Returning gives us the IDs to surface in the response.
	rows, err := tx.Query(ctx, `
		UPDATE shows
		SET removed_at = $1
		WHERE removed_at IS NULL
		  AND id IN (
		      SELECT DISTINCT show_id FROM episodes WHERE id = ANY($2::bigint[])
		  )
		  AND NOT EXISTS (
		      SELECT 1 FROM episodes
		      WHERE show_id = shows.id AND watched_at IS NULL
		  )
		RETURNING id`, now, episodeIDs)
	if err != nil {
		return nil, fmt.Errorf("close shows: %w", err)
	}
	var removed []int64
	for rows.Next() {
		var id int64
		if err := rows.Scan(&id); err != nil {
			rows.Close()
			return nil, err
		}
		removed = append(removed, id)
	}
	rows.Close()
	if err := rows.Err(); err != nil {
		return nil, err
	}

	if err := tx.Commit(ctx); err != nil {
		return nil, err
	}
	return &AdvanceResult{AdvancedCount: advanced, RemovedShowIDs: removed}, nil
}

func (s *Store) ShowHistory(ctx context.Context, showID int64) ([]HistoryEvent, error) {
	rows, err := s.pool.Query(ctx, `
		SELECT e.id, e.relative_path, h.played_at
		FROM watch_history h
		JOIN episodes e ON e.id = h.episode_id
		WHERE e.show_id = $1
		ORDER BY h.played_at`, showID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []HistoryEvent
	for rows.Next() {
		var ev HistoryEvent
		if err := rows.Scan(&ev.EpisodeID, &ev.RelativePath, &ev.PlayedAt); err != nil {
			return nil, err
		}
		out = append(out, ev)
	}
	return out, rows.Err()
}
