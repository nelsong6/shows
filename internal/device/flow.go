// Package device implements the auth.romaine.life CLI device flow.
//
// Endpoints we hit (defined in nelsong6/auth/src/server.ts):
//
//	POST /api/cli/device  → { device_code, user_code, verification_uri,
//	                          verification_uri_complete, expires_in, interval }
//	POST /api/cli/token   → { token, expires_at, expires_in_hours, purpose }
//	                       or { error: "authorization_pending"|"access_denied"|... }
//
// The local mpv-driving client embeds this package to obtain and cache a
// bot token. The cmd/shows-migrate one-shot tool uses it too.
package device

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"time"
)

const (
	DefaultAuthBaseURL = "https://auth.romaine.life"

	deviceEndpoint = "/api/cli/device"
	tokenEndpoint  = "/api/cli/token"

	deviceCodeGrantType = "urn:ietf:params:oauth:grant-type:device_code"
)

// RequesterInfo identifies who is making this token request to the admin
// who approves it. Required by the upstream device endpoint:
// where_happening, intended_use, misc_identifier are all mandatory.
// (See cli-device-flow.ts::requireRequesterInfo.)
type RequesterInfo struct {
	WhereHappening string `json:"where_happening"`
	IntendedUse    string `json:"intended_use"`
	MiscIdentifier string `json:"misc_identifier"`
}

// Token is the cached bot token. expires_at is a unix second timestamp
// produced by the auth.romaine.life mint site.
type Token struct {
	Token     string `json:"token"`
	ExpiresAt int64  `json:"expires_at"`
	Purpose   string `json:"purpose"`
}

func (t *Token) Expired() bool {
	if t == nil || t.Token == "" {
		return true
	}
	// 60s safety margin so a refresh kicks off before requests start
	// failing with expired-token errors on the API side.
	return time.Now().Unix()+60 >= t.ExpiresAt
}

// Client drives the device flow against a single auth.romaine.life base
// URL. Reuse across calls — http.Client connection pooling matters when
// we're polling every 5s.
type Client struct {
	BaseURL string
	HTTP    *http.Client
}

func NewClient(baseURL string) *Client {
	if baseURL == "" {
		baseURL = DefaultAuthBaseURL
	}
	return &Client{
		BaseURL: baseURL,
		HTTP:    &http.Client{Timeout: 30 * time.Second},
	}
}

// deviceResponse mirrors the auth.romaine.life /api/cli/device 200 body.
type deviceResponse struct {
	DeviceCode              string `json:"device_code"`
	UserCode                string `json:"user_code"`
	VerificationURI         string `json:"verification_uri"`
	VerificationURIComplete string `json:"verification_uri_complete"`
	ExpiresIn               int    `json:"expires_in"`
	Interval                int    `json:"interval"`
}

// tokenResponse handles both the success and error shapes of /api/cli/token.
type tokenResponse struct {
	Token            string `json:"token"`
	ExpiresAt        int64  `json:"expires_at"`
	ExpiresInHours   int    `json:"expires_in_hours"`
	Purpose          string `json:"purpose"`
	Error            string `json:"error"`
	ErrorDescription string `json:"error_description"`
	Interval         int    `json:"interval"`
}

// Authorize runs the full device flow synchronously. On success it returns
// the minted Token. On failure (denied, expired, network error, ctx
// cancel) it returns a non-nil error.
//
// userPrompt is called once with the human-readable details (user_code +
// verification_uri) so the caller can decide how to surface them: the
// local client prints them to stderr AND opens the browser; an automated
// migrator might just log.
func (c *Client) Authorize(ctx context.Context, info RequesterInfo, userPrompt func(userCode, verificationURI string)) (*Token, error) {
	if info.WhereHappening == "" || info.IntendedUse == "" || info.MiscIdentifier == "" {
		return nil, errors.New("device: requester info is required (where_happening, intended_use, misc_identifier)")
	}

	dev, err := c.requestDevice(ctx, info)
	if err != nil {
		return nil, err
	}

	if userPrompt != nil {
		uri := dev.VerificationURIComplete
		if uri == "" {
			uri = dev.VerificationURI
		}
		userPrompt(dev.UserCode, uri)
	}

	interval := dev.Interval
	if interval <= 0 {
		interval = 5
	}
	deadline := time.Now().Add(time.Duration(dev.ExpiresIn) * time.Second)

	for {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(time.Duration(interval) * time.Second):
		}
		if time.Now().After(deadline) {
			return nil, errors.New("device: code expired before approval")
		}

		tok, retry, err := c.poll(ctx, dev.DeviceCode)
		if err != nil {
			return nil, err
		}
		if retry > 0 {
			interval = retry
			continue
		}
		if tok != nil {
			return tok, nil
		}
	}
}

