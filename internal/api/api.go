// Package api wires the HTTP routes for shows-api.
//
// All /api/* routes are gated by the auth.romaine.life JWT verifier from
// internal/auth. The /healthz + /readyz routes are unauthenticated so the
// k8s probes can hit them without an Authorization header.
package api

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/nelsong6/shows/internal/auth"
	"github.com/nelsong6/shows/internal/ordering"
	"github.com/nelsong6/shows/internal/store"
)

type Server struct {
	Store    *store.Store
	Verifier *auth.Verifier
}

// Router builds the chi router with all routes mounted.
func (s *Server) Router() http.Handler {
	r := chi.NewRouter()

	r.Use(middleware.RequestID)
	r.Use(middleware.RealIP)
	r.Use(middleware.Logger)
	r.Use(middleware.Recoverer)
	r.Use(middleware.Timeout(30 * time.Second))
	r.Use(metricsMiddleware)

	r.Get("/healthz", func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("ok"))
	})
	r.Get("/readyz", s.handleReady)

	// /metrics — unauthenticated. kube-prometheus-stack scrapes via
	// the PodMonitor in k8s/templates/podmonitor.yaml. The handler
	// exposes shows_* counters/histograms plus prom client's default
	// Go runtime / process metrics.
	r.Method(http.MethodGet, "/metrics", promhttp.Handler())

	r.Route("/api", func(r chi.Router) {
		r.Use(auth.Middleware(s.Verifier))

		r.Get("/playlists", s.handleListPlaylists)
		r.Get("/playlists/{name}", s.handleGetPlaylist)
		r.Get("/playlists/{name}/next-round", s.handleNextRound)
		r.Post("/playlists/{name}/advance", s.handleAdvance)
		r.Post("/playlists/{name}/defer-show", s.handleDeferShow)

		// Cross-playlist round (additive; the per-playlist routes above stay
		// the primary path). See round-and-advance.md "Cross-playlist rounds".
		r.Get("/rounds", s.handleCrossRound)
		r.Post("/rounds/advance", s.handleCrossAdvance)

		// Offline-first sync: the desktop is the engine, this is its durable
		// origin. /library is the seed/reconcile pull; /sync is the push of
		// locally-changed records (last-write-wins). These will supersede the
		// round endpoints above once the offline client ships.
		r.Get("/library", s.handleLibrary)
		r.Post("/sync", s.handleSync)

		r.Post("/shows", s.handleCreateShow)
		r.Get("/shows/{id}", s.handleGetShow)
		r.Post("/shows/{id}/episodes", s.handleAppendEpisodes)
		r.Get("/shows/{id}/history", s.handleShowHistory)

		r.Post("/migrate/from-json", s.handleMigrateFromJSON)
	})

	return r
}

// ─── helpers ───────────────────────────────────────────────────────

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if v != nil {
		_ = json.NewEncoder(w).Encode(v)
	}
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

func (s *Server) requirePlaylist(w http.ResponseWriter, r *http.Request) *store.Playlist {
	name := chi.URLParam(r, "name")
	p, err := s.Store.GetPlaylistByName(r.Context(), name)
	if errors.Is(err, store.ErrPlaylistNotFound) {
		writeErr(w, http.StatusNotFound, "playlist not found")
		return nil
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return nil
	}
	return p
}

// ─── probes ────────────────────────────────────────────────────────

func (s *Server) handleReady(w http.ResponseWriter, r *http.Request) {
	if err := s.Store.Ping(r.Context()); err != nil {
		writeErr(w, http.StatusServiceUnavailable, "db unreachable")
		return
	}
	_, _ = w.Write([]byte("ready"))
}

// ─── playlists ─────────────────────────────────────────────────────

func (s *Server) handleListPlaylists(w http.ResponseWriter, r *http.Request) {
	ps, err := s.Store.ListPlaylists(r.Context())
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"playlists": ps})
}

func (s *Server) handleGetPlaylist(w http.ResponseWriter, r *http.Request) {
	p := s.requirePlaylist(w, r)
	if p == nil {
		return
	}
	shows, err := s.Store.ListActiveShows(r.Context(), p.Name)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"playlist": p,
		"shows":    shows,
	})
}

