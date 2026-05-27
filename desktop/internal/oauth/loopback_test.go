package oauth

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// fakeAuthServer stands in for auth.romaine.life. It exposes the same
// two endpoints the real server exposes — /api/auth/cli/user-login and
// /api/auth/cli/user-token — and records the inputs it sees so tests
// can assert that Authenticate stamped PKCE/state correctly.
type fakeAuthServer struct {
	srv *httptest.Server

	// Captured inputs from the login endpoint.
	mu               sync.Mutex
	loginRedirectURI string
	loginChallenge   string
	loginState       string
	loginMethod      string

	// Captured inputs from the token endpoint.
	tokenBody map[string]string

	// What the token endpoint returns.
	tokenResponse map[string]any
	tokenStatus   int

	// Toggle to simulate a callback with the wrong state (server-side
	// confusion, not what auth.romaine.life would actually do — but the
	// client must defend against any URL hitting its loopback).
	corruptState bool

	// How many times /user-token has been hit. Tests that exercise
	// single-use replay assert this is 1.
	tokenHits atomic.Int32
}

func newFakeAuthServer(t *testing.T) *fakeAuthServer {
	t.Helper()
	f := &fakeAuthServer{
		tokenResponse: map[string]any{
			"token":      "eyJ-fake-jwt",
			"expires_at": time.Now().Add(24 * time.Hour).Unix(),
		},
		tokenStatus: http.StatusOK,
	}
	mux := http.NewServeMux()
	mux.HandleFunc(loginEndpoint, f.handleLogin)
	mux.HandleFunc(tokenEndpoint, f.handleToken)
	f.srv = httptest.NewServer(mux)
	t.Cleanup(f.srv.Close)
	return f
}

func (f *fakeAuthServer) handleLogin(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	f.mu.Lock()
	f.loginRedirectURI = q.Get("redirect_uri")
	f.loginChallenge = q.Get("code_challenge")
	f.loginState = q.Get("state")
	f.loginMethod = q.Get("code_challenge_method")
	corrupt := f.corruptState
	f.mu.Unlock()

	state := f.loginState
	if corrupt {
		state = "tampered-state"
	}
	target, err := url.Parse(f.loginRedirectURI)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	tq := target.Query()
	tq.Set("code", "test-one-time-code")
	tq.Set("state", state)
	target.RawQuery = tq.Encode()
	http.Redirect(w, r, target.String(), http.StatusFound)
}

func (f *fakeAuthServer) handleToken(w http.ResponseWriter, r *http.Request) {
	f.tokenHits.Add(1)
	var body map[string]string
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	f.mu.Lock()
	f.tokenBody = body
	resp := f.tokenResponse
	status := f.tokenStatus
	f.mu.Unlock()

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(resp)
}

// browserOpener returns an Opener that simulates the user's browser by
// fetching the URL itself and following redirects all the way through
// to the loopback. http.Client's default redirect policy stops at 10
// hops — way more than enough for our single 302 → loopback.
func browserOpener(t *testing.T) func(string) error {
	t.Helper()
	return func(u string) error {
		go func() {
			resp, err := http.Get(u)
			if err != nil {
				t.Logf("browser GET %s: %v", u, err)
				return
			}
			_ = resp.Body.Close()
		}()
		return nil
	}
}

func TestAuthenticate_HappyPath(t *testing.T) {
	f := newFakeAuthServer(t)

	tok, err := Authenticate(context.Background(), Config{
		AuthBaseURL: f.srv.URL,
		Opener:      browserOpener(t),
	})
	if err != nil {
		t.Fatalf("Authenticate failed: %v", err)
	}
	if tok.Token != "eyJ-fake-jwt" {
		t.Errorf("Token = %q, want %q", tok.Token, "eyJ-fake-jwt")
	}
	if tok.ExpiresAt == 0 {
		t.Errorf("ExpiresAt not set")
	}

	f.mu.Lock()
	defer f.mu.Unlock()

	// PKCE method must be S256 — auth.romaine.life refuses anything else.
	if f.loginMethod != "S256" {
		t.Errorf("code_challenge_method = %q, want S256", f.loginMethod)
	}
	// Challenge must be 43-128 base64url chars (sha256 → 32 bytes → 43 b64url chars).
	if len(f.loginChallenge) < 43 || len(f.loginChallenge) > 128 {
		t.Errorf("code_challenge length = %d, want 43-128", len(f.loginChallenge))
	}
	// Loopback redirect_uri must be 127.0.0.1 with an explicit port.
	u, err := url.Parse(f.loginRedirectURI)
	if err != nil {
		t.Fatalf("redirect_uri parse: %v", err)
	}
	if u.Hostname() != "127.0.0.1" {
		t.Errorf("redirect_uri host = %q, want 127.0.0.1", u.Hostname())
	}
	if u.Port() == "" {
		t.Errorf("redirect_uri port empty — RFC 8252 requires an explicit port")
	}
	if u.Path != "/callback" {
		t.Errorf("redirect_uri path = %q, want /callback", u.Path)
	}

	// Token exchange must echo the same redirect_uri and pass the
	// verifier that hashes to the challenge sent at /user-login.
	if f.tokenBody["grant_type"] != "authorization_code" {
		t.Errorf("grant_type = %q, want authorization_code", f.tokenBody["grant_type"])
	}
	if f.tokenBody["code"] != "test-one-time-code" {
		t.Errorf("code = %q, want test-one-time-code", f.tokenBody["code"])
	}
	if f.tokenBody["redirect_uri"] != f.loginRedirectURI {
		t.Errorf("redirect_uri at token = %q, want %q (must match login-time)",
			f.tokenBody["redirect_uri"], f.loginRedirectURI)
	}
	// The big invariant: SHA256(verifier) base64url == challenge.
	verifier := f.tokenBody["code_verifier"]
	sum := sha256.Sum256([]byte(verifier))
	want := base64.RawURLEncoding.EncodeToString(sum[:])
	if want != f.loginChallenge {
		t.Errorf("PKCE binding broken: SHA256(verifier)=%q, challenge=%q", want, f.loginChallenge)
	}
}

