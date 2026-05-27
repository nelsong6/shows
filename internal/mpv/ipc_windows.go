//go:build windows

package mpv

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os/exec"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Microsoft/go-winio"
)

// PipePath is the named-pipe address we pass to mpv via
// --input-ipc-server. Pipe names are global on Windows, so a hard-coded
// path is fine for a single-user app.
const PipePath = `\\.\pipe\shows-mpv`

// Config controls mpv launch behavior. The defaults match the intended
// "tv channel" experience: fullscreen, on-screen controls visible, mpv
// stays alive between files so the client can queue the next one.
type Config struct {
	// MPVBinary is the path to mpv.exe. Empty means look it up via PATH.
	MPVBinary string
	// ExtraArgs is appended to the default arg list. Useful for
	// debugging (--log-file=..., --msg-level=ipc=v).
	ExtraArgs []string
}

// Event is one async message emitted by mpv. Name corresponds to the
// "event" JSON key (e.g. "start-file", "end-file", "file-loaded"). For
// end-file events, Reason carries one of {eof, stop, quit, error,
// redirect, unknown}.
type Event struct {
	Name   string
	Reason string
	Raw    json.RawMessage
}

// Client controls a running mpv process. Construct with Start; methods
// are safe to call concurrently. Close terminates the process.
type Client struct {
	cmd    *exec.Cmd
	conn   net.Conn
	writer *bufio.Writer

	nextID atomic.Int64

	mu      sync.Mutex
	pending map[int64]chan response

	events chan Event
	done   chan struct{}
	closed atomic.Bool
}

type request struct {
	Command   []any `json:"command"`
	RequestID int64 `json:"request_id"`
}

type response struct {
	RequestID int64           `json:"request_id"`
	Data      json.RawMessage `json:"data,omitempty"`
	Error     string          `json:"error"`
}

// Start spawns mpv with the IPC pipe enabled, then connects to the pipe.
// Returns once the pipe is ready and the read loop is running. The caller
// can immediately call LoadFile etc.
//
// If mpv exits before the pipe is reachable (e.g. binary not found,
// invalid arg), Start returns an error.
func Start(ctx context.Context, cfg Config) (*Client, error) {
	bin := cfg.MPVBinary
	if bin == "" {
		bin = "mpv"
	}
	args := []string{
		"--idle",
		"--input-ipc-server=" + PipePath,
		"--fullscreen",
		"--osc=yes",
		"--keep-open=yes",
		"--force-window=yes",
	}
	args = append(args, cfg.ExtraArgs...)

	cmd := exec.CommandContext(ctx, bin, args...)
	// Don't inherit stdin; we don't want mpv reading from the terminal.
	cmd.Stdout = nil
	cmd.Stderr = nil
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("mpv: start: %w", err)
	}

	// Poll the pipe until mpv has created it, with an overall budget.
	conn, err := dialWithRetry(ctx, PipePath, 10*time.Second)
	if err != nil {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		return nil, fmt.Errorf("mpv: connect ipc: %w", err)
	}

	c := &Client{
		cmd:     cmd,
		conn:    conn,
		writer:  bufio.NewWriter(conn),
		pending: make(map[int64]chan response),
		events:  make(chan Event, 32),
		done:    make(chan struct{}),
	}
	go c.readLoop()
	return c, nil
}

func dialWithRetry(ctx context.Context, path string, total time.Duration) (net.Conn, error) {
	deadline := time.Now().Add(total)
	for {
		// go-winio's DialPipe with a per-attempt timeout. Use a short
		// timeout per attempt so a stalled wait doesn't blow the whole
		// budget.
		attempt := 500 * time.Millisecond
		conn, err := winio.DialPipe(path, &attempt)
		if err == nil {
			return conn, nil
		}
		if time.Now().After(deadline) {
			return nil, err
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(100 * time.Millisecond):
		}
	}
}

