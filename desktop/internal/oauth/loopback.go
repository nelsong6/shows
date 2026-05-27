// Package oauth runs the auth.romaine.life CLI device flow in its
// PKCE+loopback variant: spins up a localhost HTTP listener, sends
// the user's browser to the approval page with a redirect_uri pointing
// at the listener, and catches the auth code on redirect — no polling,
// no user_code-display dance.
//
// auth.romaine.life's /api/cli/device endpoint accepts both:
//   - device-code grant (no redirect_uri; client polls /api/cli/token)
//   - authorization-code grant with loopback redirect_uri + PKCE
//
// We use the second flow because shows-desktop is a real Windows app
// with a real browser — there's no reason to ask the user to read a
// VK-XXXX-XXXX code off a console window. See cli-device-flow.ts in
// nelsong6/auth for the server-side contract.
package oauth

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"
)

const (
	DefaultAuthBaseURL = "https://auth.romaine.life"

	deviceEndpoint = "/api/cli/device"
	tokenEndpoint  = "/api/cli/token"
)

// Token is the cached bot token. Persisted to disk between launches
// at %APPDATA%\shows\token.json so the user only sees the browser
// approval on the first launch (and again ~weekly when the JWT expires).
type Token struct {
	Token     string `json:"token"`
	ExpiresAt int64  `json:"expires_at"`
	Purpose   string `json:"purpose"`
}

// Expired returns true when the cached token is close enough to expiry
// that we should treat it as gone. 60s safety margin so a refresh
// kicks off before requests start failing with expired-token errors
// at the API gate.
func (t *Token) Expired() bool {
	if t == nil || t.Token == "" {
		return true
	}
	return time.Now().Unix()+60 >= t.ExpiresAt
}

// RequesterInfo is the human-facing description of who's asking for a
// token — presented on the approval page so the admin (you) can
// recognize the request. Required fields per cli-device-flow.ts.
type RequesterInfo struct {
	WhereHappening string
	IntendedUse    string
	MiscIdentifier string
}

// Config drives a single Authenticate call.
type Config struct {
	AuthBaseURL string
	Info        RequesterInfo

	// Opener is called once with the verification URL the user must
	// approve at. Typically wired to wails runtime.BrowserOpenURL so
	// the browser opens via the platform's preferred mechanism. If
	// nil, the URL is just printed to stderr and the user is expected
	// to open it themselves.
	Opener func(url string) error
}

// Authenticate runs the full PKCE+loopback flow and returns the
// minted Token. Blocks until the user approves or the device code
// expires (typically 10 minutes per the auth.romaine.life mint
// helpers).
func Authenticate(ctx context.Context, cfg Config) (*Token, error) {
	if cfg.AuthBaseURL == "" {
		cfg.AuthBaseURL = DefaultAuthBaseURL
	}
	if cfg.Info.WhereHappening == "" || cfg.Info.IntendedUse == "" || cfg.Info.MiscIdentifier == "" {
		return nil, errors.New("oauth: requester info required (where_happening, intended_use, misc_identifier)")
	}

	verifier, err := randomURLToken(32)
	if err != nil {
		return nil, fmt.Errorf("oauth: pkce verifier: %w", err)
	}
	challenge := pkceS256(verifier)

	state, err := randomURLToken(24)
	if err != nil {
		return nil, fmt.Errorf("oauth: state: %w", err)
	}

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fmt.Errorf("oauth: loopback listen: %w", err)
	}
	defer listener.Close()
	port := listener.Addr().(*net.TCPAddr).Port
	redirectURI := fmt.Sprintf("http://localhost:%d/callback", port)

	codeCh := make(chan string, 1)
	errCh := make(chan error, 1)

	srv := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path != "/callback" {
				http.NotFound(w, r)
				return
			}
			q := r.URL.Query()
			if q.Get("state") != state {
				w.WriteHeader(http.StatusBadRequest)
				_, _ = w.Write([]byte("state mismatch"))
				select {
				case errCh <- errors.New("oauth: state mismatch on callback"):
				default:
				}
				return
			}
			code := q.Get("code")
			if code == "" {
				w.WriteHeader(http.StatusBadRequest)
				_, _ = w.Write([]byte("missing code"))
				select {
				case errCh <- errors.New("oauth: callback missing code"):
				default:
				}
				return
			}
			w.Header().Set("Content-Type", "text/html; charset=utf-8")
			_, _ = w.Write([]byte(`<!doctype html><html><head><title>shows: authorized</title></head>
<body style="background:#0a0a0a;color:#eee;font-family:monospace;padding:32px;">
<h2 style="text-transform:uppercase;letter-spacing:0.05em;color:#888;">shows: authorized</h2>
<p>You can close this tab. The desktop app has your token.</p>
</body></html>`))
			select {
			case codeCh <- code:
			default:
			}
		}),
		ReadHeaderTimeout: 5 * time.Second,
	}
	go func() { _ = srv.Serve(listener) }()
	defer func() {
		ctx, cancel := context.WithTimeout(context.Background(), 1*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
	}()

	dev, err := requestDeviceCode(ctx, cfg.AuthBaseURL, cfg.Info, redirectURI, challenge, state)
	if err != nil {
		return nil, err
	}

	verifyURL := dev.VerificationURIComplete
	if verifyURL == "" {
		verifyURL = dev.VerificationURI
	}
	if cfg.Opener != nil {
		_ = cfg.Opener(verifyURL)
	} else {
		fmt.Fprintln(os.Stderr, "oauth: approve at "+verifyURL)
	}

	expiry := time.Duration(dev.ExpiresIn) * time.Second
	if expiry == 0 {
		expiry = 10 * time.Minute
	}

	var code string
	select {
	case code = <-codeCh:
	case err := <-errCh:
		return nil, err
	case <-time.After(expiry):
		return nil, errors.New("oauth: code expired before approval")
	case <-ctx.Done():
		return nil, ctx.Err()
	}

	return exchangeCode(ctx, cfg.AuthBaseURL, code, verifier)
}

