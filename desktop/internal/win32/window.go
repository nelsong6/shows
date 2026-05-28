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
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
)

var (
	user32               = windows.NewLazySystemDLL("user32.dll")
	procFindWindow       = user32.NewProc("FindWindowW")
	procEnumChildWindows = user32.NewProc("EnumChildWindows")
	procGetClassNameW    = user32.NewProc("GetClassNameW")
	procSetWindowPos     = user32.NewProc("SetWindowPos")
	procGetClientRect    = user32.NewProc("GetClientRect")
)

type rect struct{ left, top, right, bottom int32 }

const (
	swpNoActivate = 0x0010
	swpShowWindow = 0x0040
	// hwndTop (0) raises the window to the top of its sibling z-order.
	hwndTop = 0
)

// RaiseChildByClass finds the first descendant of parent whose window
// class equals className, raises it to the top of the sibling z-order,
// and resizes it to fill parent's client area. Returns the matched HWND,
// or 0 if no descendant matched.
//
// shows-desktop uses this to lift libmpv's render window (registered
// class "mpv") above the Wails WebView2 control. Both are children of
// the top-level host window; WebView2 is created first and otherwise
// wins the z-order, so mpv's video draws *behind* the React chrome —
// audible but invisible. Called at the start of each round so a
// WebView2 re-assert (e.g. on focus) doesn't permanently re-bury it.
func RaiseChildByClass(parent uintptr, className string) uintptr {
	var found uintptr
	cb := syscall.NewCallback(func(hwnd, _ uintptr) uintptr {
		buf := make([]uint16, 64)
		n, _, _ := procGetClassNameW.Call(hwnd, uintptr(unsafe.Pointer(&buf[0])), uintptr(len(buf)))
		if n > 0 && windows.UTF16ToString(buf[:n]) == className {
			found = hwnd
			return 0 // stop enumeration
		}
		return 1 // continue
	})
	_, _, _ = procEnumChildWindows.Call(parent, cb, 0)
	if found == 0 {
		return 0
	}
	var r rect
	_, _, _ = procGetClientRect.Call(parent, uintptr(unsafe.Pointer(&r)))
	_, _, _ = procSetWindowPos.Call(
		found, hwndTop,
		0, 0, uintptr(r.right-r.left), uintptr(r.bottom-r.top),
		swpShowWindow|swpNoActivate,
	)
	return found
}

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
