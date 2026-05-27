// Package auth verifies inbound JWTs minted by auth.romaine.life.
//
// Contract mirrors nelsong6/romaine-auth-py:
//
//   - RS256 signature against the JWKS at /api/auth/jwks
//   - Required claims: exp, iat, iss, role
//   - iss must equal the configured issuer (default https://auth.romaine.life)
//   - role must be in {admin, user, service}; pending users are refused
//   - role=service tokens must carry actor_email
//   - aud is intentionally NOT pinned — every auth.romaine.life token today
//     uses aud=<issuer> which gives no per-app isolation. Skip rather than
//     pin a value that conveys nothing.
//   - 60s clock-skew leeway on exp/iat
//
// JWKS keys are cached in process by the upstream `keyfunc` library and
// refreshed on key-not-found.
package auth

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/MicahParks/keyfunc/v3"
	"github.com/golang-jwt/jwt/v5"
)

const (
	DefaultIssuer  = "https://auth.romaine.life"
	DefaultJWKSURL = "https://auth.romaine.life/api/auth/jwks"

	leewaySeconds = 60
)

// AllowedRoles is the closed set of role claims accepted on inbound tokens.
// Pending users are deliberately excluded — they have authenticated but
// have not been promoted to a usable role.
var AllowedRoles = map[string]struct{}{
	"admin":   {},
	"user":    {},
	"service": {},
}

// Caller is the resolved identity from a verified JWT.
type Caller struct {
	Sub         string
	Email       string
	Name        string
	Role        string
	ActorEmail  string
	RawToken    string
}

func (c *Caller) IsService() bool { return c.Role == "service" }
func (c *Caller) IsAdmin() bool   { return c.Role == "admin" }
func (c *Caller) IsHuman() bool   { return c.Role == "admin" || c.Role == "user" }

// DisplayActor is the best-effort human identity for audit logging. Falls
// back to email when the caller is a human (no actor_email present).
func (c *Caller) DisplayActor() string {
	if c.ActorEmail != "" {
		return c.ActorEmail
	}
	return c.Email
}

// Verifier verifies tokens against a JWKS endpoint. Construct once and
// share across the process — keyfunc's JWKS cache is concurrency-safe.
type Verifier struct {
	issuer  string
	jwksURL string
	jwks    keyfunc.Keyfunc
}

// New builds a Verifier from explicit config. ctx governs the initial
// JWKS fetch.
func New(ctx context.Context, issuer, jwksURL string) (*Verifier, error) {
	if issuer == "" {
		return nil, errors.New("auth: empty issuer")
	}
	if jwksURL == "" {
		return nil, errors.New("auth: empty jwksURL")
	}
	jwks, err := keyfunc.NewDefaultCtx(ctx, []string{jwksURL})
	if err != nil {
		return nil, fmt.Errorf("auth: jwks fetch: %w", err)
	}
	return &Verifier{
		issuer:  issuer,
		jwksURL: jwksURL,
		jwks:    jwks,
	}, nil
}

// FromEnv builds a Verifier using the AUTH_ROMAINE_LIFE_ISSUER and
// AUTH_ROMAINE_LIFE_JWKS_URL environment variables, falling back to the
// public defaults.
func FromEnv(ctx context.Context) (*Verifier, error) {
	issuer := os.Getenv("AUTH_ROMAINE_LIFE_ISSUER")
	if issuer == "" {
		issuer = DefaultIssuer
	}
	jwksURL := os.Getenv("AUTH_ROMAINE_LIFE_JWKS_URL")
	if jwksURL == "" {
		jwksURL = DefaultJWKSURL
	}
	return New(ctx, issuer, jwksURL)
}

// Verify parses and verifies the token string. On any failure (bad
// signature, wrong issuer, expired, missing required claim, disallowed
// role) it returns a non-nil error and a nil Caller.
func (v *Verifier) Verify(tokenString string) (*Caller, error) {
	parser := jwt.NewParser(
		jwt.WithValidMethods([]string{"RS256"}),
		jwt.WithIssuer(v.issuer),
		jwt.WithLeeway(time.Duration(leewaySeconds)*time.Second),
		jwt.WithExpirationRequired(),
		jwt.WithIssuedAt(),
		// aud verification is intentionally not enabled — see package doc.
	)

	claims := jwt.MapClaims{}
	if _, err := parser.ParseWithClaims(tokenString, claims, v.jwks.Keyfunc); err != nil {
		return nil, fmt.Errorf("auth: parse/verify: %w", err)
	}

	// Required-claim contract. jwt.WithExpirationRequired covers exp;
	// WithIssuedAt covers iat. iss is covered by WithIssuer. role is ours.
	if _, ok := claims["role"]; !ok {
		return nil, errors.New("auth: missing required claim: role")
	}

	role := strings.TrimSpace(stringClaim(claims, "role"))
	if _, ok := AllowedRoles[role]; !ok {
		return nil, fmt.Errorf("auth: role not approved: %q", role)
	}

	email := strings.ToLower(strings.TrimSpace(stringClaim(claims, "email")))
	actorEmail := strings.ToLower(strings.TrimSpace(stringClaim(claims, "actor_email")))

	if role == "service" {
		if actorEmail == "" {
			return nil, errors.New("auth: service token missing actor_email")
		}
		if email == "" {
			// Service tokens routinely omit `email`. Backfill so
			// downstream consumers logging Email get a usable string.
			email = actorEmail
		}
	} else if email == "" {
		return nil, errors.New("auth: token missing email claim")
	}

	return &Caller{
		Sub:        stringClaim(claims, "sub"),
		Email:      email,
		Name:       stringClaim(claims, "name"),
		Role:       role,
		ActorEmail: actorEmail,
		RawToken:   tokenString,
	}, nil
}

func stringClaim(c jwt.MapClaims, key string) string {
	v, ok := c[key]
	if !ok || v == nil {
		return ""
	}
	s, ok := v.(string)
	if !ok {
		return ""
	}
	return s
}