func (c *Client) requestDevice(ctx context.Context, info RequesterInfo) (*deviceResponse, error) {
	body, _ := json.Marshal(info)
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, c.BaseURL+deviceEndpoint, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.HTTP.Do(req)
	if err != nil {
		return nil, fmt.Errorf("device request: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("device request: %d %s", resp.StatusCode, bytes.TrimSpace(raw))
	}
	var dev deviceResponse
	if err := json.Unmarshal(raw, &dev); err != nil {
		return nil, fmt.Errorf("device request: parse: %w", err)
	}
	if dev.DeviceCode == "" || dev.UserCode == "" {
		return nil, fmt.Errorf("device request: bad payload: %s", raw)
	}
	return &dev, nil
}

// poll attempts one token exchange. Returns (token, 0, nil) on success,
// (nil, newInterval, nil) when still pending, (nil, 0, err) on terminal
// failure. The newInterval lets the server slow us down with the standard
// "slow_down" oauth error response.
func (c *Client) poll(ctx context.Context, deviceCode string) (*Token, int, error) {
	body, _ := json.Marshal(map[string]string{
		"grant_type":  deviceCodeGrantType,
		"device_code": deviceCode,
	})
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, c.BaseURL+tokenEndpoint, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.HTTP.Do(req)
	if err != nil {
		return nil, 0, fmt.Errorf("token poll: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)

	var tr tokenResponse
	if err := json.Unmarshal(raw, &tr); err != nil {
		return nil, 0, fmt.Errorf("token poll: parse: %w (body=%s)", err, raw)
	}

	if tr.Token != "" {
		return &Token{Token: tr.Token, ExpiresAt: tr.ExpiresAt, Purpose: tr.Purpose}, 0, nil
	}

	switch tr.Error {
	case "authorization_pending":
		if tr.Interval > 0 {
			return nil, tr.Interval, nil
		}
		return nil, 5, nil
	case "slow_down":
		if tr.Interval > 0 {
			return nil, tr.Interval, nil
		}
		return nil, 10, nil
	case "access_denied":
		return nil, 0, errors.New("device: approval denied")
	case "expired_token":
		return nil, 0, errors.New("device: code expired before approval")
	case "invalid_grant":
		return nil, 0, errors.New("device: invalid_grant")
	default:
		if tr.Error != "" {
			return nil, 0, fmt.Errorf("device: %s%s", tr.Error, descSuffix(tr.ErrorDescription))
		}
		return nil, 0, fmt.Errorf("device: unexpected response: %s", raw)
	}
}

func descSuffix(s string) string {
	if s == "" {
		return ""
	}
	return ": " + s
}

// ── token cache on disk ──────────────────────────────────────────────

// CachePath returns the location where the bot token is persisted across
// runs. On Windows this is %APPDATA%\shows\token.json; on macOS/Linux
// it's $XDG_CONFIG_HOME/shows/token.json (or ~/.config/shows/token.json).
func CachePath() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "shows", "token.json"), nil
}

func LoadCachedToken() (*Token, error) {
	path, err := CachePath()
	if err != nil {
		return nil, err
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}
		return nil, err
	}
	var t Token
	if err := json.Unmarshal(raw, &t); err != nil {
		return nil, fmt.Errorf("decode cached token: %w", err)
	}
	return &t, nil
}

func SaveToken(t *Token) error {
	path, err := CachePath()
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	raw, err := json.MarshalIndent(t, "", "  ")
	if err != nil {
		return err
	}
	// 0600 is the right mode on Linux/macOS; Windows ignores it but the
	// %APPDATA% directory is already user-scoped.
	return os.WriteFile(path, raw, 0o600)
}

// EnsureToken returns a cached token if it's still valid, otherwise runs
// the full device flow and persists the result. The default user prompt
// prints the user_code + verification URI to stderr and tries to open
// the URL in the default browser.
func EnsureToken(ctx context.Context, baseURL string, info RequesterInfo) (*Token, error) {
	cached, err := LoadCachedToken()
	if err != nil {
		return nil, err
	}
	if cached != nil && !cached.Expired() {
		return cached, nil
	}

	client := NewClient(baseURL)
	tok, err := client.Authorize(ctx, info, defaultPrompt)
	if err != nil {
		return nil, err
	}
	if err := SaveToken(tok); err != nil {
		return nil, fmt.Errorf("save token: %w", err)
	}
	return tok, nil
}

func defaultPrompt(userCode, verificationURI string) {
	fmt.Fprintln(os.Stderr, "")
	fmt.Fprintln(os.Stderr, "Approve this token at:")
	fmt.Fprintln(os.Stderr, "  "+verificationURI)
	fmt.Fprintln(os.Stderr, "Code: "+userCode)
	fmt.Fprintln(os.Stderr, "")
	if err := openBrowser(verificationURI); err != nil {
		fmt.Fprintf(os.Stderr, "(could not auto-open browser: %v)\n", err)
	}
}

func openBrowser(url string) error {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "windows":
		// `start` is a cmd builtin; `rundll32 url.dll,FileProtocolHandler`
		// is the direct equivalent that doesn't need a shell. Use the
		// latter so we don't have to escape URL chars for cmd.
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	case "darwin":
		cmd = exec.Command("open", url)
	default:
		cmd = exec.Command("xdg-open", url)
	}
	return cmd.Start()
}
