//go:build !windows

package main

import (
	"fmt"
	"os"
)

func main() {
	fmt.Fprintln(os.Stderr, "shows-client is Windows-only (uses mpv via named-pipe IPC).")
	fmt.Fprintln(os.Stderr, "Build with: GOOS=windows go build ./cmd/shows-client")
	os.Exit(1)
}
