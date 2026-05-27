// Package apiclient is the shows-desktop side of the shows.romaine.life
// HTTP API. It mirrors the surface in cmd/shows-api/internal/api but
// types things from the client's perspective.
//
// Auth: a bearer JWT minted by auth.romaine.life. Set the Token field
// once after the oauth handshake; subsequent calls thread it through
// the Authorization header automatically.
package apiclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

const DefaultBaseURL = "https://shows.romaine.life"

type Client struct {
	BaseURL string
	Token   string
	HTTP    *http.Client
}

func New(baseURL, token string) *Client {
	if baseURL == "" {
		baseURL = DefaultBaseURL
	}
	return &Client{
		BaseURL: baseURL,
		Token:   token,
		HTTP:    &http.Client{Timeout: 30 * time.Second},
	}
}

// SetToken swaps the bearer credential mid-flight. Used when the
// token is refreshed because of expiry without recreating the client.
func (c *Client) SetToken(t string) { c.Token = t }

// ─── domain types — mirror internal/api/api.go in cmd/shows-api ─────

// RoundEntry is one episode in a /next-round response. AbsolutePath
// is the on-disk file the desktop's libmpv will load; OrderValue is
// the deterministic-shuffle key (uint32 from SHA-256 of the path)
// included for debugging and visibility, not enforcement.
type RoundEntry struct {
	ShowID       string `json:"show_id"`
	ShowName     string `json:"show_name"`
	EpisodeID    string `json:"episode_id"`
	AbsolutePath string `json:"absolute_path"`
	OrderValue   uint32 `json:"order_value"`
}

type roundResponse struct {
	Round []RoundEntry `json:"round"`
}

// AdvanceEntry identifies one episode the client just played. Both
// IDs are required because show_id is the partition key for the show
// doc in Cosmos; sending only episode_id would force a cross-partition
// scan server-side.
type AdvanceEntry struct {
	ShowID    string `json:"show_id"`
	EpisodeID string `json:"episode_id"`
}

type advanceRequest struct {
	Entries []AdvanceEntry `json:"entries"`
}

// RemovedShow is included on /advance when a show's queue emptied —
// the desktop can render the "this show took N days to watch" reveal
// without a follow-up history fetch.
type RemovedShow struct {
	ID           string    `json:"id"`
	Name         string    `json:"name"`
	DateAdded    time.Time `json:"date_added"`
	LastPlayedAt time.Time `json:"last_played_at"`
}

type AdvanceResult struct {
	AdvancedCount int           `json:"advanced_count"`
	RemovedShows  []RemovedShow `json:"removed_shows"`
}

// ─── operations ─────────────────────────────────────────────────────

// NextRound fetches the next batch of episodes to play for the given
// playlist. Returns an empty slice when every show in the playlist
// has been drained; callers should treat that as "nothing to play."
func (c *Client) NextRound(ctx context.Context, playlist string) ([]RoundEntry, error) {
	var resp roundResponse
	if err := c.do(ctx, http.MethodGet, "/api/playlists/"+playlist+"/next-round", nil, &resp); err != nil {
		return nil, err
	}
	return resp.Round, nil
}

// Advance reports a batch of just-played episodes. Server marks them
// watched, tombstones any shows whose queue emptied, appends rows to
// the watch_history container.
func (c *Client) Advance(ctx context.Context, playlist string, entries []AdvanceEntry) (*AdvanceResult, error) {
	if len(entries) == 0 {
		return &AdvanceResult{}, nil
	}
	var resp AdvanceResult
	if err := c.do(ctx, http.MethodPost, "/api/playlists/"+playlist+"/advance",
		advanceRequest{Entries: entries}, &resp); err != nil {
		return nil, err
	}
	return &resp, nil
}

// ─── transport ──────────────────────────────────────────────────────

func (c *Client) do(ctx context.Context, method, path string, body any, out any) error {
	if c.Token == "" {
		return errors.New("apiclient: token not set")
	}
	var br io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return err
		}
		br = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.BaseURL+path, br)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.Token)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.HTTP.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 300 {
		return fmt.Errorf("apiclient: %s %s: %d %s", method, path, resp.StatusCode, strings.TrimSpace(string(raw)))
	}
	if out != nil {
		if err := json.Unmarshal(raw, out); err != nil {
			return fmt.Errorf("apiclient: decode %s: %w (body=%s)", path, err, raw)
		}
	}
	return nil
}
