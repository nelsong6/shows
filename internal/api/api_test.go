package api

import (
	"net/http"
	"net/http/httptest"
	"reflect"
	"testing"

	"github.com/romaine-life/shows/internal/auth"
)

func TestParsePlaylists(t *testing.T) {
	cases := []struct {
		name string
		in   string
		want []string
	}{
		{"single", "nelson", []string{"nelson"}},
		{"comma separated", "a,b,c", []string{"a", "b", "c"}},
		{"trims surrounding space", " a , b ,c ", []string{"a", "b", "c"}},
		{"drops empty segments", "a,,b,", []string{"a", "b"}},
		{"empty string yields none", "", nil},
		{"only separators yields none", " , , ", nil},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := parsePlaylists(c.in)
			if !reflect.DeepEqual(got, c.want) {
				t.Fatalf("parsePlaylists(%q) = %#v, want %#v", c.in, got, c.want)
			}
		})
	}
}

// DeleteShowHistory is admin-only: a missing or non-admin caller is refused
// before the store is ever touched (so a nil Store is fine here).
func TestDeleteShowHistoryRequiresAdmin(t *testing.T) {
	s := &Server{}
	for _, role := range []string{"", "user", "service"} {
		req := httptest.NewRequest(http.MethodDelete, "/api/shows/x/history", nil)
		if role != "" {
			req = req.WithContext(auth.WithCaller(req.Context(), &auth.Caller{Role: role}))
		}
		rr := httptest.NewRecorder()
		s.handleDeleteShowHistory(rr, req)
		if rr.Code != http.StatusForbidden {
			t.Fatalf("role=%q: got %d, want 403", role, rr.Code)
		}
	}
}
