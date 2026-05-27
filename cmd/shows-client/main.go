//go:build windows

// shows-client is the local Windows binary that drives mpv in a
// never-ending playback loop against shows.romaine.life.
//
// Subcommands:
//
//	shows-client login          run the auth.romaine.life device flow,
//	                            cache the token at %APPDATA%\shows\token.json
//	shows-client play           play the configured playlist forever
//	shows-client logout         delete the cached token
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/nelsong6/shows/internal/device"
	"github.com/nelsong6/shows/internal/mpv"
)

const (
	defaultAPIBaseURL  = "https://shows.romaine.life"
	defaultAuthBaseURL = "https://auth.romaine.life"
	defaultPlaylist    = "nelson"

	envAPIBaseURL  = "SHOWS_API_URL"
	envAuthBaseURL = "SHOWS_AUTH_URL"
)

func main() {
	if len(os.Args) < 2 {
		printUsage()
		os.Exit(2)
	}
	cmd, args := os.Args[1], os.Args[2:]

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	var err error
	switch cmd {
	case "login":
		err = runLogin(ctx, args)
	case "logout":
		err = runLogout()
	case "play":
		err = runPlay(ctx, args)
	case "-h", "--help", "help":
		printUsage()
		return
	default:
		fmt.Fprintf(os.Stderr, "unknown command: %s\n\n", cmd)
		printUsage()
		os.Exit(2)
	}
	if err != nil && !errors.Is(err, context.Canceled) {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func printUsage() {
	fmt.Fprintln(os.Stderr, "usage: shows-client <command> [flags]")
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "commands:")
	fmt.Fprintln(os.Stderr, "  login            run device flow and cache a bot token")
	fmt.Fprintln(os.Stderr, "  logout           delete the cached token")
	fmt.Fprintln(os.Stderr, "  play [--playlist nelson]")
	fmt.Fprintln(os.Stderr, "                   play episodes from a playlist forever")
}

// ─── login / logout ────────────────────────────────────────────────

func runLogin(ctx context.Context, _ []string) error {
	host, _ := os.Hostname()
	info := device.RequesterInfo{
		WhereHappening: fmt.Sprintf("shows-client on %s", host),
		IntendedUse:    "play episodes via shows.romaine.life",
		MiscIdentifier: "couch",
	}
	tok, err := device.EnsureToken(ctx, authBaseURL(), info)
	if err != nil {
		return err
	}
	exp := time.Unix(tok.ExpiresAt, 0)
	fmt.Fprintf(os.Stderr, "logged in (token expires %s)\n", exp.Format(time.RFC3339))
	return nil
}

func runLogout() error {
	path, err := device.CachePath()
	if err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	fmt.Fprintln(os.Stderr, "token removed")
	return nil
}

// ─── play ──────────────────────────────────────────────────────────

func runPlay(ctx context.Context, args []string) error {
	fs := flag.NewFlagSet("play", flag.ExitOnError)
	playlist := fs.String("playlist", defaultPlaylist, "playlist name (default: nelson)")
	_ = fs.Parse(args)

	api := newAPIClient(apiBaseURL())
	if err := api.ensureToken(ctx); err != nil {
		return fmt.Errorf("auth: %w", err)
	}

	player, err := mpv.Start(ctx, mpv.Config{})
	if err != nil {
		return fmt.Errorf("mpv: %w", err)
	}
	defer player.Close()

	fmt.Fprintln(os.Stderr, "shows-client: connected to mpv, fetching round...")

	for {
		round, err := api.nextRound(ctx, *playlist)
		if err != nil {
			return fmt.Errorf("next-round: %w", err)
		}
		if len(round) == 0 {
			fmt.Fprintln(os.Stderr, "no shows left in this playlist — all queues drained")
			return nil
		}

		announceRound(round)
		if err := queueRound(ctx, player, round); err != nil {
			return fmt.Errorf("queue round: %w", err)
		}

		// Block until every queued file has ended naturally — or mpv
		// quits / ctx cancels.
		if err := waitForRound(ctx, player, len(round)); err != nil {
			return err
		}

		entries := make([]advanceEntry, len(round))
		for i, r := range round {
			entries[i] = advanceEntry{ShowID: r.ShowID, EpisodeID: r.EpisodeID}
		}
		adv, err := api.advance(ctx, *playlist, entries)
		if err != nil {
			return fmt.Errorf("advance: %w", err)
		}
		announceAdvance(adv)
	}
}

func announceRound(round []roundEntry) {
	fmt.Fprintf(os.Stderr, "\n--- round of %d episode(s) ---\n", len(round))
	for _, r := range round {
		fmt.Fprintf(os.Stderr, "  %s\n", r.ShowName)
	}
}

func announceAdvance(adv advanceResponse) {
	fmt.Fprintf(os.Stderr, "advanced %d episode(s)\n", adv.AdvancedCount)
	for _, sh := range adv.RemovedShows {
		days := int(sh.LastPlayedAt.Sub(sh.DateAdded).Hours() / 24)
		fmt.Fprintf(os.Stderr, "  * finished %q -- took %d days\n", sh.Name, days)
	}
}

func queueRound(ctx context.Context, player *mpv.Client, round []roundEntry) error {
	for i, r := range round {
		mode := mpv.LoadAppendPlay
		if i == 0 {
			mode = mpv.LoadReplace
		}
		if err := player.LoadFile(ctx, r.AbsolutePath, mode); err != nil {
			return fmt.Errorf("loadfile %d (%s): %w", i, r.AbsolutePath, err)
		}
	}
	return nil
}

// waitForRound blocks until `n` end-file events with reason "eof" have
// arrived (i.e., the queued playlist drained naturally), or the user
// closed mpv (reason "quit"), or ctx is canceled.
func waitForRound(ctx context.Context, player *mpv.Client, n int) error {
	eofs := 0
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-player.Done():
			return errors.New("mpv exited unexpectedly")
		case ev, ok := <-player.Events():
			if !ok {
				return errors.New("mpv closed events channel")
			}
			if ev.Name != "end-file" {
				continue
			}
			switch ev.Reason {
			case "eof":
				eofs++
				if eofs >= n {
					return nil
				}
			case "quit":
				return errors.New("mpv quit")
			case "stop", "error", "unknown", "redirect":
				// "stop" happens on a LoadReplace into the same player.
				// Don't count it toward the round.
			}
		}
	}
}