func (c *Client) readLoop() {
	defer close(c.events)
	defer close(c.done)

	scanner := bufio.NewScanner(c.conn)
	// mpv can emit fairly large JSON for property responses. Bump the
	// per-line buffer from the default 64KiB to 1MiB.
	scanner.Buffer(make([]byte, 64*1024), 1024*1024)

	for scanner.Scan() {
		line := scanner.Bytes()
		// One JSON object per line. Classify by which keys are present.
		var probe struct {
			Event     string `json:"event"`
			RequestID *int64 `json:"request_id"`
		}
		if err := json.Unmarshal(line, &probe); err != nil {
			continue
		}
		if probe.Event != "" {
			var ev struct {
				Event  string `json:"event"`
				Reason string `json:"reason,omitempty"`
			}
			_ = json.Unmarshal(line, &ev)
			raw := make(json.RawMessage, len(line))
			copy(raw, line)
			select {
			case c.events <- Event{Name: ev.Event, Reason: ev.Reason, Raw: raw}:
			default:
				// Drop if the consumer isn't draining. The events
				// channel is the equivalent of a UDP stream — we
				// always prefer the latest behavior over blocking
				// the mpv connection.
			}
			continue
		}
		if probe.RequestID != nil {
			var resp response
			if err := json.Unmarshal(line, &resp); err != nil {
				continue
			}
			c.mu.Lock()
			ch, ok := c.pending[resp.RequestID]
			delete(c.pending, resp.RequestID)
			c.mu.Unlock()
			if ok {
				ch <- resp
			}
		}
	}
}

// Events returns the channel of async mpv events. It's closed when the
// underlying connection drops (mpv exited or Close was called).
func (c *Client) Events() <-chan Event { return c.events }

// Done returns a channel that closes when the read loop exits — i.e.
// when mpv has gone away.
func (c *Client) Done() <-chan struct{} { return c.done }

// command sends one IPC command and waits for the matching response.
// Returns the data payload (if any) and the error string mpv returned.
func (c *Client) command(ctx context.Context, args ...any) (json.RawMessage, error) {
	if c.closed.Load() {
		return nil, errors.New("mpv: client closed")
	}
	id := c.nextID.Add(1)
	ch := make(chan response, 1)
	c.mu.Lock()
	c.pending[id] = ch
	c.mu.Unlock()

	raw, err := json.Marshal(request{Command: args, RequestID: id})
	if err != nil {
		c.mu.Lock()
		delete(c.pending, id)
		c.mu.Unlock()
		return nil, err
	}

	c.mu.Lock() // serialize writes — net.Conn isn't required to be safe for concurrent writers
	_, werr := c.writer.Write(append(raw, '\n'))
	if werr == nil {
		werr = c.writer.Flush()
	}
	c.mu.Unlock()
	if werr != nil {
		return nil, fmt.Errorf("mpv: write: %w", werr)
	}

	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case resp := <-ch:
		if resp.Error != "success" {
			return resp.Data, fmt.Errorf("mpv: %s", resp.Error)
		}
		return resp.Data, nil
	case <-c.done:
		return nil, io.ErrUnexpectedEOF
	}
}

// LoadMode controls how a loadfile command interacts with the existing
// playlist. "replace" clears the current entry; "append" / "append-play"
// queue behind it.
type LoadMode string

const (
	LoadReplace    LoadMode = "replace"
	LoadAppend     LoadMode = "append"
	LoadAppendPlay LoadMode = "append-play"
)

// LoadFile asks mpv to play (or queue) a file. With LoadReplace the file
// starts immediately; with LoadAppend it queues silently behind the
// current item.
func (c *Client) LoadFile(ctx context.Context, path string, mode LoadMode) error {
	_, err := c.command(ctx, "loadfile", path, string(mode))
	return err
}

// Stop clears mpv's playlist and stops playback. mpv stays running
// because --idle + --keep-open were set.
func (c *Client) Stop(ctx context.Context) error {
	_, err := c.command(ctx, "stop")
	return err
}

// Quit asks mpv to exit cleanly. After Quit, the events channel will
// close shortly.
func (c *Client) Quit(ctx context.Context) error {
	_, err := c.command(ctx, "quit")
	return err
}

// Close terminates mpv (best-effort Quit, then Kill on timeout). Safe to
// call multiple times.
func (c *Client) Close() error {
	if !c.closed.CompareAndSwap(false, true) {
		return nil
	}
	quitCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	_ = c.Quit(quitCtx)
	cancel()

	// Best-effort wait for the process to exit cleanly. If it doesn't,
	// kill it.
	exitCh := make(chan error, 1)
	go func() { exitCh <- c.cmd.Wait() }()
	select {
	case <-exitCh:
	case <-time.After(3 * time.Second):
		_ = c.cmd.Process.Kill()
		<-exitCh
	}
	_ = c.conn.Close()
	return nil
}