// RoundEntry is one episode in a next-round response. show_id is the
// partition key of the show doc; the client echoes both ids back on
// advance so the API can do point operations without a cross-partition
// scan.
type RoundEntry struct {
	ShowID       string `json:"show_id"`
	ShowName     string `json:"show_name"`
	EpisodeID    string `json:"episode_id"`
	AbsolutePath string `json:"absolute_path"`
	OrderValue   uint32 `json:"order_value"`
	// Playlist is set only on cross-playlist rounds (GET /api/rounds) so the
	// client can route each entry's advance back; empty/omitted on the
	// single-playlist next-round.
	Playlist string `json:"playlist,omitempty"`
}

// roundFromOrdered maps store NextEpisode rows + their ordering.Sort output
// into the wire RoundEntry list. nameByEpisode / playlistByEpisode carry the
// fields ordering.Candidate doesn't.
func roundFromOrdered(ordered []ordering.Ordered, nameByEpisode, playlistByEpisode map[string]string) []RoundEntry {
	out := make([]RoundEntry, len(ordered))
	for i, o := range ordered {
		out[i] = RoundEntry{
			ShowID:       o.ShowID,
			ShowName:     nameByEpisode[o.EpisodeID],
			EpisodeID:    o.EpisodeID,
			AbsolutePath: o.AbsolutePath,
			OrderValue:   o.OrderValue,
			Playlist:     playlistByEpisode[o.EpisodeID],
		}
	}
	return out
}

func (s *Server) handleNextRound(w http.ResponseWriter, r *http.Request) {
	p := s.requirePlaylist(w, r)
	if p == nil {
		return
	}
	next, err := s.Store.NextEpisodes(r.Context(), p.Name)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	if len(next) == 0 {
		roundResponseSize.Observe(0)
		writeJSON(w, http.StatusOK, map[string]any{"round": []RoundEntry{}})
		return
	}
	roundResponseSize.Observe(float64(len(next)))

	cands := make([]ordering.Candidate, len(next))
	for i, n := range next {
		cands[i] = ordering.Candidate{
			EpisodeID:    n.EpisodeID,
			ShowID:       n.ShowID,
			RootPath:     n.RootPath,
			RelativePath: n.RelativePath,
		}
	}
	ordered := ordering.Sort(cands)

	// Map back ShowName from the store rows (Candidate doesn't carry it).
	nameByEpisode := make(map[string]string, len(next))
	for _, n := range next {
		nameByEpisode[n.EpisodeID] = n.ShowName
	}

	writeJSON(w, http.StatusOK, map[string]any{
		"round": roundFromOrdered(ordered, nameByEpisode, nil),
	})
}

type advanceRequest struct {
	Entries []store.AdvanceEntry `json:"entries"`
}

// RemovedShow is the per-show payload on /advance when a show's queue
// emptied. Lets the client render the "this show took N days to watch"
// reveal without a follow-up request.
type RemovedShow struct {
	ID           string    `json:"id"`
	Name         string    `json:"name"`
	DateAdded    time.Time `json:"date_added"`
	LastPlayedAt time.Time `json:"last_played_at"`
}

func (s *Server) handleAdvance(w http.ResponseWriter, r *http.Request) {
	p := s.requirePlaylist(w, r)
	if p == nil {
		return
	}
	var req advanceRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if len(req.Entries) == 0 {
		writeErr(w, http.StatusBadRequest, "entries is required")
		return
	}

	result, err := s.Store.Advance(r.Context(), p.Name, req.Entries)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	advancedEpisodesTotal.Add(float64(result.AdvancedCount))
	removedShowsTotal.Add(float64(len(result.RemovedShowIDs)))

	writeJSON(w, http.StatusOK, map[string]any{
		"advanced_count": result.AdvancedCount,
		"removed_shows":  s.removedShowsPayload(r.Context(), p.Name, result.RemovedShowIDs),
	})
}

// removedShowsPayload builds the per-show reveal ("took N days to watch")
// for shows tombstoned by an advance — one row each, with the last play
// time pulled from watch_history. Shared by /advance and /rounds/advance.
func (s *Server) removedShowsPayload(ctx context.Context, playlist string, ids []string) []RemovedShow {
	removed := make([]RemovedShow, 0, len(ids))
	for _, id := range ids {
		sh, err := s.Store.GetShow(ctx, playlist, id)
		if err != nil {
			continue
		}
		history, err := s.Store.ShowHistory(ctx, id)
		var last time.Time
		if err == nil && len(history) > 0 {
			last = history[len(history)-1].PlayedAt
		}
		removed = append(removed, RemovedShow{
			ID:           sh.ID,
			Name:         sh.Name,
			DateAdded:    sh.DateAdded,
			LastPlayedAt: last,
		})
	}
	return removed
}

