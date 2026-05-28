// Package store backs the shows API with Azure Cosmos DB.
//
// Two containers on the shared infra-cosmos-serverless account:
//
//	shows          one doc per show, episodes embedded as an array,
//	               partitioned by /playlist
//	watch_history  one doc per played episode, append-only,
//	               partitioned by /show_id
//
// Auth: workload identity. The pod's ServiceAccount is annotated with
// the shows-identity UAMI's client_id (provisioned in tofu/identity.tf),
// which DefaultAzureCredential picks up via the projected token.
// Cosmos data-plane role is narrowed to `dbs/shows` only.
package store

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"
	"github.com/Azure/azure-sdk-for-go/sdk/data/azcosmos"
	"github.com/google/uuid"
)

const (
	containerShows        = "shows"
	containerWatchHistory = "watch_history"
)

// Store is the data layer. Construct once with New(), share across the
// process. Methods are safe for concurrent use — the Cosmos SDK client
// is internally concurrency-safe.
type Store struct {
	client       *azcosmos.Client
	databaseName string
	shows        *azcosmos.ContainerClient
	history      *azcosmos.ContainerClient
}

// New constructs a Store. endpoint is the Cosmos account URL
// (https://<account>.documents.azure.com:443/) and databaseName is the
// SQL database within it ("shows" by default).
//
// Credentials are resolved via DefaultAzureCredential, which picks up
// the projected workload-identity token at /var/run/secrets/azure/tokens
// when running in-cluster.
func New(ctx context.Context, endpoint, databaseName string) (*Store, error) {
	if endpoint == "" {
		return nil, errors.New("store: empty Cosmos endpoint")
	}
	if databaseName == "" {
		return nil, errors.New("store: empty database name")
	}
	cred, err := azidentity.NewDefaultAzureCredential(nil)
	if err != nil {
		return nil, fmt.Errorf("store: credential: %w", err)
	}
	client, err := azcosmos.NewClient(endpoint, cred, nil)
	if err != nil {
		return nil, fmt.Errorf("store: cosmos client: %w", err)
	}
	shows, err := client.NewContainer(databaseName, containerShows)
	if err != nil {
		return nil, fmt.Errorf("store: shows container: %w", err)
	}
	history, err := client.NewContainer(databaseName, containerWatchHistory)
	if err != nil {
		return nil, fmt.Errorf("store: watch_history container: %w", err)
	}
	return &Store{
		client:       client,
		databaseName: databaseName,
		shows:        shows,
		history:      history,
	}, nil
}

// Ping does a lightweight check that the Cosmos endpoint is reachable
// and the configured database exists. Used by the /readyz probe.
func (s *Store) Ping(ctx context.Context) error {
	db, err := s.client.NewDatabase(s.databaseName)
	if err != nil {
		return err
	}
	_, err = db.Read(ctx, nil)
	return err
}

// ─── document shapes ───────────────────────────────────────────────

// showDoc is the on-disk representation; embedding Episodes as a
// nested array keeps every advance to a single point-write. JSON
// field names use snake_case to match the API and the legacy data.
type showDoc struct {
	ID         string       `json:"id"`
	Playlist   string       `json:"playlist"`
	Name       string       `json:"name"`
	RootPath   string       `json:"root_path"`
	DateAdded  time.Time    `json:"date_added"`
	RemovedAt  *time.Time   `json:"removed_at,omitempty"`
	CreatedAt  time.Time    `json:"created_at"`
	// UpdatedAt is the last-write-wins key for offline sync — the client owns
	// it (sends it on /sync, reads it on /library). Zero for pre-sync docs,
	// which correctly reads as "oldest".
	UpdatedAt  time.Time    `json:"updated_at"`
	Episodes   []episodeDoc `json:"episodes"`
}

type episodeDoc struct {
	ID           string     `json:"id"`
	RelativePath string     `json:"relative_path"`
	Position     int        `json:"position"`
	WatchedAt    *time.Time `json:"watched_at,omitempty"`
	ResumePos    *float64   `json:"resume_pos,omitempty"` // seconds into the file
	UpdatedAt    time.Time  `json:"updated_at"`           // LWW key (see showDoc)
}

