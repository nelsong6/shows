//go:build windows

// Package player wraps libmpv via dweymouth/go-mpv. This file owns the
// Windows-specific cgo flags so cgo finds libmpv's headers + import lib
// without callers having to set CGO_CFLAGS / CGO_LDFLAGS by hand.
//
// libmpv lives in desktop/third_party/libmpv, populated by
// scripts/setup-libmpv.ps1 (idempotent download from shinchiro's
// mpv-player-windows builds on SourceForge). The path here uses
// ${SRCDIR} so it resolves correctly regardless of where the module
// is checked out.
package player

// #cgo windows CFLAGS: -I${SRCDIR}/../../third_party/libmpv/include
// #cgo windows LDFLAGS: -L${SRCDIR}/../../third_party/libmpv -lmpv
import "C"
