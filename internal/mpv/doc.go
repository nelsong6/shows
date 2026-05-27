// Package mpv drives a local mpv player process over its JSON IPC socket.
//
// On Windows we connect to mpv's --input-ipc-server named pipe via
// github.com/Microsoft/go-winio. The implementation is Windows-only for
// now (the local client only ships as a Windows binary); on other GOOS
// the package compiles to an empty shim.
//
// Protocol: mpv emits one JSON object per line. Commands are JSON of the
// form { "command": [name, args...], "request_id": <int64> }; mpv
// replies with { "request_id": ..., "error": "success"|..., "data": ... }.
// Async events are JSON of the form { "event": "end-file", ... }.
// See https://mpv.io/manual/stable/#json-ipc.
package mpv