// ─── defer (swap a show's next-round pick) ─────────────────────────

type deferShowRequest struct {
	ShowID    string `json:"show_id"`
	EpisodeID string `json:"episode_id"`
}

func (s *Server) handleDeferShow(w http.ResponseWriter, r *http.Request) {
	p := s.requirePlaylist(w, r)
	if p == nil {
		return
	}
	var req deferShowRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if req.ShowID == "" || req.EpisodeID == "" {
		writeErr(w, http.StatusBadRequest, "show_id and episode_id are required")
		return
	}
	err := s.Store.DeferEpisode(r.Context(), p.Name, req.ShowID, req.EpisodeID)
	if errors.Is(err, store.ErrShowNotFound) || errors.Is(err, store.ErrEpisodeNotFound) {
		// D3: deferring an absent/already-watched episode is a no-op.
		writeErr(w, http.StatusNotFound, "show or unwatched episode not found")
		return
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	deferredEpisodesTotal.Inc()
	w.WriteHeader(http.StatusNoContent)
}

// ─── cross-playlist rounds ─────────────────────────────────────────

func parsePlaylists(raw string) []string {
	var out []string
	for _, p := range strings.Split(raw, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}

func (s *Server) handleCrossRound(w http.ResponseWriter, r *http.Request) {
	playlists := parsePlaylists(r.URL.Query().Get("playlists"))
	if len(playlists) == 0 {
		writeErr(w, http.StatusBadRequest, "playlists query param is required (comma-separated)")
		return
	}
	next, err := s.Store.NextEpisodesMulti(r.Context(), playlists)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	roundResponseSize.Observe(float64(len(next)))
	if len(next) == 0 {
		writeJSON(w, http.StatusOK, map[string]any{"round": []RoundEntry{}})
		return
	}

	cands := make([]ordering.Candidate, len(next))
	nameByEpisode := make(map[string]string, len(next))
	playlistByEpisode := make(map[string]string, len(next))
	for i, n := range next {
		cands[i] = ordering.Candidate{
			EpisodeID:    n.EpisodeID,
			ShowID:       n.ShowID,
			RootPath:     n.RootPath,
			RelativePath: n.RelativePath,
		}
		nameByEpisode[n.EpisodeID] = n.ShowName
		playlistByEpisode[n.EpisodeID] = n.Playlist
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"round": roundFromOrdered(ordering.Sort(cands), nameByEpisode, playlistByEpisode),
	})
}

type crossAdvanceEntry struct {
	Playlist  string `json:"playlist"`
	ShowID    string `json:"show_id"`
	EpisodeID string `json:"episode_id"`
}

type crossAdvanceRequest struct {
	Entries []crossAdvanceEntry `json:"entries"`
}

func (s *Server) handleCrossAdvance(w http.ResponseWriter, r *http.Request) {
	var req crossAdvanceRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if len(req.Entries) == 0 {
		writeErr(w, http.StatusBadRequest, "entries is required")
		return
	}
	// Group by playlist; each playlist advances independently (X2).
	byPlaylist := map[string][]store.AdvanceEntry{}
	for _, e := range req.Entries {
		if e.Playlist == "" || e.ShowID == "" || e.EpisodeID == "" {
			writeErr(w, http.StatusBadRequest, "each entry needs playlist, show_id, episode_id")
			return
		}
		byPlaylist[e.Playlist] = append(byPlaylist[e.Playlist],
			store.AdvanceEntry{ShowID: e.ShowID, EpisodeID: e.EpisodeID})
	}

	advanced := 0
	removed := make([]RemovedShow, 0)
	for pl, entries := range byPlaylist {
		result, err := s.Store.Advance(r.Context(), pl, entries)
		if err != nil {
			writeErr(w, http.StatusInternalServerError, err.Error())
			return
		}
		advanced += result.AdvancedCount
		removed = append(removed, s.removedShowsPayload(r.Context(), pl, result.RemovedShowIDs)...)
	}
	advancedEpisodesTotal.Add(float64(advanced))
	removedShowsTotal.Add(float64(len(removed)))
	writeJSON(w, http.StatusOK, map[string]any{
		"advanced_count": advanced,
		"removed_shows":  removed,
	})
}

// ─── offline sync ──────────────────────────────────────────────────

// handleLibrary is the client's seed/reconcile pull: all shows (incl. removed)
// + embedded episodes for the named playlists, each carrying updated_at.
func (s *Server) handleLibrary(w http.ResponseWriter, r *http.Request) {
	playlists := parsePlaylists(r.URL.Query().Get("playlists"))
	if len(playlists) == 0 {
		writeErr(w, http.StatusBadRequest, "playlists query param is required (comma-separated)")
		return
	}
	shows, err := s.Store.FullLibrary(r.Context(), playlists)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"shows": shows})
}

