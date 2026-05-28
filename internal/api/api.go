// Package api wires the HTTP routes for shows-api.
//
// Under the offline-first design the desktop is the engine; this server is a
// durable origin it syncs with. The client's only round-trip is GET /library
// (seed/reconcile pull) + POST /sync (push locally-changed records, LWW). The
// round endpoints + server-side round engine were removed — the client computes
// rounds locally now. A few read/utility endpoints remain for humans/debugging.
//
// All /api/* routes are gated by the auth.romaine.life JWT verifier from
// internal/auth. The /healthz + /readyz routes are unauthenticated so the
// k8s probes can hit them without an Authorization header.
package api

import (
	"encoding/json"
	"errors"
	"net/http"
	"strings"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/nelsong6/shows/internal/auth"
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
	// the PodMonitor in k8s/templates/podmonitor.yaml.
	r.Method(http.MethodGet, "/metrics", promhttp.Handler())

	r.Route("/api", func(r chi.Router) {
		r.Use(auth.Middleware(s.Verifier))

		// The desktop is the engine; this server is its durable origin.
		// /library is the seed/reconcile pull, /sync the push of locally-changed
		// records (last-write-wins) — the client's only round-trip.
		r.Get("/library", s.handleLibrary)
		r.Post("/sync", s.handleSync)

		// Read/utility endpoints (humans / debugging).
		r.Get("/playlists", s.handleListPlaylists)
		r.Get("/playlists/{name}", s.handleGetPlaylist)
		r.Post("/shows", s.handleCreateShow)
		r.Get("/shows/{id}", s.handleGetShow)
		r.Post("/shows/{id}/episodes", s.handleAppendEpisodes)
		r.Get("/shows/{id}/history", s.handleShowHistory)
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

// parsePlaylists splits the comma-separated ?playlists= query (used by /library).
func parsePlaylists(raw string) []string {
	var out []string
	for _, p := range strings.Split(raw, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}

// ─── probes ────────────────────────────────────────────────────────

func (s *Server) handleReady(w http.ResponseWriter, r *http.Request) {
	if err := s.Store.Ping(r.Context()); err != nil {
		writeErr(w, http.StatusServiceUnavailable, "db unreachable")
		return
	}
	_, _ = w.Write([]byte("ready"))
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
	writeJSON(w, http.StatusOK, map[string]any{"playlist": p, "shows": shows})
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