type historyDoc struct {
	ID           string    `json:"id"`
	ShowID       string    `json:"show_id"`
	EpisodeID    string    `json:"episode_id"`
	RelativePath string    `json:"relative_path"`
	PlayedAt     time.Time `json:"played_at"`
}

// ─── public types ──────────────────────────────────────────────────

type Playlist struct {
	Name string `json:"name"`
}

type Show struct {
	ID         string     `json:"id"`
	Playlist   string     `json:"playlist"`
	Name       string     `json:"name"`
	RootPath   string     `json:"root_path"`
	DateAdded  time.Time  `json:"date_added"`
	RemovedAt  *time.Time `json:"removed_at,omitempty"`
}

type HistoryEvent struct {
	EpisodeID    string    `json:"episode_id"`
	RelativePath string    `json:"relative_path"`
	PlayedAt     time.Time `json:"played_at"`
}

// ─── offline sync wire types ───────────────────────────────────────
//
// The desktop is the engine; the server is a durable origin it syncs with
// git-style. GET /library returns LibraryShow (pull/seed); POST /sync takes a
// SyncRequest of locally-changed records (push). Both carry updated_at so
// reconciliation is last-write-wins.

type LibraryEpisode struct {
	ID           string     `json:"id"`
	RelativePath string     `json:"relative_path"`
	Position     int        `json:"position"`
	WatchedAt    *time.Time `json:"watched_at"`
	ResumePos    *float64   `json:"resume_pos"`
	UpdatedAt    time.Time  `json:"updated_at"`
}

type LibraryShow struct {
	ID        string           `json:"id"`
	Playlist  string           `json:"playlist"`
	Name      string           `json:"name"`
	RootPath  string           `json:"root_path"`
	DateAdded time.Time        `json:"date_added"`
	RemovedAt *time.Time       `json:"removed_at"`
	UpdatedAt time.Time        `json:"updated_at"`
	Episodes  []LibraryEpisode `json:"episodes"`
}

// SyncShow is a show-level change the client pushes. Episodes ride in
// SyncRequest.Episodes (keyed by show_id) so a row-level change doesn't
// re-send the whole show.
type SyncShow struct {
	ID        string     `json:"id"`
	Playlist  string     `json:"playlist"`
	Name      string     `json:"name"`
	RootPath  string     `json:"root_path"`
	DateAdded time.Time  `json:"date_added"`
	RemovedAt *time.Time `json:"removed_at"`
	UpdatedAt time.Time  `json:"updated_at"`
}

type SyncEpisode struct {
	ID           string     `json:"id"`
	ShowID       string     `json:"show_id"` // routes to the embedding show doc
	RelativePath string     `json:"relative_path"`
	Position     int        `json:"position"`
	WatchedAt    *time.Time `json:"watched_at"`
	ResumePos    *float64   `json:"resume_pos"`
	UpdatedAt    time.Time  `json:"updated_at"`
}

type SyncHistory struct {
	ID           string    `json:"id"`
	ShowID       string    `json:"show_id"`
	EpisodeID    string    `json:"episode_id"`
	RelativePath string    `json:"relative_path"`
	PlayedAt     time.Time `json:"played_at"`
}

type SyncRequest struct {
	Shows    []SyncShow    `json:"shows"`
	Episodes []SyncEpisode `json:"episodes"`
	History  []SyncHistory `json:"history"`
}

// ─── errors ────────────────────────────────────────────────────────

var (
	ErrPlaylistNotFound = errors.New("playlist not found")
	ErrShowNotFound     = errors.New("show not found")
	ErrEpisodeNotFound  = errors.New("episode not found")
)

// ─── playlists ─────────────────────────────────────────────────────

