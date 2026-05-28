// shows-migrate imports the legacy play_show JSON layout into the shows API.
//
// Usage:
//
//	shows-migrate                 # reads D:\Downloads\Group-Nelson\nelson.json
//	shows-migrate --dry-run       # parse but don't POST
//	shows-migrate --ordered-playlist <path> --playlist <name>
//
// The legacy "ordered playlist" file is a JSON array of absolute paths to
// per-show JSON files. Each per-show JSON has:
//
//	{
//	  "Name": "Dr. Katz",
//	  "Episodes": ["Dr. Katz S06\\Dr.Katz.S06E11.Big.TV.avi", ...],
//	  "DateAdded": "1/29/2024 8:34:00 AM"
//	}
//
// We resolve the show's root_path as the parent directory of the per-show
// JSON, preserve the Episodes array order as the queue position, and parse
// DateAdded as US locale m/d/yyyy h:mm:ss tt.
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
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/nelsong6/shows/internal/device"
)

const (
	defaultOrdered     = `D:\Downloads\Group-Nelson\nelson.json`
	defaultAPIBaseURL  = "https://shows.romaine.life"
	defaultAuthBaseURL = "https://auth.romaine.life"
)

func main() {
	if err := run(); err != nil && !errors.Is(err, context.Canceled) {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	orderedPath := flag.String("ordered-playlist", defaultOrdered, "path to the legacy ordered playlist (nelson.json)")
	playlistName := flag.String("playlist", "nelson", "target playlist name in shows API")
	apiURL := flag.String("api-url", envOr("SHOWS_API_URL", defaultAPIBaseURL), "shows API base URL")
	authURL := flag.String("auth-url", envOr("SHOWS_AUTH_URL", defaultAuthBaseURL), "auth.romaine.life base URL")
	dryRun := flag.Bool("dry-run", false, "parse and summarize but don't POST")
	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	shows, err := loadLegacyShows(*orderedPath, *playlistName)
	if err != nil {
		return err
	}
	summarize(shows)

	if *dryRun {
		fmt.Fprintln(os.Stderr, "(dry-run, no requests sent)")
		return nil
	}

	// SHOWS_TOKEN lets a caller inject an already-minted JWT (e.g. the
	// user token the desktop app caches at %APPDATA%\shows\token.json)
	// and skip the bot-token device flow. The migrate endpoint accepts
	// any role in {admin, user}, so a human's own token works — no
	// browser approval, no separate bot identity. Falls back to the
	// device flow when unset.
	var token string
	if t := strings.TrimSpace(os.Getenv("SHOWS_TOKEN")); t != "" {
		fmt.Fprintln(os.Stderr, "using SHOWS_TOKEN from environment (skipping device flow)")
		token = t
	} else {
		host, _ := os.Hostname()
		tok, err := device.EnsureToken(ctx, *authURL, device.RequesterInfo{
			WhereHappening: fmt.Sprintf("shows-migrate on %s", host),
			IntendedUse:    "import legacy play_show JSON state",
			MiscIdentifier: "almanac",
		})
		if err != nil {
			return fmt.Errorf("auth: %w", err)
		}
		token = tok.Token
	}

	res, err := postMigrate(ctx, *apiURL, token, *playlistName, shows)
	if err != nil {
		return err
	}
	reportResults(res)
	return nil
}

// ─── legacy parsing ────────────────────────────────────────────────

type psShow struct {
	Name      string   `json:"Name"`
	Episodes  []string `json:"Episodes"`
	DateAdded string   `json:"DateAdded"`
}

type apiShow struct {
	Playlist  string    `json:"playlist"`
	Name      string    `json:"name"`
	RootPath  string    `json:"root_path"`
	DateAdded time.Time `json:"date_added"`
	Episodes  []string  `json:"episodes"`
}

func loadLegacyShows(orderedPath, playlistName string) ([]apiShow, error) {
	raw, err := os.ReadFile(orderedPath)
	if err != nil {
		return nil, fmt.Errorf("read ordered playlist: %w", err)
	}
	var perShowPaths []string
	if err := json.Unmarshal(raw, &perShowPaths); err != nil {
		return nil, fmt.Errorf("parse ordered playlist: %w", err)
	}

	out := make([]apiShow, 0, len(perShowPaths))
	for _, p := range perShowPaths {
		shRaw, err := os.ReadFile(p)
		if err != nil {
			return nil, fmt.Errorf("read %s: %w", p, err)
		}
		var ps psShow
		if err := json.Unmarshal(shRaw, &ps); err != nil {
			return nil, fmt.Errorf("parse %s: %w", p, err)
		}
		if ps.Name == "" {
			return nil, fmt.Errorf("%s: missing Name", p)
		}
		if len(ps.Episodes) == 0 {
			fmt.Fprintf(os.Stderr, "warning: %s has no episodes — skipping\n", p)
			continue
		}
		dt, err := parseLegacyDate(ps.DateAdded)
		if err != nil {
			return nil, fmt.Errorf("%s DateAdded: %w", p, err)
		}
		out = append(out, apiShow{
			Playlist:  playlistName,
			Name:      ps.Name,
			RootPath:  filepath.Dir(p),
			DateAdded: dt,
			Episodes:  ps.Episodes,
		})
	}
	return out, nil
}

// parseLegacyDate parses the US-locale timestamp format PowerShell's
// `[datetime]::Now.ToString()` writes: "M/d/yyyy h:mm:ss tt". We accept
// a few near-variants because the legacy files have drifted over the
// years (some have leading zeros, some used the invariant culture).
func parseLegacyDate(s string) (time.Time, error) {
	s = strings.TrimSpace(s)
	formats := []string{
		"1/2/2006 3:04:05 PM",
		"01/02/2006 3:04:05 PM",
		"1/2/2006 03:04:05 PM",
		"01/02/2006 03:04:05 PM",
		time.RFC3339,
	}
	for _, f := range formats {
		// time.Parse defaults to UTC when no zone is present. The
		// legacy timestamps were local-time stamped by PowerShell with
		// no zone, so UTC vs. local is a wash for "days elapsed"
		// arithmetic — never wrong by more than ~1 day. Good enough.
		if t, err := time.Parse(f, s); err == nil {
			return t, nil
		}
	}
	return time.Time{}, fmt.Errorf("could not parse %q (tried %d formats)", s, len(formats))
}

func summarize(shows []apiShow) {
	fmt.Fprintf(os.Stderr, "\nWill import %d show(s):\n", len(shows))
	for _, sh := range shows {
		fmt.Fprintf(os.Stderr, "  %-30s  %4d eps  added %s\n",
			truncate(sh.Name, 30), len(sh.Episodes), sh.DateAdded.Format("2006-01-02"))
	}
	fmt.Fprintln(os.Stderr, "")
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n-1] + "~"
}