// ─── server interactions ────────────────────────────────────────────

type deviceRequest struct {
	WhereHappening      string `json:"where_happening"`
	IntendedUse         string `json:"intended_use"`
	MiscIdentifier      string `json:"misc_identifier"`
	RedirectURI         string `json:"redirect_uri,omitempty"`
	CodeChallenge       string `json:"code_challenge,omitempty"`
	CodeChallengeMethod string `json:"code_challenge_method,omitempty"`
	State               string `json:"state,omitempty"`
}

type deviceResponse struct {
	DeviceCode              string `json:"device_code"`
	UserCode                string `json:"user_code"`
	VerificationURI         string `json:"verification_uri"`
	VerificationURIComplete string `json:"verification_uri_complete"`
	ExpiresIn               int    `json:"expires_in"`
	Interval                int    `json:"interval"`
}

func requestDeviceCode(ctx context.Context, baseURL string, info RequesterInfo, redirectURI, challenge, state string) (*deviceResponse, error) {
	body, _ := json.Marshal(deviceRequest{
		WhereHappening:      info.WhereHappening,
		IntendedUse:         info.IntendedUse,
		MiscIdentifier:      info.MiscIdentifier,
		RedirectURI:         redirectURI,
		CodeChallenge:       challenge,
		CodeChallengeMethod: "S256",
		State:               state,
	})
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+deviceEndpoint, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	resp, err := httpDo(req)
	if err != nil {
		return nil, fmt.Errorf("oauth: device request: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("oauth: device request: %d %s", resp.StatusCode, bytes.TrimSpace(raw))
	}
	var dev deviceResponse
	if err := json.Unmarshal(raw, &dev); err != nil {
		return nil, fmt.Errorf("oauth: device parse: %w", err)
	}
	if dev.DeviceCode == "" {
		return nil, fmt.Errorf("oauth: device response missing fields: %s", raw)
	}
	return &dev, nil
}

type tokenResponse struct {
	Token            string `json:"token"`
	ExpiresAt        int64  `json:"expires_at"`
	Purpose          string `json:"purpose"`
	Error            string `json:"error"`
	ErrorDescription string `json:"error_description"`
}

func exchangeCode(ctx context.Context, baseURL, code, verifier string) (*Token, error) {
	body, _ := json.Marshal(map[string]string{
		"grant_type":    "authorization_code",
		"code":          code,
		"code_verifier": verifier,
	})
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+tokenEndpoint, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	resp, err := httpDo(req)
	if err != nil {
		return nil, fmt.Errorf("oauth: token exchange: %w", err)
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	var tr tokenResponse
	if err := json.Unmarshal(raw, &tr); err != nil {
		return nil, fmt.Errorf("oauth: token parse: %w (body=%s)", err, raw)
	}
	if tr.Token != "" {
		return &Token{Token: tr.Token, ExpiresAt: tr.ExpiresAt, Purpose: tr.Purpose}, nil
	}
	if tr.Error != "" {
		desc := ""
		if tr.ErrorDescription != "" {
			desc = ": " + tr.ErrorDescription
		}
		return nil, fmt.Errorf("oauth: %s%s", tr.Error, desc)
	}
	return nil, fmt.Errorf("oauth: token exchange returned %d %s", resp.StatusCode, raw)
}

// ─── token cache ────────────────────────────────────────────────────

// CachePath is %APPDATA%\shows\token.json on Windows. Shared with the
// retired cmd/shows-client and the still-living cmd/shows-migrate so
// any of them can warm the cache for the others.
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
		return nil, fmt.Errorf("oauth: decode cached: %w", err)
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
	return os.WriteFile(path, raw, 0o600)
}

// EnsureToken returns a cached token if it's still valid, otherwise
// runs Authenticate and persists the result.
func EnsureToken(ctx context.Context, cfg Config) (*Token, error) {
	cached, err := LoadCachedToken()
	if err != nil {
		return nil, err
	}
	if cached != nil && !cached.Expired() {
		return cached, nil
	}
	tok, err := Authenticate(ctx, cfg)
	if err != nil {
		return nil, err
	}
	if err := SaveToken(tok); err != nil {
		return nil, fmt.Errorf("oauth: save token: %w", err)
	}
	return tok, nil
}

// ─── helpers ────────────────────────────────────────────────────────

func randomURLToken(nBytes int) (string, error) {
	b := make([]byte, nBytes)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.RawURLEncoding.EncodeToString(b), nil
}

func pkceS256(verifier string) string {
	sum := sha256.Sum256([]byte(verifier))
	return base64.RawURLEncoding.EncodeToString(sum[:])
}

var httpClient = &http.Client{Timeout: 30 * time.Second}

func httpDo(req *http.Request) (*http.Response, error) {
	return httpClient.Do(req)
}