// ListPlaylists returns the distinct playlist names that appear on any
// show doc. Cross-partition — rare, called only by humans.
//
// The query is a plain projection, NOT `SELECT DISTINCT`: the azcosmos
// Go SDK can't perform the cross-partition merge that DISTINCT (and
// ORDER BY / GROUP BY / aggregates) require — the gateway rejects it
// with "cross partition query can not be directly served by the gateway"
// (surfacing as a 500 on GET /api/playlists). A bare `SELECT VALUE
// c.playlist FROM c` streams every doc's playlist across partitions and
// we dedupe with the `seen` map below — same result, server-supported.
func (s *Store) ListPlaylists(ctx context.Context) ([]Playlist, error) {
	pager := s.shows.NewQueryItemsPager(
		"SELECT VALUE c.playlist FROM c",
		azcosmos.NewPartitionKey(),
		nil,
	)
	seen := map[string]struct{}{}
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, raw := range page.Items {
			var name string
			if err := json.Unmarshal(raw, &name); err != nil {
				continue
			}
			seen[name] = struct{}{}
		}
	}
	out := make([]Playlist, 0, len(seen))
	for n := range seen {
		out = append(out, Playlist{Name: n})
	}
	return out, nil
}

// playlistExists is a convenience for endpoints that take a playlist
// name in the URL — Cosmos has no first-class playlist entity, so we
// approximate "this playlist exists" as "at least one show is in it."
// Special-case the default "nelson" so an empty cluster still lets
// /api/playlists/nelson/next-round return an empty round instead of 404.
func (s *Store) playlistExists(ctx context.Context, name string) (bool, error) {
	if name == "nelson" {
		return true, nil
	}
	pager := s.shows.NewQueryItemsPager(
		"SELECT VALUE COUNT(1) FROM c WHERE c.playlist = @pl",
		azcosmos.NewPartitionKeyString(name),
		&azcosmos.QueryOptions{
			QueryParameters: []azcosmos.QueryParameter{{Name: "@pl", Value: name}},
		},
	)
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return false, err
		}
		for _, raw := range page.Items {
			var n int
			if err := json.Unmarshal(raw, &n); err != nil {
				continue
			}
			if n > 0 {
				return true, nil
			}
		}
	}
	return false, nil
}

// GetPlaylistByName resolves a playlist by name. Today playlists are
// just identifiers tagged onto show docs, so this is a thin wrapper
// around playlistExists.
func (s *Store) GetPlaylistByName(ctx context.Context, name string) (*Playlist, error) {
	ok, err := s.playlistExists(ctx, name)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, ErrPlaylistNotFound
	}
	return &Playlist{Name: name}, nil
}

// ─── shows ─────────────────────────────────────────────────────────

func (s *Store) ListActiveShows(ctx context.Context, playlist string) ([]Show, error) {
	pager := s.shows.NewQueryItemsPager(
		"SELECT * FROM c WHERE c.playlist = @pl AND (NOT IS_DEFINED(c.removed_at) OR IS_NULL(c.removed_at)) ORDER BY c.name",
		azcosmos.NewPartitionKeyString(playlist),
		&azcosmos.QueryOptions{
			QueryParameters: []azcosmos.QueryParameter{{Name: "@pl", Value: playlist}},
		},
	)
	var out []Show
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, raw := range page.Items {
			var d showDoc
			if err := json.Unmarshal(raw, &d); err != nil {
				return nil, err
			}
			out = append(out, showFromDoc(d))
		}
	}
	return out, nil
}

func showFromDoc(d showDoc) Show {
	return Show{
		ID:        d.ID,
		Playlist:  d.Playlist,
		Name:      d.Name,
		RootPath:  d.RootPath,
		DateAdded: d.DateAdded,
		RemovedAt: d.RemovedAt,
	}
}

// GetShow does a point read when the caller knows the partition key.
// Used by Advance, which has the playlist in context.
func (s *Store) GetShow(ctx context.Context, playlist, id string) (*Show, error) {
	d, err := s.readShow(ctx, playlist, id)
	if err != nil {
		return nil, err
	}
	out := showFromDoc(*d)
	return &out, nil
}

