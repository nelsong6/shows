//go:build windows

// Package win32 wraps the small set of Win32 calls shows-desktop needs
// to bridge Wails's window to libmpv's render target. Today: one helper
// that locates the Wails main HWND by title. Future: enumerate child
// windows when we want to embed mpv into a sub-region rather than
// covering the whole window.
package win32

import (
	"errors"
	"syscall"
	"unsafe"
	"time"

	"golang.org/x/sys/windows"
)

var (
	user32         = windows.NewLazySystemDLL("user32.dll")
	procFindWindow = user32.NewProc("FindWindowW")
)

// FindWindowByTitle returns the HWND of the first top-level window
// with the given title, or an error if none exists yet. Wails creates
// its host window during startup; right at OnStartup the window may
// not be enumerable yet, so callers typically retry briefly. See
// WaitForWindow.
func FindWindowByTitle(title string) (uintptr, error) {
	ptr, err := syscall.UTF16PtrFromString(title)
	if err != nil {
		return 0, err
	}
	hwnd, _, _ := procFindWindow.Call(0, uintptr(unsafe.Pointer(ptr)))
	if hwnd == 0 {
		return 0, errors.New("win32: window not found")
	}
	return hwnd, nil
}

// WaitForWindow polls FindWindowByTitle until the window appears or
// the deadline is hit. Wails's startup sequence usually has the host
// window ready by the time OnStartup fires, but this guards against
// the race where libmpv parenting is attempted before the host window
// is visible to the Windows shell.
func WaitForWindow(title string, timeout time.Duration) (uintptr, error) {
	deadline := time.Now().Add(timeout)
	for {
		hwnd, err := FindWindowByTitle(title)
		if err == nil {
			return hwnd, nil
		}
		if time.Now().After(deadline) {
			return 0, err
		}
		time.Sleep(25 * time.Millisecond)
	}
}
