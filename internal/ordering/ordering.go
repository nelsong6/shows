// Package ordering computes the deterministic-random round order that the
// legacy play_ordered_show.ps1 used.
//
// For each candidate episode we hash its absolute path with SHA-256, take
// the first four hex characters of the digest, and parse them as a uint32.
// Episodes in a round are sorted ascending by that value. The ordering is
// deterministic, so re-fetching a round before any advance returns the
// same shuffle — the property that lets the client survive crashes
// without re-rolling the order.
//
// This mirrors:
//
//	$stringAsStream = [System.IO.MemoryStream]::new()
//	$writer = [System.IO.StreamWriter]::new($stringAsStream)  # UTF-8, no BOM
//	$writer.write($showPath)
//	$rawHex = Get-FileHash -InputStream $stringAsStream        # SHA-256
//	$truncatedHexValue = $rawHex.hash.SubString(0, 4)
//	$hexToInt = [uint32]("0x" + $truncatedHexValue)
//
// from play_ordered_show.ps1.
package ordering

import (
	"crypto/sha256"
	"encoding/hex"
	"sort"
	"strconv"
	"strings"
)

// PathSeparator is the path separator the legacy scripts used to join show
// root directories with episode relative paths. The hash input must use
// backslash, regardless of the host OS the API runs on, to stay compatible
// with the existing playlist files we migrate from.
const PathSeparator = "\\"

// JoinPath builds the absolute path string that gets hashed for ordering.
// It does NOT use filepath.Join — that would collapse separators based on
// the host OS, which would change the hash on Linux pods vs. Windows
// clients.
func JoinPath(rootPath, relativePath string) string {
	r := strings.TrimRight(rootPath, "\\/")
	p := strings.TrimLeft(relativePath, "\\/")
	return r + PathSeparator + p
}

// OrderValue computes the uint32 sort key for a single absolute path.
func OrderValue(absolutePath string) uint32 {
	sum := sha256.Sum256([]byte(absolutePath))
	prefix := hex.EncodeToString(sum[:])[:4]
	// strconv.ParseUint is case-insensitive for hex; PowerShell's
	// Get-FileHash returns upper-case but it doesn't matter for parsing.
	n, _ := strconv.ParseUint(prefix, 16, 32)
	return uint32(n)
}

// Candidate is one episode being considered for a round. IDs are
// strings because the Cosmos-backed store uses UUIDs; the legacy
// Postgres int64 IDs are gone.
type Candidate struct {
	EpisodeID    string
	ShowID       string
	RootPath     string
	RelativePath string
}

// Ordered is a candidate with its precomputed sort key. Returned from
// Sort so the caller can persist the order (or echo it back to the
// client) without rehashing.
type Ordered struct {
	Candidate
	AbsolutePath string
	OrderValue   uint32
}

// Sort returns the input candidates in round order. Stable on
// (OrderValue, EpisodeID) — ties (which are extremely rare with a
// 32-bit key over a small N) break by EpisodeID so the order is fully
// deterministic.
func Sort(in []Candidate) []Ordered {
	out := make([]Ordered, len(in))
	for i, c := range in {
		abs := JoinPath(c.RootPath, c.RelativePath)
		out[i] = Ordered{
			Candidate:    c,
			AbsolutePath: abs,
			OrderValue:   OrderValue(abs),
		}
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].OrderValue != out[j].OrderValue {
			return out[i].OrderValue < out[j].OrderValue
		}
		return out[i].EpisodeID < out[j].EpisodeID
	})
	return out
}
