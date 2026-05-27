package auth

import (
	"context"
	"net/http"
	"strings"
)

type ctxKey struct{}

// WithCaller stores a verified Caller on the request context.
func WithCaller(ctx context.Context, c *Caller) context.Context {
	return context.WithValue(ctx, ctxKey{}, c)
}

// CallerFromContext returns the verified Caller for the request, if any.
// Returns nil when no Authorization header was presented or verification
// failed (the middleware short-circuits 401 in that case, so handlers
// downstream only ever see a non-nil caller).
func CallerFromContext(ctx context.Context) *Caller {
	v, _ := ctx.Value(ctxKey{}).(*Caller)
	return v
}

// Middleware verifies the Authorization: Bearer <jwt> header on every
// request. Failure modes:
//
//   - No Authorization header → 401
//   - Malformed header → 401
//   - Token verification failure → 401
//
// On success the verified Caller is stashed on the request context for
// handlers to read via CallerFromContext.
func Middleware(v *Verifier) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			auth := r.Header.Get("Authorization")
			if auth == "" {
				http.Error(w, "missing authorization", http.StatusUnauthorized)
				return
			}
			parts := strings.SplitN(auth, " ", 2)
			if len(parts) != 2 || !strings.EqualFold(parts[0], "Bearer") || parts[1] == "" {
				http.Error(w, "malformed authorization", http.StatusUnauthorized)
				return
			}
			caller, err := v.Verify(parts[1])
			if err != nil {
				http.Error(w, "invalid token", http.StatusUnauthorized)
				return
			}
			next.ServeHTTP(w, r.WithContext(WithCaller(r.Context(), caller)))
		})
	}
}