// FindShow does a cross-partition lookup by id alone. Used by the
// /api/shows/:id endpoints — humans don't always have the playlist on
// hand. With ~40 shows in this single-user instance, the cross-
// partition cost is trivial.
func (s *Store) FindShow(ctx context.Context, id string) (*Show, error) {
	d, err := s.findShowDoc(ctx, id)
	if err != nil {
		return nil, err
	}
	out := showFromDoc(*d)
	return &out, nil
}

func (s *Store) findShowDoc(ctx context.Context, id string) (*showDoc, error) {
	pager := s.shows.NewQueryItemsPager(
		"SELECT * FROM c WHERE c.id = @id",
		azcosmos.NewPartitionKey(),
		&azcosmos.QueryOptions{
			QueryParameters: []azcosmos.QueryParameter{{Name: "@id", Value: id}},
		},
	)
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, raw := range page.Items {
			var d showDoc
			if err := json.Unmarshal(raw, &d); err != nil {
				return nil, err
			}
			return &d, nil
		}
	}
	return nil, ErrShowNotFound
}

// AppendEpisodesByID is the cross-partition variant for the /api/shows/:id/episodes
// endpoint that doesn't take a playlist context. Internally it looks
// the show up, then delegates to the same write path.
func (s *Store) AppendEpisodesByID(ctx context.Context, showID string, episodes []string) (int, error) {
	if len(episodes) == 0 {
		return 0, nil
	}
	d, err := s.findShowDoc(ctx, showID)
	if err != nil {
		return 0, err
	}
	return s.AppendEpisodes(ctx, d.Playlist, d.ID, episodes)
}

func (s *Store) readShow(ctx context.Context, playlist, id string) (*showDoc, error) {
	resp, err := s.shows.ReadItem(ctx, azcosmos.NewPartitionKeyString(playlist), id, nil)
	if err != nil {
		if isCosmosNotFound(err) {
			return nil, ErrShowNotFound
		}
		return nil, err
	}
	var d showDoc
	if err := json.Unmarshal(resp.Value, &d); err != nil {
		return nil, err
	}
	return &d, nil
}

// CreateShow writes a new show doc with its initial episode queue.
// Positions start at 0 and increment in input order. IDs are
// server-generated UUIDs.
func (s *Store) CreateShow(
	ctx context.Context,
	playlist, name, rootPath string,
	dateAdded time.Time,
	episodes []string,
) (*Show, error) {
	now := time.Now().UTC()
	d := showDoc{
		ID:        uuid.NewString(),
		Playlist:  playlist,
		Name:      name,
		RootPath:  rootPath,
		DateAdded: dateAdded.UTC(),
		CreatedAt: now,
		UpdatedAt: now,
		Episodes:  make([]episodeDoc, len(episodes)),
	}
	for i, rel := range episodes {
		d.Episodes[i] = episodeDoc{
			ID:           uuid.NewString(),
			RelativePath: rel,
			Position:     i,
			UpdatedAt:    now,
		}
	}
	raw, err := json.Marshal(d)
	if err != nil {
		return nil, err
	}
	_, err = s.shows.CreateItem(ctx, azcosmos.NewPartitionKeyString(playlist), raw, nil)
	if err != nil {
		return nil, fmt.Errorf("create show: %w", err)
	}
	out := showFromDoc(d)
	return &out, nil
}

// AppendEpisodes adds episodes to an existing show's queue. Positions
// continue from the show's current max(position) + 1.
func (s *Store) AppendEpisodes(ctx context.Context, playlist, showID string, episodes []string) (int, error) {
	if len(episodes) == 0 {
		return 0, nil
	}
	d, err := s.readShow(ctx, playlist, showID)
	if err != nil {
		return 0, err
	}
	start := 0
	for _, ep := range d.Episodes {
		if ep.Position >= start {
			start = ep.Position + 1
		}
	}
	now := time.Now().UTC()
	for i, rel := range episodes {
		d.Episodes = append(d.Episodes, episodeDoc{
			ID:           uuid.NewString(),
			RelativePath: rel,
			Position:     start + i,
			UpdatedAt:    now,
		})
	}
	d.UpdatedAt = now
	if err := s.writeShow(ctx, d); err != nil {
		return 0, err
	}
	return len(episodes), nil
}

