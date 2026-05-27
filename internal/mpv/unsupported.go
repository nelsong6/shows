//go:build !windows

package mpv

// The mpv driver uses Windows named pipes; cross-compile cmd/shows-client
// with GOOS=windows. This file keeps the package non-empty so `go build ./...`
// on non-Windows hosts (e.g. the API's Linux CI build) succeeds.
