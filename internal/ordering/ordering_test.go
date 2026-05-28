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
		{EpisodeID: "e1", ShowID: "s1", RootPath: `D:\A`, RelativePath: `a.mkv`},
		{EpisodeID: "e2", ShowID: "s2", RootPath: `D:\B`, RelativePath: `b.mkv`},
		{EpisodeID: "e3", ShowID: "s3", RootPath: `D:\C`, RelativePath: `c.mkv`},
		{EpisodeID: "e4", ShowID: "s4", RootPath: `D:\D`, RelativePath: `d.mkv`},
	}
	first := Sort(in)
	second := Sort(in)
	if len(first) != len(second) {
		t.Fatalf("length mismatch: %d vs %d", len(first), len(second))
	}
	for i := range first {
		if first[i].EpisodeID != second[i].EpisodeID {
			t.Fatalf("non-deterministic at idx %d: %s vs %s", i, first[i].EpisodeID, second[i].EpisodeID)
		}
	}
	for i := 1; i < len(first); i++ {
		if first[i-1].OrderValue > first[i].OrderValue {
			t.Fatalf("not sorted ascending: idx %d (%#x) > idx %d (%#x)",
				i-1, first[i-1].OrderValue, i, first[i].OrderValue)
		}
	}
}

// X1 (cross-playlist rounds): a cross-playlist round is identical to
// sorting the union of every playlist's candidates, because Sort keys on
// the absolute path alone — it never looks at playlist membership or input
// order. Permuting the input stands in for different playlist groupings /
// fetch orders of the same union; the sorted result must be identical.
func TestSort_PermutationInvariant(t *testing.T) {
	base := []Candidate{
		{EpisodeID: "e1", ShowID: "s1", RootPath: `D:\A`, RelativePath: `a.mkv`},
		{EpisodeID: "e2", ShowID: "s2", RootPath: `D:\B`, RelativePath: `b.mkv`},
		{EpisodeID: "e3", ShowID: "s3", RootPath: `D:\C`, RelativePath: `c.mkv`},
		{EpisodeID: "e4", ShowID: "s4", RootPath: `D:\D`, RelativePath: `d.mkv`},
		{EpisodeID: "e5", ShowID: "s5", RootPath: `D:\E`, RelativePath: `e.mkv`},
	}
	want := Sort(base)

	perms := [][]int{
		{4, 3, 2, 1, 0},
		{2, 0, 4, 1, 3},
		{1, 2, 0, 4, 3},
	}
	for _, p := range perms {
		shuffled := make([]Candidate, len(p))
		for i, idx := range p {
			shuffled[i] = base[idx]
		}
		got := Sort(shuffled)
		if len(got) != len(want) {
			t.Fatalf("perm %v: len %d, want %d", p, len(got), len(want))
		}
		for i := range want {
			if got[i].EpisodeID != want[i].EpisodeID || got[i].OrderValue != want[i].OrderValue {
				t.Fatalf("perm %v idx %d: got (%s,%#x), want (%s,%#x)",
					p, i, got[i].EpisodeID, got[i].OrderValue, want[i].EpisodeID, want[i].OrderValue)
			}
		}
	}
}

func TestSort_TieBreakOnEpisodeID(t *testing.T) {
	// Force a tie by reusing the same path. Tie-break is lexical on the
	// string EpisodeID, so "a" < "b" < "c".
	in := []Candidate{
		{EpisodeID: "c", ShowID: "s1", RootPath: `D:\X`, RelativePath: `same.mkv`},
		{EpisodeID: "a", ShowID: "s2", RootPath: `D:\X`, RelativePath: `same.mkv`},
		{EpisodeID: "b", ShowID: "s3", RootPath: `D:\X`, RelativePath: `same.mkv`},
	}
	got := Sort(in)
	wantIDs := []string{"a", "b", "c"}
	for i, w := range wantIDs {
		if got[i].EpisodeID != w {
			t.Fatalf("idx %d: got EpisodeID=%q, want %q", i, got[i].EpisodeID, w)
		}
	}
}
