import { useEffect, useRef, useState } from 'react';
import {
  subscribeStatus,
  listShows,
  listHistory,
  pause,
  skip,
  defer,
  seekPercent,
  seekRelative,
  setVolume,
  setSub,
  setAudio,
  type Status,
  type Show,
  type HistoryEvent,
  type Playback,
} from './api';
import './App.css';

// Qt build: the overlay composites ON TOP of live mpv video. The UI is
// adaptive — an always-on control bar (phase + now-playing + pause/skip),
// plus the full dashboard (sidebar + queue + history + status). The
// dashboard shows automatically when NOT playing; during playback it's
// hidden so the video is visible, but the `v`/Tab key (or the "show list"
// button) toggles it back on over the video.

function relTime(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return '';
  const days = Math.floor((Date.now() - d.getTime()) / 86_400_000);
  if (days === 0) return 'today';
  if (days === 1) return 'yesterday';
  if (days < 30) return `${days}d ago`;
  if (days < 365) return `${Math.floor(days / 30)}mo ago`;
  return `${Math.floor(days / 365)}y ago`;
}

// Index of the entry currently playing, clamped into the round.
function currentPos(status: Status): number {
  const n = status.round?.length ?? 0;
  if (n === 0) return 0;
  return Math.min(Math.max(status.round_pos ?? 0, 0), n - 1);
}

