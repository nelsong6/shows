// Package api wires the HTTP routes for shows-api.
//
// All /api/* routes are gated by the auth.romaine.life JWT verifier from
// internal/auth. The /healthz + /readyz routes are unauthenticated so the
// k8s probes can hit them without an Authorization header.
package api

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
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

	out := make([]RoundEntry, len(ordered))
	for i, o := range ordered {
		out[i] = RoundEntry{
			ShowID:       o.ShowID,
			ShowName:     nameByEpisode[o.EpisodeID],
			EpisodeID:    o.EpisodeID,
			AbsolutePath: o.AbsolutePath,
			OrderValue:   o.OrderValue,
		}
	}
	writeJSON(w, http.StatusOK, map[string]any{"round": out})
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

	// Build the "removed shows" payload so the client can show the
	// reveal. One row per show that just got tombstoned.
	removed := make([]RemovedShow, 0, len(result.RemovedShowIDs))
	for _, id := range result.RemovedShowIDs {
		sh, err := s.Store.GetShow(r.Context(), p.Name, id)
		if err != nil {
			continue
		}
		history, err := s.Store.ShowHistory(r.Context(), id)
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

	writeJSON(w, http.StatusOK, map[string]any{
		"advanced_count": result.AdvancedCount,
		"removed_shows":  removed,
	})
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