func (s *Store) writeShow(ctx context.Context, d *showDoc) error {
	raw, err := json.Marshal(d)
	if err != nil {
		return err
	}
	_, err = s.shows.ReplaceItem(ctx, azcosmos.NewPartitionKeyString(d.Playlist), d.ID, raw, nil)
	return err
}

// ─── offline sync (library pull + record push) ─────────────────────

// FullLibrary returns every show (including removed/tombstoned, so the client
// learns of tombstones) in the named playlists, episodes embedded — the
// client's seed/reconcile pull. Queried per-playlist (partition-scoped).
func (s *Store) FullLibrary(ctx context.Context, playlists []string) ([]LibraryShow, error) {
	out := make([]LibraryShow, 0)
	for _, pl := range playlists {
		pager := s.shows.NewQueryItemsPager(
			"SELECT * FROM c WHERE c.playlist = @pl",
			azcosmos.NewPartitionKeyString(pl),
			&azcosmos.QueryOptions{QueryParameters: []azcosmos.QueryParameter{{Name: "@pl", Value: pl}}},
		)
		for pager.More() {
			page, err := pager.NextPage(ctx)
			if err != nil {
				return nil, err
			}
			for _, raw := range page.Items {
				var d showDoc
				if err := json.Unmarshal(raw, &d); err != nil {
					return nil, err
				}
				out = append(out, libraryShowFromDoc(d))
			}
		}
	}
	return out, nil
}

func libraryShowFromDoc(d showDoc) LibraryShow {
	eps := make([]LibraryEpisode, len(d.Episodes))
	for i, e := range d.Episodes {
		eps[i] = LibraryEpisode{
			ID: e.ID, RelativePath: e.RelativePath, Position: e.Position,
			WatchedAt: e.WatchedAt, ResumePos: e.ResumePos, UpdatedAt: e.UpdatedAt,
		}
	}
	return LibraryShow{
		ID: d.ID, Playlist: d.Playlist, Name: d.Name, RootPath: d.RootPath,
		DateAdded: d.DateAdded, RemovedAt: d.RemovedAt, UpdatedAt: d.UpdatedAt, Episodes: eps,
	}
}

// SyncUpsert applies a batch of client changes, last-write-wins by updated_at.
// Show + episode changes are grouped by show and folded into one read-modify-
// write per show doc (episodes are embedded). History rows are appended,
// idempotently (duplicate ids ignored). A show change for an id that doesn't
// exist yet creates it (a locally-created show synced up).
func (s *Store) SyncUpsert(ctx context.Context, req SyncRequest) error {
	showByID := map[string]SyncShow{}
	for _, sh := range req.Shows {
		showByID[sh.ID] = sh
	}
	epsByShow := map[string][]SyncEpisode{}
	for _, e := range req.Episodes {
		epsByShow[e.ShowID] = append(epsByShow[e.ShowID], e)
	}
	affected := map[string]struct{}{}
	for id := range showByID {
		affected[id] = struct{}{}
	}
	for id := range epsByShow {
		affected[id] = struct{}{}
	}

	for id := range affected {
		d, err := s.findShowDoc(ctx, id)
		if errors.Is(err, ErrShowNotFound) {
			sh, ok := showByID[id]
			if !ok {
				continue // episodes for an unknown show; its show record will arrive
			}
			nd := showDoc{
				ID: sh.ID, Playlist: sh.Playlist, Name: sh.Name, RootPath: sh.RootPath,
				DateAdded: sh.DateAdded, RemovedAt: sh.RemovedAt,
				CreatedAt: sh.UpdatedAt, UpdatedAt: sh.UpdatedAt,
			}
			applyEpisodeSyncs(&nd, epsByShow[id])
			raw, err := json.Marshal(nd)
			if err != nil {
				return err
			}
			if _, err := s.shows.CreateItem(ctx, azcosmos.NewPartitionKeyString(nd.Playlist), raw, nil); err != nil {
				return fmt.Errorf("sync create %s: %w", id, err)
			}
			continue
		}
		if err != nil {
			return err
		}
		if sh, ok := showByID[id]; ok && sh.UpdatedAt.After(d.UpdatedAt) {
			d.Name, d.RootPath, d.Playlist = sh.Name, sh.RootPath, sh.Playlist
			d.DateAdded, d.RemovedAt, d.UpdatedAt = sh.DateAdded, sh.RemovedAt, sh.UpdatedAt
		}
		applyEpisodeSyncs(d, epsByShow[id])
		if err := s.writeShow(ctx, d); err != nil {
			return fmt.Errorf("sync write %s: %w", id, err)
		}
	}

	for _, h := range req.History {
		hd := historyDoc{
			ID: h.ID, ShowID: h.ShowID, EpisodeID: h.EpisodeID,
			RelativePath: h.RelativePath, PlayedAt: h.PlayedAt,
		}
		raw, err := json.Marshal(hd)
		if err != nil {
			return err
		}
		_, err = s.history.CreateItem(ctx, azcosmos.NewPartitionKeyString(h.ShowID), raw, nil)
		if err != nil && !isCosmosConflict(err) {
			return fmt.Errorf("sync history %s: %w", h.ID, err)
		}
	}
	return nil
}

