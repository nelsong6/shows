package api

import (
	"reflect"
	"testing"

	"github.com/nelsong6/shows/internal/ordering"
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

func TestRoundFromOrdered(t *testing.T) {
	ordered := []ordering.Ordered{
		{
			Candidate:    ordering.Candidate{EpisodeID: "e1", ShowID: "s1", RootPath: `D:\A`, RelativePath: `a.mkv`},
			AbsolutePath: `D:\A\a.mkv`,
			OrderValue:   0x1111,
		},
		{
			Candidate:    ordering.Candidate{EpisodeID: "e2", ShowID: "s2", RootPath: `D:\B`, RelativePath: `b.mkv`},
			AbsolutePath: `D:\B\b.mkv`,
			OrderValue:   0x2222,
		},
	}
	names := map[string]string{"e1": "Show One", "e2": "Show Two"}

	t.Run("cross-playlist carries name and playlist through", func(t *testing.T) {
		playlists := map[string]string{"e1": "nelson", "e2": "couple"}
		got := roundFromOrdered(ordered, names, playlists)
		want := []RoundEntry{
			{ShowID: "s1", ShowName: "Show One", EpisodeID: "e1", AbsolutePath: `D:\A\a.mkv`, OrderValue: 0x1111, Playlist: "nelson"},
			{ShowID: "s2", ShowName: "Show Two", EpisodeID: "e2", AbsolutePath: `D:\B\b.mkv`, OrderValue: 0x2222, Playlist: "couple"},
		}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("roundFromOrdered = %#v, want %#v", got, want)
		}
	})

	t.Run("single-playlist (nil map) leaves Playlist empty", func(t *testing.T) {
		got := roundFromOrdered(ordered, names, nil)
		for _, e := range got {
			if e.Playlist != "" {
				t.Fatalf("entry %s has Playlist=%q, want empty on the single-playlist path", e.EpisodeID, e.Playlist)
			}
		}
	})
}
