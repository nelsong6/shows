package ordering

import "testing"

// Known SHA-256 fixtures from public references. These let us catch any
// drift in the hash plumbing — wrong encoding, wrong algorithm, accidental
// re-hashing — without needing PowerShell to be installed to regenerate.
func TestOrderValue_KnownFixtures(t *testing.T) {
	cases := []struct {
		name  string
		input string
		// First 4 hex chars of SHA-256(UTF-8(input)), parsed as uint32.
		want uint32
	}{
		// SHA-256("") = e3b0c44298fc1c14...
		{"empty", "", 0xe3b0},
		// SHA-256("abc") = ba7816bf8f01cfea...
		{"abc", "abc", 0xba78},
		// SHA-256("hello") = 2cf24dba5fb0a30e...
		{"hello", "hello", 0x2cf2},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := OrderValue(c.input)
			if got != c.want {
				t.Fatalf("OrderValue(%q) = %#x, want %#x", c.input, got, c.want)
			}
		})
	}
}

func TestJoinPath(t *testing.T) {
	cases := []struct {
		root string
		rel  string
		want string
	}{
		{
			`D:\Downloads\Group-Nelson\Dr. Katz, Professional Therapist`,
			`Dr. Katz S06\Dr.Katz.S06E11.Big.TV.avi`,
			`D:\Downloads\Group-Nelson\Dr. Katz, Professional Therapist\Dr. Katz S06\Dr.Katz.S06E11.Big.TV.avi`,
		},
		{
			// Trailing backslash on root: must not produce a double sep.
			`D:\Foo\`,
			`bar\baz.mkv`,
			`D:\Foo\bar\baz.mkv`,
		},
		{
			// Leading backslash on rel: same.
			`D:\Foo`,
			`\bar\baz.mkv`,
			`D:\Foo\bar\baz.mkv`,
		},
	}
	for _, c := range cases {
		got := JoinPath(c.root, c.rel)
		if got != c.want {
			t.Errorf("JoinPath(%q, %q) = %q, want %q", c.root, c.rel, got, c.want)
		}
	}
}

func TestSort_Deterministic(t *testing.T) {
	in := []Candidate{
		{EpisodeID: 1, ShowID: 10, RootPath: `D:\A`, RelativePath: `a.mkv`},
		{EpisodeID: 2, ShowID: 20, RootPath: `D:\B`, RelativePath: `b.mkv`},
		{EpisodeID: 3, ShowID: 30, RootPath: `D:\C`, RelativePath: `c.mkv`},
		{EpisodeID: 4, ShowID: 40, RootPath: `D:\D`, RelativePath: `d.mkv`},
	}
	first := Sort(in)
	second := Sort(in)
	if len(first) != len(second) {
		t.Fatalf("length mismatch: %d vs %d", len(first), len(second))
	}
	for i := range first {
		if first[i].EpisodeID != second[i].EpisodeID {
			t.Fatalf("non-deterministic at idx %d: %d vs %d", i, first[i].EpisodeID, second[i].EpisodeID)
		}
	}
	for i := 1; i < len(first); i++ {
		if first[i-1].OrderValue > first[i].OrderValue {
			t.Fatalf("not sorted ascending: idx %d (%#x) > idx %d (%#x)",
				i-1, first[i-1].OrderValue, i, first[i].OrderValue)
		}
	}
}

func TestSort_TieBreakOnEpisodeID(t *testing.T) {
	// Force a tie by reusing the same path.
	in := []Candidate{
		{EpisodeID: 5, ShowID: 1, RootPath: `D:\X`, RelativePath: `same.mkv`},
		{EpisodeID: 2, ShowID: 2, RootPath: `D:\X`, RelativePath: `same.mkv`},
		{EpisodeID: 9, ShowID: 3, RootPath: `D:\X`, RelativePath: `same.mkv`},
	}
	got := Sort(in)
	wantIDs := []int64{2, 5, 9}
	for i, w := range wantIDs {
		if got[i].EpisodeID != w {
			t.Fatalf("idx %d: got EpisodeID=%d, want %d", i, got[i].EpisodeID, w)
		}
	}
}
