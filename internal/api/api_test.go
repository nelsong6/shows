package api

import (
	"reflect"
	"testing"
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