// handleSync is the client's push: a batch of locally-changed shows/episodes/
// history, applied last-write-wins by updated_at (idempotent on replay).
func (s *Server) handleSync(w http.ResponseWriter, r *http.Request) {
	var req store.SyncRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if err := s.Store.SyncUpsert(r.Context(), req); err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	syncedRecordsTotal.Add(float64(len(req.Shows) + len(req.Episodes) + len(req.History)))
	w.WriteHeader(http.StatusNoContent)
}

// ─── shows ─────────────────────────────────────────────────────────

type createShowRequest struct {
	Playlist  string    `json:"playlist"`
	Name      string    `json:"name"`
	RootPath  string    `json:"root_path"`
	DateAdded time.Time `json:"date_added"`
	Episodes  []string  `json:"episodes"`
}

func (s *Server) handleCreateShow(w http.ResponseWriter, r *http.Request) {
	var req createShowRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if req.Playlist == "" || req.Name == "" || req.RootPath == "" || len(req.Episodes) == 0 {
		writeErr(w, http.StatusBadRequest, "playlist, name, root_path, and episodes are required")
		return
	}
	if req.DateAdded.IsZero() {
		req.DateAdded = time.Now().UTC()
	}
	sh, err := s.Store.CreateShow(r.Context(), req.Playlist, req.Name, req.RootPath, req.DateAdded, req.Episodes)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusCreated, sh)
}

func (s *Server) handleGetShow(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if id == "" {
		writeErr(w, http.StatusBadRequest, "id is required")
		return
	}
	sh, err := s.Store.FindShow(r.Context(), id)
	if errors.Is(err, store.ErrShowNotFound) {
		writeErr(w, http.StatusNotFound, "show not found")
		return
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, sh)
}

type appendEpisodesRequest struct {
	Episodes []string `json:"episodes"`
}

func (s *Server) handleAppendEpisodes(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if id == "" {
		writeErr(w, http.StatusBadRequest, "id is required")
		return
	}
	var req appendEpisodesRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if len(req.Episodes) == 0 {
		writeErr(w, http.StatusBadRequest, "episodes is required")
		return
	}
	n, err := s.Store.AppendEpisodesByID(r.Context(), id, req.Episodes)
	if errors.Is(err, store.ErrShowNotFound) {
		writeErr(w, http.StatusNotFound, "show not found")
		return
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"appended": n})
}

func (s *Server) handleShowHistory(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	if id == "" {
		writeErr(w, http.StatusBadRequest, "id is required")
		return
	}
	hist, err := s.Store.ShowHistory(r.Context(), id)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"history": hist})
}

// ─── migrate ───────────────────────────────────────────────────────

type migrateRequest struct {
	Playlist string                `json:"playlist"`
	Shows    []createShowRequest   `json:"shows"`
}

// handleMigrateFromJSON is the bulk-import endpoint cmd/shows-migrate
// posts to. Each show is created in its own write; partial success is
// possible (failed shows are reported in the response).
func (s *Server) handleMigrateFromJSON(w http.ResponseWriter, r *http.Request) {
	var req migrateRequest
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeErr(w, http.StatusBadRequest, "invalid json")
		return
	}
	if req.Playlist == "" || len(req.Shows) == 0 {
		writeErr(w, http.StatusBadRequest, "playlist and shows are required")
		return
	}

	type result struct {
		Name  string `json:"name"`
		ID    string `json:"id,omitempty"`
		Error string `json:"error,omitempty"`
	}
	out := make([]result, 0, len(req.Shows))
	for _, sh := range req.Shows {
		da := sh.DateAdded
		if da.IsZero() {
			da = time.Now().UTC()
		}
		created, err := s.Store.CreateShow(r.Context(), req.Playlist, sh.Name, sh.RootPath, da, sh.Episodes)
		if err != nil {
			out = append(out, result{Name: sh.Name, Error: err.Error()})
			continue
		}
		out = append(out, result{Name: sh.Name, ID: created.ID})
	}
	writeJSON(w, http.StatusOK, map[string]any{"results": out})
}

// silence imports when only used via store types
var _ = fmt.Sprintf