function App() {
  const [status, setStatus] = useState<Status>({
    phase: 'initializing',
    message: '',
    playlist: '',
  });
  const [shows, setShows] = useState<Show[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [history, setHistory] = useState<HistoryEvent[]>([]);
  const [showDashboard, setShowDashboard] = useState(false);

  useEffect(() => subscribeStatus(setStatus), []);

  // Latest playback, mirrored to a ref so the (mount-once) key handler can read
  // current volume for relative +/- without re-binding every poll.
  const pbRef = useRef<Playback | undefined>(undefined);
  useEffect(() => {
    pbRef.current = status.playback;
  }, [status.playback]);

  // Keyboard controls. Bound on window so they work whenever the overlay has
  // focus (main.py gives the WebEngineView active focus). space=pause/play,
  // n/→=skip, d=defer, j/l=seek -/+10s, ↑/↓=volume, v/Tab=dashboard, Esc=hide.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      switch (e.key) {
        case ' ':
          e.preventDefault();
          pause();
          break;
        case 'n':
        case 'ArrowRight':
          skip();
          break;
        case 'd':
          defer();
          break;
        case 'j':
          seekRelative(-10);
          break;
        case 'l':
          seekRelative(10);
          break;
        case 'ArrowUp': {
          e.preventDefault();
          setVolume(Math.min(130, (pbRef.current?.volume ?? 100) + 5));
          break;
        }
        case 'ArrowDown': {
          e.preventDefault();
          setVolume(Math.max(0, (pbRef.current?.volume ?? 100) - 5));
          break;
        }
        case 'v':
        case 'Tab':
          e.preventDefault();
          setShowDashboard((v) => !v);
          break;
        case 'Escape':
          setShowDashboard(false);
          break;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  useEffect(() => {
    // Re-fetch shows whenever the playlist gains a round or advance — a
    // finished show drops out of the active list after advance. Keyed on
    // advanced_count (a primitive) so polling's fresh Status objects don't
    // retrigger this every tick.
    if (status.phase === 'playing' || status.phase === 'fetching' || status.phase === 'drained') {
      listShows()
        .then((s) => setShows(s ?? []))
        .catch(() => setShows([]));
    }
  }, [status.phase, status.last_advance?.advanced_count]);

  useEffect(() => {
    // Watch history for the selected show.
    if (!selected) {
      setHistory([]);
      return;
    }
    let alive = true;
    listHistory(selected)
      .then((h) => alive && setHistory(h ?? []))
      .catch(() => alive && setHistory([]));
    return () => {
      alive = false;
    };
  }, [selected, status.last_advance?.advanced_count]);

  const playingByShow = new Set((status.round ?? []).map((r) => r.show_id));
  const playing = status.phase === 'playing';
  const dashboardVisible = !playing || showDashboard;
  const round = status.round ?? [];
  const pos = currentPos(status);
  const selectedShow = shows.find((s) => s.id === selected) ?? null;

  return (
    <div className="overlay-root">
      <ControlBar
        status={status}
        pos={pos}
        playing={playing}
        viewing={showDashboard}
        onToggleView={() => setShowDashboard((v) => !v)}
      />
      {playing && <PlaybackBar pb={status.playback} />}
      {dashboardVisible && (
        <div className={`layout${playing ? ' over-video' : ''}`}>
          <aside className="sidebar">
            <h2>{status.playlist || 'playlist'}</h2>
            {shows.length === 0 ? (
              <div className="empty" style={{ margin: '0 16px' }}>
                no shows yet.
              </div>
            ) : (
              <ul>
                {shows.map((sh) => (
                  <li
                    key={sh.id}
                    className={selected === sh.id ? 'selected' : ''}
                    onClick={() => setSelected(selected === sh.id ? null : sh.id)}
                  >
                    <div>{sh.name}</div>
                    <div className="meta">
                      added {relTime(sh.date_added)}
                      {playingByShow.has(sh.id) && (
                        <span className="pill busy" style={{ marginLeft: 8 }}>
                          playing
                        </span>
                      )}
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </aside>

          <main className="main">
            <div className="kpi">
              <div className="kpi-cell">
                <div className="kpi-key">phase</div>
                <div className="kpi-val">
                  <span className={`pill ${pillClass(status.phase)}`}>{status.phase}</span>
                </div>
              </div>
              <div className="kpi-cell">
                <div className="kpi-key">active shows</div>
                <div className="kpi-val">{shows.length}</div>
              </div>
              <div className="kpi-cell">
                <div className="kpi-key">round</div>
                <div className="kpi-val">
                  {round.length ? `${pos + 1}/${round.length}` : '—'}
                </div>
              </div>
              <div className="kpi-cell">
                <div className="kpi-key">last advance</div>
                <div className="kpi-val">{status.last_advance?.advanced_count ?? 0}</div>
              </div>
            </div>

            {selectedShow ? (
              <ShowHistory show={selectedShow} events={history} onClose={() => setSelected(null)} />
            ) : (
              <>
                <Queue round={round} pos={pos} />
                {round.length === 0 && (
                  <div className="section">
                    <h3>status</h3>
                    <div style={{ color: 'var(--fg-secondary)' }}>{status.message || '—'}</div>
                    {status.phase === 'auth' && (
                      <p className="filter-hint">a browser tab should be open. approve, then come back.</p>
                    )}
                  </div>
                )}
                {status.last_advance?.removed_shows && status.last_advance.removed_shows.length > 0 && (
                  <div className="section">
                    <h3>just finished</h3>
                    <table className="runs">
                      <thead>
                        <tr>
                          <th>show</th>
                          <th>added</th>
                          <th>took</th>
                        </tr>
                      </thead>
                      <tbody>
                        {status.last_advance.removed_shows.map((r) => (
                          <tr key={r.id}>
                            <td>{r.name}</td>
                            <td style={{ color: 'var(--fg-dim)' }}>{relTime(r.date_added)}</td>
                            <td style={{ color: 'var(--fg-dim)' }}>{durationDays(r.date_added, r.last_played_at)}</td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
              </>
            )}
          </main>
        </div>
      )}
    </div>
  );
}

// Always-on control bar at the top — the only chrome over live video when
// the dashboard is hidden. Top-anchored + semi-opaque so it composites
// reliably over the mpv layer.
function ControlBar({
  status,
  pos,
  playing,
  viewing,
  onToggleView,
}: {
  status: Status;
  pos: number;
  playing: boolean;
  viewing: boolean;
  onToggleView: () => void;
}) {
  const round = status.round ?? [];
  const now = round.length
    ? `${round[pos].show_name}   (${pos + 1}/${round.length})`
    : status.message || '—';
  return (
    <div className="controlbar">
      <span className={`pill ${pillClass(status.phase)}`}>{status.phase}</span>
      <span className="now">{now}</span>
      <button className="gb" onClick={() => pause()} title="space">
        pause / play
      </button>
      <button className="gb" onClick={() => skip()} title="n — mark watched, next">
        skip
      </button>
      <button className="gb" onClick={() => defer()} title="d — different episode next round">
        defer
      </button>
      {playing && (
        <button className="gb" onClick={onToggleView} title="v / tab">
          {viewing ? 'hide list' : 'show list'}
        </button>
      )}
      <span className="keys">space · n · d · j/l · ↑↓ · v · esc</span>
    </div>
  );
}

function fmtTime(s: number | null | undefined): string {
  if (s == null || isNaN(s)) return '--:--';
  const t = Math.max(0, Math.floor(s));
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m);
  return (h > 0 ? `${h}:` : '') + `${mm}:${String(sec).padStart(2, '0')}`;
}

// Scrub bar + time + volume + subtitle/audio menus, shown under the control bar
// during playback. Driven by status.playback (live mpv state, polled).
function PlaybackBar({ pb }: { pb?: Playback }) {
  if (!pb) return null;
  const pct =
    pb.percent_pos ??
    (pb.duration && pb.time_pos != null ? (pb.time_pos / pb.duration) * 100 : 0);
  return (
    <div className="playbar">
      <span className="time">{fmtTime(pb.time_pos)}</span>
      <div
        className="scrub"
        onClick={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          seekPercent(Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100)));
        }}
      >
        <div className="scrub-fill" style={{ width: `${pct ?? 0}%` }} />
      </div>
      <span className="time">{fmtTime(pb.duration)}</span>
      <label className="vol" title="volume (up / down)">
        vol
        <input
          type="range"
          min={0}
          max={130}
          value={Math.round(pb.volume ?? 100)}
          onChange={(e) => setVolume(Number(e.currentTarget.value))}
        />
      </label>
      {pb.sub_tracks.length > 0 && (
        <select
          className="trk"
          title="subtitles"
          value={String(pb.sid ?? 'no')}
          onChange={(e) =>
            setSub(e.currentTarget.value === 'no' ? 'no' : Number(e.currentTarget.value))
          }
        >
          <option value="no">subs: off</option>
          {pb.sub_tracks.map((t) => (
            <option key={t.id} value={t.id}>
              {t.title}
            </option>
          ))}
        </select>
      )}
      {pb.audio_tracks.length > 1 && (
        <select
          className="trk"
          title="audio track"
          value={String(pb.aid ?? '')}
          onChange={(e) => setAudio(Number(e.currentTarget.value))}
        >
          {pb.audio_tracks.map((t) => (
            <option key={t.id} value={t.id}>
              {t.title}
            </option>
          ))}
        </select>
      )}
    </div>
  );
}

// Flat "now playing / up next" queue. Entries are in play order; the current
// one is marked, earlier ones are done, later ones are what's next.
function Queue({ round, pos }: { round: Status['round']; pos: number }) {
  const entries = round ?? [];
  if (entries.length === 0) return null;
  // Show the playlist tag only when the round interleaves more than one
  // (a cross-playlist round); for a single playlist it's just noise.
  const multiPlaylist =
    new Set(entries.map((e) => e.playlist).filter(Boolean)).size > 1;
  return (
    <div className="section">
      <h3>queue</h3>
      <ul className="queue">
        {entries.map((r, i) => (
          <li key={r.episode_id} className={i === pos ? 'now' : i < pos ? 'done' : 'next'}>
            <span className="q-mark">{i === pos ? '▶' : i < pos ? '✓' : ''}</span>
            {multiPlaylist && r.playlist && <span className="q-pl">{r.playlist}</span>}
            <span className="q-show">{r.show_name}</span>
            <span className="q-ep">{shortPath(r.absolute_path)}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function ShowHistory({
  show,
  events,
  onClose,
}: {
  show: Show;
  events: HistoryEvent[];
  onClose: () => void;
}) {
  return (
    <div className="section">
      <h3>
        history — {show.name}
        <button className="gb" style={{ marginLeft: 12 }} onClick={onClose}>
          back
        </button>
      </h3>
      {events.length === 0 ? (
        <div className="empty">no watch history yet.</div>
      ) : (
        <table className="runs">
          <thead>
            <tr>
              <th>episode</th>
              <th>watched</th>
            </tr>
          </thead>
          <tbody>
            {events.map((e) => (
              <tr key={e.episode_id + e.played_at}>
                <td>{shortPath(e.relative_path)}</td>
                <td style={{ color: 'var(--fg-dim)' }}>{relTime(e.played_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function pillClass(phase: Status['phase']): string {
  switch (phase) {
    case 'playing':
      return 'busy';
    case 'drained':
      return 'drain';
    case 'error':
      return 'drain';
    case 'fetching':
    case 'auth':
    case 'initializing':
      return 'info';
    default:
      return 'info';
  }
}

function shortPath(p: string): string {
  if (!p) return '';
  const parts = p.split(/[\\/]/);
  return parts[parts.length - 1];
}

function durationDays(start: string, end: string): string {
  const a = new Date(start).getTime();
  const b = new Date(end).getTime();
  if (isNaN(a) || isNaN(b)) return '';
  const days = Math.floor((b - a) / 86_400_000);
  return `${days}d`;
}

export default App;