// applyEpisodeSyncs folds episode changes into a show doc, last-write-wins per
// episode; an unknown episode id is appended (a locally-added episode).
func applyEpisodeSyncs(d *showDoc, eps []SyncEpisode) {
	for _, se := range eps {
		found := false
		for i := range d.Episodes {
			if d.Episodes[i].ID != se.ID {
				continue
			}
			if se.UpdatedAt.After(d.Episodes[i].UpdatedAt) {
				d.Episodes[i].RelativePath = se.RelativePath
				d.Episodes[i].Position = se.Position
				d.Episodes[i].WatchedAt = se.WatchedAt
				d.Episodes[i].ResumePos = se.ResumePos
				d.Episodes[i].UpdatedAt = se.UpdatedAt
			}
			found = true
			break
		}
		if !found {
			d.Episodes = append(d.Episodes, episodeDoc{
				ID: se.ID, RelativePath: se.RelativePath, Position: se.Position,
				WatchedAt: se.WatchedAt, ResumePos: se.ResumePos, UpdatedAt: se.UpdatedAt,
			})
		}
	}
}

// ShowHistory returns the per-show play log ordered by played_at.
func (s *Store) ShowHistory(ctx context.Context, showID string) ([]HistoryEvent, error) {
	pager := s.history.NewQueryItemsPager(
		"SELECT * FROM c WHERE c.show_id = @s ORDER BY c.played_at",
		azcosmos.NewPartitionKeyString(showID),
		&azcosmos.QueryOptions{
			QueryParameters: []azcosmos.QueryParameter{{Name: "@s", Value: showID}},
		},
	)
	var out []HistoryEvent
	for pager.More() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			return nil, err
		}
		for _, raw := range page.Items {
			var d historyDoc
			if err := json.Unmarshal(raw, &d); err != nil {
				return nil, err
			}
			out = append(out, HistoryEvent{
				EpisodeID:    d.EpisodeID,
				RelativePath: d.RelativePath,
				PlayedAt:     d.PlayedAt,
			})
		}
	}
	return out, nil
}

// ─── helpers ───────────────────────────────────────────────────────

// isCosmosNotFound returns true if err is a 404 from the Cosmos SDK.
// The SDK wraps the HTTP status on *azcore.ResponseError; check it
// explicitly rather than relying on string matching.
func isCosmosNotFound(err error) bool {
	var rerr *azcore.ResponseError
	if errors.As(err, &rerr) {
		return rerr.StatusCode == 404
	}
	return strings.Contains(err.Error(), "404")
}

// isCosmosConflict reports a 409 (item id already exists) — used to make
// history inserts idempotent on sync replay.
func isCosmosConflict(err error) bool {
	var rerr *azcore.ResponseError
	if errors.As(err, &rerr) {
		return rerr.StatusCode == 409
	}
	return false
}
