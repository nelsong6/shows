import { useEffect, useState } from 'react';
import {
  subscribeStatus,
  listShows,
  pause,
  skip,
  type Status,
  type Show,
} from './api';
import './App.css';

// Qt build: the overlay composites ON TOP of live mpv video. So the UI is
// adaptive — an always-on control bar (phase + now-playing + pause/skip),
// plus the full dashboard (sidebar + status panels) only when NOT playing.
// During playback the dashboard collapses so the video is visible; a
// top-anchored bar composites reliably over actively-rendering video
// (a bottom-anchored or full-screen panel does not — see desktop-qt README).

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

function App() {
  const [status, setStatus] = useState<Status>({
    phase: 'initializing',
    message: '',
    playlist: '',
  });
  const [shows, setShows] = useState<Show[]>([]);
  const [selected, setSelected] = useState<string | null>(null);

  useEffect(() => subscribeStatus(setStatus), []);

  useEffect(() => {
    // Re-fetch shows whenever the playlist gains a round or advance —
    // a finished show drops out of the active list after advance. Keyed
    // on advanced_count (a primitive) so polling's fresh Status objects
    // don't retrigger this every tick.
    if (status.phase === 'playing' || status.phase === 'fetching' || status.phase === 'drained') {
      listShows()
        .then((s) => setShows(s ?? []))
        .catch(() => setShows([]));
    }
  }, [status.phase, status.last_advance?.advanced_count]);

  const playingByShow = new Set((status.round ?? []).map((r) => r.show_id));
  const playing = status.phase === 'playing';

  return (
    <div className="overlay-root">
      <ControlBar status={status} />
      {!playing && (
        <div className="layout">
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
                    onClick={() => setSelected(sh.id)}
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
                <div className="kpi-key">current round</div>
                <div className="kpi-val">{status.round?.length ?? 0}</div>
              </div>
              <div className="kpi-cell">
                <div className="kpi-key">last advance</div>
                <div className="kpi-val">{status.last_advance?.advanced_count ?? 0}</div>
              </div>
            </div>

            <div className="section">
              <h3>status</h3>
              <div style={{ color: 'var(--fg-secondary)' }}>{status.message || '—'}</div>
              {status.phase === 'auth' && (
                <p className="filter-hint">a browser tab should be open. approve, then come back.</p>
              )}
            </div>

            {status.round && status.round.length > 0 && (
              <div className="section">
                <h3>current round</h3>
                <table className="runs">
                  <thead>
                    <tr>
                      <th>show</th>
                      <th>episode</th>
                      <th>order</th>
                    </tr>
                  </thead>
                  <tbody>
                    {status.round.map((r) => (
                      <tr key={r.episode_id}>
                        <td>{r.show_name}</td>
                        <td style={{ color: 'var(--fg-dim)' }}>{shortPath(r.absolute_path)}</td>
                        <td style={{ color: 'var(--fg-dim)' }}>{r.order_value}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
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
          </main>
        </div>
      )}
    </div>
  );
}

// Always-on control bar at the top — the only chrome over live video.
// Top-anchored + semi-opaque so it composites reliably over the mpv layer.
function ControlBar({ status }: { status: Status }) {
  const round = status.round ?? [];
  const now = round.length
    ? `${round[0].show_name}   (1/${round.length})`
    : status.message || '—';
  return (
    <div className="controlbar">
      <span className={`pill ${pillClass(status.phase)}`}>{status.phase}</span>
      <span className="now">{now}</span>
      <button className="gb" onClick={() => pause()}>
        pause / play
      </button>
      <button className="gb" onClick={() => skip()}>
        skip
      </button>
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
  // Display just the basename — full path is in console output for
  // diagnostic purposes; the table cell is for show-at-a-glance.
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