// ─── API client ────────────────────────────────────────────────────

type apiClient struct {
	baseURL string
	http    *http.Client
	token   *device.Token
}

func newAPIClient(baseURL string) *apiClient {
	return &apiClient{baseURL: baseURL, http: &http.Client{Timeout: 30 * time.Second}}
}

func (a *apiClient) ensureToken(ctx context.Context) error {
	host, _ := os.Hostname()
	info := device.RequesterInfo{
		WhereHappening: fmt.Sprintf("shows-client on %s", host),
		IntendedUse:    "play episodes via shows.romaine.life",
		MiscIdentifier: "couch",
	}
	tok, err := device.EnsureToken(ctx, authBaseURL(), info)
	if err != nil {
		return err
	}
	a.token = tok
	return nil
}

func (a *apiClient) do(ctx context.Context, method, path string, body any, out any) error {
	var bodyR io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return err
		}
		bodyR = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, a.baseURL+path, bodyR)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+a.token.Token)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := a.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 300 {
		return fmt.Errorf("%s %s: %d %s", method, path, resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if out != nil {
		if err := json.Unmarshal(raw, out); err != nil {
			return fmt.Errorf("decode: %w (body=%s)", err, raw)
		}
	}
	return nil
}

type roundEntry struct {
	ShowID       string `json:"show_id"`
	ShowName     string `json:"show_name"`
	EpisodeID    string `json:"episode_id"`
	AbsolutePath string `json:"absolute_path"`
	OrderValue   uint32 `json:"order_value"`
}

type roundResponse struct {
	Round []roundEntry `json:"round"`
}

func (a *apiClient) nextRound(ctx context.Context, playlist string) ([]roundEntry, error) {
	var resp roundResponse
	if err := a.do(ctx, http.MethodGet, "/api/playlists/"+playlist+"/next-round", nil, &resp); err != nil {
		return nil, err
	}
	return resp.Round, nil
}

type advanceEntry struct {
	ShowID    string `json:"show_id"`
	EpisodeID string `json:"episode_id"`
}

type advanceResponse struct {
	AdvancedCount int           `json:"advanced_count"`
	RemovedShows  []removedShow `json:"removed_shows"`
}

type removedShow struct {
	ID           string    `json:"id"`
	Name         string    `json:"name"`
	DateAdded    time.Time `json:"date_added"`
	LastPlayedAt time.Time `json:"last_played_at"`
}

func (a *apiClient) advance(ctx context.Context, playlist string, entries []advanceEntry) (advanceResponse, error) {
	var resp advanceResponse
	err := a.do(ctx, http.MethodPost, "/api/playlists/"+playlist+"/advance",
		map[string]any{"entries": entries}, &resp)
	return resp, err
}

// ─── config helpers ────────────────────────────────────────────────

func apiBaseURL() string {
	if v := os.Getenv(envAPIBaseURL); v != "" {
		return v
	}
	return defaultAPIBaseURL
}

func authBaseURL() string {
	if v := os.Getenv(envAuthBaseURL); v != "" {
		return v
	}
	return defaultAuthBaseURL
}