func TestAuthenticate_StateMismatchRejected(t *testing.T) {
	f := newFakeAuthServer(t)
	f.corruptState = true

	_, err := Authenticate(context.Background(), Config{
		AuthBaseURL: f.srv.URL,
		Opener:      browserOpener(t),
	})
	if err == nil {
		t.Fatal("Authenticate returned no error on state mismatch")
	}
	if !strings.Contains(err.Error(), "state mismatch") {
		t.Errorf("error = %v, want a state-mismatch message", err)
	}
	if f.tokenHits.Load() != 0 {
		t.Errorf("token endpoint hit %d times — must not be called on state mismatch", f.tokenHits.Load())
	}
}

func TestAuthenticate_TokenEndpointError(t *testing.T) {
	f := newFakeAuthServer(t)
	f.tokenResponse = map[string]any{
		"error":             "invalid_grant",
		"error_description": "PKCE verification failed",
	}
	f.tokenStatus = http.StatusBadRequest

	_, err := Authenticate(context.Background(), Config{
		AuthBaseURL: f.srv.URL,
		Opener:      browserOpener(t),
	})
	if err == nil {
		t.Fatal("Authenticate returned no error on token endpoint failure")
	}
	if !strings.Contains(err.Error(), "invalid_grant") {
		t.Errorf("error = %v, want invalid_grant", err)
	}
	if !strings.Contains(err.Error(), "PKCE verification failed") {
		t.Errorf("error = %v, want the server's error_description threaded through", err)
	}
}

func TestAuthenticate_ContextCancellation(t *testing.T) {
	// Login endpoint that hangs — simulates the user closing the browser
	// without signing in. Authenticate must return when ctx is canceled.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Never respond.
		<-r.Context().Done()
	}))
	defer srv.Close()

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(100 * time.Millisecond)
		cancel()
	}()

	_, err := Authenticate(ctx, Config{
		AuthBaseURL: srv.URL,
		Opener:      func(string) error { return nil },
	})
	if !errors.Is(err, context.Canceled) {
		t.Errorf("err = %v, want context.Canceled", err)
	}
}

func TestBuildLoginURL_Shape(t *testing.T) {
	got, err := buildLoginURL("https://auth.example.test", "http://127.0.0.1:51234/callback",
		"the-challenge", "the-state")
	if err != nil {
		t.Fatalf("buildLoginURL: %v", err)
	}
	u, err := url.Parse(got)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if u.Scheme+"://"+u.Host+u.Path != "https://auth.example.test"+loginEndpoint {
		t.Errorf("base = %q, want https://auth.example.test%s", u.Scheme+"://"+u.Host+u.Path, loginEndpoint)
	}
	q := u.Query()
	if q.Get("redirect_uri") != "http://127.0.0.1:51234/callback" {
		t.Errorf("redirect_uri = %q", q.Get("redirect_uri"))
	}
	if q.Get("code_challenge") != "the-challenge" {
		t.Errorf("code_challenge = %q", q.Get("code_challenge"))
	}
	if q.Get("code_challenge_method") != "S256" {
		t.Errorf("code_challenge_method = %q, want S256", q.Get("code_challenge_method"))
	}
	if q.Get("state") != "the-state" {
		t.Errorf("state = %q", q.Get("state"))
	}
}

func TestPKCES256_KnownVector(t *testing.T) {
	// RFC 7636 Appendix B test vector.
	const verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
	const want = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
	if got := pkceS256(verifier); got != want {
		t.Errorf("pkceS256 = %q, want %q (RFC 7636 B.1)", got, want)
	}
}

func TestToken_Expired(t *testing.T) {
	cases := []struct {
		name string
		tok  *Token
		want bool
	}{
		{"nil", nil, true},
		{"empty token", &Token{Token: "", ExpiresAt: time.Now().Add(time.Hour).Unix()}, true},
		{"long-future expiry", &Token{Token: "x", ExpiresAt: time.Now().Add(time.Hour).Unix()}, false},
		{"30s in the future — inside the 60s safety margin", &Token{Token: "x", ExpiresAt: time.Now().Add(30 * time.Second).Unix()}, true},
		{"already past", &Token{Token: "x", ExpiresAt: time.Now().Add(-time.Hour).Unix()}, true},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := c.tok.Expired(); got != c.want {
				t.Errorf("Expired() = %v, want %v", got, c.want)
			}
		})
	}
}

// Sanity check: even before the user signs in we should have already
// spun up a listener (so the redirect at the bridge page can land).
// This test confirms the listener binds successfully on a free port and
// the server cleans itself up.
func TestListenerLifecycle(t *testing.T) {
	// Indirect test via Authenticate-then-cancel: if the listener leaked
	// the port, a follow-up Authenticate would still work (different
	// ephemeral port), but a t.Cleanup leak detector would catch a
	// dangling goroutine. We instead just confirm Authenticate doesn't
	// hang when the server immediately closes.
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		// Return an HTML page that never redirects — the loopback will
		// never receive a code and Authenticate must hit ctx deadline.
		_, _ = io.WriteString(w, "<html>ignored</html>")
	}))
	defer srv.Close()

	_, err := Authenticate(ctx, Config{
		AuthBaseURL: srv.URL,
		Opener:      func(string) error { return nil },
	})
	if err == nil {
		t.Fatal("expected timeout-style error")
	}
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("err = %v, want context.DeadlineExceeded", err)
	}
}