// ─── API post ──────────────────────────────────────────────────────

type migrateRequest struct {
	Playlist string    `json:"playlist"`
	Shows    []apiShow `json:"shows"`
}

type migrateResult struct {
	Name  string `json:"name"`
	ID    string `json:"id,omitempty"`
	Error string `json:"error,omitempty"`
}

type migrateResponse struct {
	Results []migrateResult `json:"results"`
}

func postMigrate(ctx context.Context, apiURL, token, playlist string, shows []apiShow) (*migrateResponse, error) {
	body, _ := json.Marshal(migrateRequest{Playlist: playlist, Shows: shows})
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, apiURL+"/api/migrate/from-json", bytes.NewReader(body))
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")

	client := &http.Client{Timeout: 60 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 300 {
		return nil, fmt.Errorf("migrate: %d %s", resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	var out migrateResponse
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("decode: %w", err)
	}
	return &out, nil
}

func reportResults(res *migrateResponse) {
	ok := 0
	fmt.Fprintln(os.Stderr, "")
	for _, r := range res.Results {
		if r.Error != "" {
			fmt.Fprintf(os.Stderr, "  [FAIL] %-30s  %s\n", truncate(r.Name, 30), r.Error)
			continue
		}
		ok++
		fmt.Fprintf(os.Stderr, "  [ok]   %-30s  id=%s\n", truncate(r.Name, 30), r.ID)
	}
	fmt.Fprintf(os.Stderr, "\n%d imported, %d failed\n", ok, len(res.Results)-ok)
}

func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}
