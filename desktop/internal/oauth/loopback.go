// Package oauth runs the auth.romaine.life user-login flow for this
// native desktop app: the user signs in to the browser with
// Microsoft/Google (their normal romaine.life sign-in), and a one-time
// authorization code comes back to a loopback HTTP listener. The desktop
// then exchanges the code for the user's own JWT — never an admin-
// approved bot token, never the where_happening/intended_use ceremony.
//
// Flow (RFC 8252 — OAuth 2.0 for Native Apps):
//
//  1. Generate a PKCE verifier/challenge (S256) and a CSRF state value.
//  2. Bind a loopback listener at 127.0.0.1:<ephemeral>/callback.
//  3. Open the user's browser at
//     GET /api/auth/cli/user-login?
//     redirect_uri=http://127.0.0.1:PORT/callback
//     &state=...&code_challenge=...&code_challenge_method=S256
//     The server checks for a session cookie. If none, it bounces the
//     user through Microsoft/Google sign-in and returns here. Once
//     signed in, it redirects to the loopback URL with `?code=...`.
//  4. POST the code + code_verifier + redirect_uri to
//     /api/auth/cli/user-token (grant_type=authorization_code)
//     and receive the user's JWT in the response body.
//
// The JWT never travels through the browser. The desktop owns the
// loopback listener; the server validates that redirect_uri is loopback
// with an explicit port; PKCE binds the redemption to this specific
// flow so a leaked code is useless without the verifier still in memory.
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
	"net/url"
	"os"
	"path/filepath"
	"time"
)

const (
	DefaultAuthBaseURL = "https://auth.romaine.life"

	loginEndpoint = "/api/auth/cli/user-login"
	tokenEndpoint = "/api/auth/cli/user-token"

	// cacheVersion identifies the auth-flow generation that minted a
	// cached token. Bump it whenever the desktop switches auth flows so
	// that LoadCachedToken refuses tokens from a previous generation and
	// the next launch re-auths through the new flow — without that, an
	// unexpired bot-token from the old /api/cli/token flow would happily
	// satisfy EnsureToken's expiry check on a binary that's since been
	// rebuilt against /api/auth/cli/user-token.
	//
	// v1 = user-login JWTs from /api/auth/cli/user-token (current).
	cacheVersion = 1
)

// Token is the cached user JWT. Persisted to disk between launches at
// %APPDATA%\shows\token.json so the user only sees the browser once and
// then again ~daily when the JWT expires. The Version field gates cross-
// generation reuse — see cacheVersion above.
type Token struct {
	Version   int    `json:"version"`
	Token     string `json:"token"`
	ExpiresAt int64  `json:"expires_at"`
}

// Expired returns true when the cached token is close enough to expiry
// that a refresh should kick off. 60s safety margin so the new token is
// in hand before requests start failing at the API gate.
func (t *Token) Expired() bool {
	if t == nil || t.Token == "" {
		return true
	}
	return time.Now().Unix()+60 >= t.ExpiresAt
}

// Config drives a single Authenticate call.
type Config struct {
	AuthBaseURL string

	// Opener is called once with the sign-in URL the user must visit.
	// Typically wired to wails runtime.BrowserOpenURL so the browser
	// opens via the platform's preferred mechanism. If nil, the URL is
	// printed to stderr and the user opens it themselves.
	Opener func(url string) error
}

// Authenticate runs the full sign-in flow and returns the minted Token.
// Blocks until the user finishes signing in or the code TTL expires
// (~5 minutes per auth.romaine.life's user-login store).
func Authenticate(ctx context.Context, cfg Config) (*Token, error) {
	if cfg.AuthBaseURL == "" {
		cfg.AuthBaseURL = DefaultAuthBaseURL
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
	redirectURI := fmt.Sprintf("http://127.0.0.1:%d/callback", port)

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
			_, _ = w.Write([]byte(`<!doctype html><html><head><title>shows: signed in</title></head>
<body style="background:#0a0a0a;color:#eee;font-family:monospace;padding:32px;">
<h2 style="text-transform:uppercase;letter-spacing:0.05em;color:#888;">shows: signed in</h2>
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
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 1*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()

	loginURL, err := buildLoginURL(cfg.AuthBaseURL, redirectURI, challenge, state)
	if err != nil {
		return nil, err
	}
	if cfg.Opener != nil {
		_ = cfg.Opener(loginURL)
	} else {
		fmt.Fprintln(os.Stderr, "oauth: sign in at "+loginURL)
	}

	// 10-minute ceiling matches the server's code TTL window plus headroom
	// for the Microsoft/Google round-trip.
	var code string
	select {
	case code = <-codeCh:
	case err := <-errCh:
		return nil, err
	case <-time.After(10 * time.Minute):
		return nil, errors.New("oauth: code never arrived (sign-in window timed out)")
	case <-ctx.Done():
		return nil, ctx.Err()
	}

	return exchangeCode(ctx, cfg.AuthBaseURL, code, verifier, redirectURI)
}

// ─── server interactions ────────────────────────────────────────────

func buildLoginURL(baseURL, redirectURI, challenge, state string) (string, error) {
	u, err := url.Parse(baseURL + loginEndpoint)
	if err != nil {
		return "", fmt.Errorf("oauth: build login URL: %w", err)
	}
	q := u.Query()
	q.Set("redirect_uri", redirectURI)
	q.Set("code_challenge", challenge)
	q.Set("code_challenge_method", "S256")
	q.Set("state", state)
	u.RawQuery = q.Encode()
	return u.String(), nil
}

type tokenResponse struct {
	Token            string `json:"token"`
	ExpiresAt        int64  `json:"expires_at"`
	Error            string `json:"error"`
	ErrorDescription string `json:"error_description"`
}

func exchangeCode(ctx context.Context, baseURL, code, verifier, redirectURI string) (*Token, error) {
	body, _ := json.Marshal(map[string]string{
		"grant_type":    "authorization_code",
		"code":          code,
		"code_verifier": verifier,
		"redirect_uri":  redirectURI,
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
		return &Token{Version: cacheVersion, Token: tr.Token, ExpiresAt: tr.ExpiresAt}, nil
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

// CachePath is %APPDATA%\shows\token.json on Windows.
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
		// A corrupt cache file shouldn't make the app un-launchable —
		// treat it the same as no cache, force a fresh sign-in. The
		// next SaveToken overwrites it cleanly.
		return nil, nil
	}
	// Cross-generation guard: a token minted by an older auth flow
	// (e.g. the previous /api/cli/token bot-token path) is structurally
	// indistinguishable from a current-flow token at the JSON level —
	// both have {token, expires_at}. Without this gate, a yesterday-
	// minted bot token would silently satisfy EnsureToken's expiry check
	// on today's binary, suppressing the new sign-in flow until the bot
	// token happens to age out. Bump cacheVersion to retire the old
	// shape on the next launch.
	if t.Version != cacheVersion {
		return nil, nil
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

// EnsureToken returns the cached token if it's still valid, otherwise
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
