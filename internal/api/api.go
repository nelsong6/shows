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
	"strconv"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"

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

	// Probes — unauthenticated.
	r.Get("/healthz", func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("ok"))
	})
	r.Get("/readyz", s.handleReady)

	// Authenticated surface.
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

func parseInt64Param(r *http.Request, key string) (int64, error) {
	raw := chi.URLParam(r, key)
	id, err := strconv.ParseInt(raw, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("invalid %s: %q", key, raw)
	}
	return id, nil
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
	if err := s.Store.Pool().Ping(r.Context()); err != nil {
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
	shows, err := s.Store.ListActiveShows(r.Context(), p.ID)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"playlist": p,
		"shows":    shows,
	})
}

// RoundEntry is one episode in a next-round response, including the
// absolute path the client feeds to mpv and the deterministic order_value
// so the client (or a future debug UI) can verify the ordering.
type RoundEntry struct {
	ShowID       int64  `json:"show_id"`
	ShowName     string `json:"show_name"`
	EpisodeID    int64  `json:"episode_id"`
	AbsolutePath string `json:"absolute_path"`
	OrderValue   uint32 `json:"order_value"`
}

func (s *Server) handleNextRound(w http.ResponseWriter, r *http.Request) {
	p := s.requirePlaylist(w, r)
	if p == nil {
		return
	}
	next, err := s.Store.NextEpisodes(r.Context(), p.ID)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	if len(next) == 0 {
		writeJSON(w, http.StatusOK, map[string]any{"round": []RoundEntry{}})
		return
	}

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
	nameByEpisode := make(map[int64]string, len(next))
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
	EpisodeIDs []int64 `json:"episode_ids"`
}

// RemovedShow is the per-show payload on /advance when a show's queue
// emptied. Lets the client render the "this show took N days to watch"
// reveal without a follow-up request.
type RemovedShow struct {
	ID           int64     `json:"id"`
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
	if len(req.EpisodeIDs) == 0 {
		writeErr(w, http.StatusBadRequest, "episode_ids is required")
		return
	}

	result, err := s.Store.Advance(r.Context(), req.EpisodeIDs)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}

	// Build the "removed shows" payload so the client can show the
	// reveal. One row per show that just got tombstoned.
	removed := make([]RemovedShow, 0, len(result.RemovedShowIDs))
	for _, id := range result.RemovedShowIDs {
		sh, err := s.Store.GetShow(r.Context(), id)
		if err != nil {
			// The show was just updated in the same tx as advance; a
			// not-found here would be a real bug. Surface and move on.
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
	p, err := s.Store.GetPlaylistByName(r.Context(), req.Playlist)
	if errors.Is(err, store.ErrPlaylistNotFound) {
		writeErr(w, http.StatusNotFound, "playlist not found")
		return
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	sh, err := s.Store.CreateShow(r.Context(), p.ID, req.Name, req.RootPath, req.DateAdded, req.Episodes)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusCreated, sh)
}

func (s *Server) handleGetShow(w http.ResponseWriter, r *http.Request) {
	id, err := parseInt64Param(r, "id")
	if err != nil {
		writeErr(w, http.StatusBadRequest, err.Error())
		return
	}
	sh, err := s.Store.GetShow(r.Context(), id)
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
	id, err := parseInt64Param(r, "id")
	if err != nil {
		writeErr(w, http.StatusBadRequest, err.Error())
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
	n, err := s.Store.AppendEpisodes(r.Context(), id, req.Episodes)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"appended": n})
}

func (s *Server) handleShowHistory(w http.ResponseWriter, r *http.Request) {
	id, err := parseInt64Param(r, "id")
	if err != nil {
		writeErr(w, http.StatusBadRequest, err.Error())
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
	Playlist string            `json:"playlist"`
	Shows    []createShowRequest `json:"shows"`
}

// handleMigrateFromJSON is the bulk-import endpoint cmd/shows-migrate
// posts to. Each show is created in its own transaction; partial success
// is possible (failed shows are reported in the response).
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
	p, err := s.Store.GetPlaylistByName(r.Context(), req.Playlist)
	if errors.Is(err, store.ErrPlaylistNotFound) {
		writeErr(w, http.StatusNotFound, "playlist not found")
		return
	}
	if err != nil {
		writeErr(w, http.StatusInternalServerError, err.Error())
		return
	}

	type result struct {
		Name  string `json:"name"`
		ID    int64  `json:"id,omitempty"`
		Error string `json:"error,omitempty"`
	}
	out := make([]result, 0, len(req.Shows))
	for _, sh := range req.Shows {
		da := sh.DateAdded
		if da.IsZero() {
			da = time.Now().UTC()
		}
		created, err := s.Store.CreateShow(r.Context(), p.ID, sh.Name, sh.RootPath, da, sh.Episodes)
		if err != nil {
			out = append(out, result{Name: sh.Name, Error: err.Error()})
			continue
		}
		out = append(out, result{Name: sh.Name, ID: created.ID})
	}
	writeJSON(w, http.StatusOK, map[string]any{"results": out})
}
