import { useCallback, useEffect, useRef, useState, type WheelEventHandler } from 'react';
import {
  subscribeStatus,
  listShows,
  listHistory,
  getStats,
  pause,
  skip,
  previous,
  playShow,
  markShowWatched,
  markShowUnwatched,
  defer,
  toggleFullscreen,
  minimizeWindow,
  maximizeWindow,
  closeWindow,
  seekPercent,
  seekRelative,
  setVolume as sendVolume,
  setSub,
  setAudio,
  syncNow,
  addShow,
  previewShow,
  pickFolder,
  getNextRound,
  type NextRoundEpisode,
  removeShow,
  rescanShow,
  type Status,
  type Show,
  type HistoryEvent,
  type Playback,
  type Stats,
  type UpdateInfo,
} from './api';
import './App.css';

const VOLUME_MIN = 0;
const VOLUME_MAX = 130;
const VOLUME_STEP = 5;

function clampVolume(volume: number): number {
  if (!Number.isFinite(volume)) return VOLUME_MIN;
  return Math.max(VOLUME_MIN, Math.min(VOLUME_MAX, volume));
}

function normalizeWheelDelta(delta: number, deltaMode: number): number {
  switch (deltaMode) {
    case 1:
      return delta / 3;
    case 2:
      return delta;
    default:
      return delta / 100;
  }
}

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

// Index of the entry currently playing. Throws loud errors if invariants are violated.
function currentPos(status: Status): number {
  const n = status.round?.length ?? 0;
  if (n === 0) {
    if (status.phase === 'playing') {
      throw new Error("Durable contract violation: status.round is empty or missing while playing");
    }
    return 0;
  }
  if (status.round_pos === undefined || status.round_pos === null) {
    if (status.phase === 'playing') {
      throw new Error("Durable contract violation: status.round_pos is missing while playing");
    }
    return 0;
  }
  if (status.round_pos < 0 || status.round_pos >= n) {
    throw new Error(`Durable contract violation: status.round_pos (${status.round_pos}) is out of bounds (0-${n - 1})`);
  }
  return status.round_pos;
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
  const [showSettings, setShowSettings] = useState(false);
  const [stats, setStats] = useState<Stats | null>(null);
  const [updateDismissed, setUpdateDismissed] = useState<string | null>(null);
  const [controlsIdle, setControlsIdle] = useState(false);
  const [controlsHovered, setControlsHovered] = useState(false);
  // Volume to flash in the transient OSD, or null when hidden.
  const [volOsd, setVolOsd] = useState<number | null>(null);
  const [displayVolume, setDisplayVolume] = useState(100);
  const [displayPaused, setDisplayPaused] = useState(false);

  useEffect(() => subscribeStatus(setStatus), []);

  // Library/watch stats for the dashboard — refresh on phase change and after
  // each advance (the watched counts move).
  useEffect(() => {
    if (status.phase === 'playing' || status.phase === 'drained' || status.phase === 'fetching') {
      getStats()
        .then(setStats)
        .catch(() => {});
    }
  }, [status.phase, status.last_advance?.advanced_count]);

  // Latest playback, mirrored to refs so mount-once handlers can read current
  // state without re-binding global handlers. volumeRef and displayVolume are
  // optimistic so rapid controls are not gated by stream delivery latency.
  const pbRef = useRef<Playback | undefined>(undefined);
  const volumeRef = useRef(100);
  const pauseRef = useRef(false);
  const volumeSync = useRef<{desired: number | null; inFlight: boolean}>({
    desired: null,
    inFlight: false,
  });
  const pauseSync = useRef<{desired: boolean | null; inFlight: boolean}>({
    desired: null,
    inFlight: false,
  });
  useEffect(() => {
    pbRef.current = status.playback;
    if (status.playback?.volume != null) {
      const currentVolume = clampVolume(status.playback.volume);
      const desired = volumeSync.current.desired;
      if (desired === null || Math.abs(currentVolume - desired) < 0.5) {
        volumeSync.current.desired = null;
        volumeRef.current = currentVolume;
        setDisplayVolume(currentVolume);
      }
    }
    if (status.playback?.paused != null) {
      const currentPaused = status.playback.paused;
      const desired = pauseSync.current.desired;
      if (desired === null || currentPaused === desired) {
        pauseSync.current.desired = null;
        pauseRef.current = currentPaused;
        setDisplayPaused(currentPaused);
      }
    }
  }, [status.playback]);

  // Auto-hide timer for the volume OSD, held in a ref so the mount-once key
  // handler can re-arm it without re-binding.
  const volOsdTimer = useRef<number | undefined>(undefined);
  const wheelVolumeRemainder = useRef(0);

  const flashVolume = useCallback((volume: number) => {
    setVolOsd(volume);
    window.clearTimeout(volOsdTimer.current);
    volOsdTimer.current = window.setTimeout(() => setVolOsd(null), 1400);
  }, []);

  const pumpVolumeQueue = useCallback(() => {
    const sync = volumeSync.current;
    if (sync.inFlight || sync.desired === null) return;

    const sent = sync.desired;
    sync.inFlight = true;
    void sendVolume(sent)
      .catch(() => {
        const current = volumeSync.current;
        if (current.desired !== null && Math.abs(current.desired - sent) < 0.5) {
          current.desired = null;
        }
      })
      .finally(() => {
        const current = volumeSync.current;
        current.inFlight = false;
        if (current.desired !== null && Math.abs(current.desired - sent) >= 0.5) {
          pumpVolumeQueue();
        }
      });
  }, []);

  const requestVolume = useCallback((volume: number, flash = false) => {
    const nextVolume = clampVolume(volume);
    volumeRef.current = nextVolume;
    setDisplayVolume(nextVolume);
    volumeSync.current.desired = nextVolume;
    pumpVolumeQueue();
    if (flash) {
      flashVolume(nextVolume);
    }
  }, [flashVolume, pumpVolumeQueue]);

  const pumpPauseQueue = useCallback(() => {
    const sync = pauseSync.current;
    if (sync.inFlight || sync.desired === null) return;

    const sent = sync.desired;
    sync.inFlight = true;
    pause(sent)
      .catch(() => {
        const current = pauseSync.current;
        if (current.desired !== null && current.desired === sent) {
          current.desired = null;
        }
      })
      .finally(() => {
        const current = pauseSync.current;
        current.inFlight = false;
        if (current.desired !== null && current.desired !== sent) {
          pumpPauseQueue();
        }
      });
  }, []);

  const requestPause = useCallback((paused: boolean) => {
    pauseRef.current = paused;
    setDisplayPaused(paused);
    pauseSync.current.desired = paused;
    pumpPauseQueue();
  }, [pumpPauseQueue]);

  const adjustVolume = useCallback((delta: number) => {
    requestVolume(volumeRef.current + delta, true);
  }, [requestVolume]);

  const handleVolumeWheel: WheelEventHandler<HTMLDivElement> = (event) => {
    if (!pbRef.current) return;

    const rawDelta =
      Math.abs(event.deltaY) >= Math.abs(event.deltaX) ? event.deltaY : event.deltaX;
    if (rawDelta === 0) return;

    event.preventDefault();
    event.stopPropagation();

    wheelVolumeRemainder.current += normalizeWheelDelta(rawDelta, event.deltaMode);
    const steps = Math.trunc(wheelVolumeRemainder.current);
    if (steps === 0) return;

    wheelVolumeRemainder.current -= steps;
    adjustVolume(-steps * VOLUME_STEP);
  };

  // Last physical mouse position to prevent synthetic mousemove events from waking up controls.
  const lastMousePos = useRef<{ x: number; y: number } | null>(null);

  // Keyboard controls. Bound on window so they work whenever the overlay has
  // focus (the WebView2 overlay holds focus over the video). space=pause/play,
  // n=next show, p=previous show, d=defer, f=fullscreen, h=hide all chrome,
  // c=toggle closed captions, ←/→ (or j/l)=seek -/+10s, ↑/↓=volume, v/Tab=dashboard,
  // Esc=hide dashboard.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const isInput = e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement;
      if (isInput && e.key !== 'Escape') {
        return;
      }

      switch (e.key) {
        case ' ':
          e.preventDefault();
          requestPause(!pauseRef.current);
          break;
        case 'n':
          skip();
          break;
        case 'p':
          previous();
          break;
        case 'd':
          defer();
          break;
        case 'f':
          toggleFullscreen();
          break;

        case 'c': {
          e.preventDefault();
          const pb = pbRef.current;
          if (pb && pb.sub_tracks.length > 0) {
            const currentSid = pb.sid;
            const isOff = currentSid === null || currentSid === undefined || currentSid === 'no';
            if (isOff) {
              const firstTrack = pb.sub_tracks[0];
              if (firstTrack) {
                setSub(firstTrack.id);
              }
            } else {
              setSub('no');
            }
          }
          break;
        }
        case 'ArrowLeft':
        case 'j':
          e.preventDefault();
          seekRelative(-10);
          break;
        case 'ArrowRight':
        case 'l':
          e.preventDefault();
          seekRelative(10);
          break;
        case 'ArrowUp': {
          e.preventDefault();
          adjustVolume(VOLUME_STEP);
          break;
        }
        case 'ArrowDown': {
          e.preventDefault();
          adjustVolume(-VOLUME_STEP);
          break;
        }
        case 'Tab':
          e.preventDefault();
          setShowSettings((v) => !v);
          break;
        case 'Escape':
          setShowSettings(false);
          break;
      }
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
      window.clearTimeout(volOsdTimer.current);
    };
  }, [adjustVolume]);



  // Auto-hide the control bar (and cursor) after a short mouse idle during active
  // playback. Any movement brings it back and re-arms the timer.
  useEffect(() => {
    const playing = status.phase === 'playing';
    if (!playing || showSettings || controlsHovered) {
      lastMousePos.current = null;
      setControlsIdle(false);
      return;
    }
    // Default controls to hidden when entering the playback view
    setControlsIdle(true);
    lastMousePos.current = null;
    
    let timer: number | undefined;
    const arm = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => setControlsIdle(true), 2000);
    };
    const onActivity = (e?: MouseEvent) => {
      if (e) {
        const previous = lastMousePos.current;
        const current = { x: e.screenX, y: e.screenY };
        lastMousePos.current = current;
        if (!previous || (current.x === previous.x && current.y === previous.y)) {
          return;
        }
      }
      setControlsIdle(false);
      arm();
    };
    window.addEventListener('mousemove', onActivity);
    arm();
    return () => {
      window.clearTimeout(timer);
      window.removeEventListener('mousemove', onActivity);
    };
  }, [status.phase, showSettings, controlsHovered]);

  useEffect(() => {
    // Re-fetch shows whenever the playlist gains a round or advance — a
    // finished show drops out of the active list after advance. Keyed on
    // advanced_count so unrelated status stream updates don't reload the list.
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
  const overlayVisible = !playing || showSettings;
  const round = status.round ?? [];
  const pos = currentPos(status);
  const selectedShow = shows.find((s) => s.id === selected) ?? null;
  const playingShowName = round.length && pos >= 0 && pos < round.length ? round[pos].show_name : null;
  const activeShowName = selectedShow ? selectedShow.name : playingShowName;
  const activeShowStats = stats?.per_show.find((s) => s.name === activeShowName) ?? null;
  const playingEpisodePath = round.length && pos >= 0 && pos < round.length ? round[pos].absolute_path : null;
  const selectedRoundEntry = round.find((r) => r.show_id === selected);
  const activeEpisodePath = selectedShow
    ? (selectedRoundEntry ? selectedRoundEntry.absolute_path : null)
    : playingEpisodePath;

  // Library edits mutate the replica but don't change phase/advance, so re-fetch
  // the sidebar shows explicitly after add/remove/rescan.
  const refreshShows = () =>
    listShows()
      .then((s) => setShows(s ?? []))
      .catch(() => {});

  const refreshHistory = (showId: string) =>
    listHistory(showId)
      .then((h) => setHistory(h ?? []))
      .catch(() => setHistory([]));

  const handlePlayShow = (showId: string) => {
    playShow(showId);
  };

  const handleMarkWatched = async (showId: string) => {
    try {
      await markShowWatched(showId);
      await refreshShows();
      await refreshHistory(showId);
      getStats().then(setStats).catch(() => {});
    } catch (e) {
      console.error(e);
    }
  };

  const handleMarkUnwatched = async (showId: string) => {
    try {
      await markShowUnwatched(showId);
      await refreshShows();
      await refreshHistory(showId);
      getStats().then(setStats).catch(() => {});
    } catch (e) {
      console.error(e);
    }
  };



  const overviewContent = (
    <>
      <div className="kpi">
        <div className="kpi-cell">
          <div className="kpi-key">round #</div>
          <div className="kpi-val">{status.round_id ?? '—'}</div>
        </div>
        <div className="kpi-cell">
          <div className="kpi-key">round progress</div>
          <div className="kpi-val">
            {round.length ? `${pos + 1}/${round.length}` : '—'}
          </div>
        </div>
        <div className="kpi-cell">
          <div className="kpi-key">watched</div>
          <div className="kpi-val">
            {activeShowStats ? (
              <span
                className={`pill ${
                  activeShowStats.removed || activeShowStats.watched === activeShowStats.total
                    ? 'drain'
                    : 'busy'
                }`}
              >
                {activeShowStats.removed || activeShowStats.watched === activeShowStats.total
                  ? 'yes'
                  : activeShowName === playingShowName
                  ? 'in progress'
                  : 'no'}
              </span>
            ) : (
              '—'
            )}
          </div>
        </div>
        <div className="kpi-cell" style={{ flex: 1, minWidth: 0 }}>
          <div className="kpi-key">episode</div>
          <div
            className="kpi-val"
            style={{
              fontSize: '14.5px',
              lineHeight: '24px',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
              color: 'var(--fg-secondary)',
              fontWeight: 400
            }}
            title={activeEpisodePath || undefined}
          >
            {activeEpisodePath ? shortPath(activeEpisodePath) : '—'}
          </div>
        </div>
      </div>

      {selectedShow ? (
        <ShowHistory
          show={selectedShow}
          events={history}
          onClose={() => setSelected(null)}
          onPlay={() => handlePlayShow(selectedShow.id)}
          onMarkWatched={() => handleMarkWatched(selectedShow.id)}
          onMarkUnwatched={() => handleMarkUnwatched(selectedShow.id)}
          onRemove={() => {
            removeShow(selectedShow.id);
            setSelected(null);
            setTimeout(refreshShows, 400);
          }}
          onRescan={() => {
            void rescanShow(selectedShow.id).then(refreshShows);
          }}
        />
      ) : (
        <>
          <Queue round={round} pos={pos} onSelectShow={setSelected} />
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
                    <tr key={r.id} onClick={() => setSelected(r.id)} style={{ cursor: 'pointer' }}>
                      <td>{r.name}</td>
                      <td style={{ color: 'var(--fg-dim)' }} >{relTime(r.date_added)}</td>
                      <td style={{ color: 'var(--fg-dim)' }}>{durationDays(r.date_added, r.last_played_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
          <StatsPanel stats={stats} />
        </>
      )}
    </>
  );

  return (
    <div className={`overlay-root${controlsIdle ? ' cursor-hidden' : ''}${status.window_maximized ? ' window-maximized' : ''}${status.window_fullscreen ? ' window-fullscreen' : ''}`}>
      {!status.window_fullscreen && (
        <div className="titlebar">
          <div className="titlebar-logo">
            <img src="/favicon.ico" className="titlebar-logo-img" alt="" />
            <span>shows</span>
          </div>
          <div className="titlebar-title">{status.playlist ? `shows — ${status.playlist}` : 'shows'}</div>
          <div className="titlebar-actions">
            <button className="titlebar-btn min-btn" onClick={minimizeWindow} title="Minimize">
              <svg viewBox="0 0 10 10"><path d="M0 5h10v1H0z" fill="currentColor" /></svg>
            </button>
            <button className="titlebar-btn max-btn" onClick={maximizeWindow} title={status.window_maximized ? "Restore" : "Maximize"}>
              {status.window_maximized ? (
                <svg viewBox="0 0 10 10"><path d="M2 0v2H0v8h8V8h2V0H2zM7 9H1V3h6v6zm2-2H8V2H3V1h6v6z" fill="currentColor" /></svg>
              ) : (
                <svg viewBox="0 0 10 10"><path d="M0 0v10h10V0H0zm9 9H1V1h8v8z" fill="currentColor" /></svg>
              )}
            </button>
            <button className="titlebar-btn close-btn" onClick={closeWindow} title="Close">
              <svg viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10" stroke="currentColor" strokeWidth="1.2" fill="none" /></svg>
            </button>
          </div>
        </div>
      )}
      <div
        style={{
          position: 'absolute',
          inset: 0,
          pointerEvents: 'auto',
          zIndex: -1,
        }}
        onDoubleClick={() => toggleFullscreen()}
        onWheel={handleVolumeWheel}
      />
      <VolumeOsd volume={volOsd} />
      {status.update?.available && status.update.latest !== updateDismissed && (
        <UpdateBanner
          info={status.update}
          onDismiss={() => setUpdateDismissed(status.update!.latest)}
        />
      )}
      {overlayVisible ? (
        <div className={`layout${playing ? ' over-video' : ''}`}>


          <main className="main">
            <SettingsPanel
              status={status}
              shows={shows}
              selected={selected}
              setSelected={setSelected}
              refreshShows={refreshShows}
              removeShow={removeShow}
              overviewContent={overviewContent}
            />
          </main>
        </div>
      ) : (
        <div
          style={{ flex: 1 }}
          onDoubleClick={() => toggleFullscreen()}
          onWheel={handleVolumeWheel}
        />
      )}
      <BottomControlBar
        status={status}
        pos={pos}
        playing={playing}
        viewing={showSettings}
        onToggleView={() => {
          setShowSettings((v) => !v);
        }}
        viewingSettings={showSettings}
        onToggleSettings={() => {
          setShowSettings((v) => !v);
        }}
        controlsIdle={controlsIdle}
        onHoverChange={setControlsHovered}
        volume={displayVolume}
        onVolumeChange={requestVolume}
        onVolumeWheel={handleVolumeWheel}
        displayPaused={displayPaused}
        onRequestPause={requestPause}
      />
    </div>
  );
}

// A dismissible banner shown when the launch update-check finds a newer release.
function UpdateBanner({ info, onDismiss }: { info: UpdateInfo; onDismiss: () => void }) {
  return (
    <div
      className="updatebar"
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 10,
        padding: '6px 12px',
        background: 'rgba(18,18,18,0.94)',
        borderBottom: '1px solid var(--border, #333)',
        fontSize: 13,
        color: 'var(--fg-secondary, #ccc)',
      }}
    >
      <span className="pill info">update</span>
      <span style={{ flex: 1 }}>
        A newer build is available — <strong>{info.latest}</strong>
        {info.current ? ` (you're on ${info.current})` : ''}.
      </span>
      {info.url && (
        <a className="gb" href={info.url} target="_blank" rel="noreferrer">
          releases ↗
        </a>
      )}
      <button className="gb" onClick={onDismiss} title="dismiss">
        ✕
      </button>
    </div>
  );
}

// SVG Icons for bottom controls
const PlayIcon = () => (
  <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
    <path d="M8 5v14l11-7z" />
  </svg>
);

const PauseIcon = () => (
  <svg viewBox="0 0 24 24" width="20" height="20" fill="currentColor">
    <path d="M6 19h4V5H6v14zm8-14v14h4V5h-4z" />
  </svg>
);

const PrevIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
    <path d="M6 6h2v12H6zm3.5 6l8.5 6V6z" />
  </svg>
);

const NextIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
    <path d="M6 18l8.5-6L6 6v12zM16 6v12h2V6z" />
  </svg>
);

const RewindIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
    <path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
    <polyline points="3 3 3 8 8 8" />
    <text x="12" y="15" fontSize="8" fontWeight="bold" fontFamily="sans-serif" textAnchor="middle" fill="currentColor" stroke="none">10</text>
  </svg>
);

const ForwardIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21 12a9 9 0 1 1-9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" />
    <polyline points="21 3 21 8 16 8" />
    <text x="12" y="15" fontSize="8" fontWeight="bold" fontFamily="sans-serif" textAnchor="middle" fill="currentColor" stroke="none">10</text>
  </svg>
);

const DeferIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="10" />
    <polyline points="12 6 12 12 16 14" />
  </svg>
);

const CcIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
    <path d="M19 4H5c-1.11 0-2 .9-2 2v12c0 1.1.89 2 2 2h14c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2zm-8 7H9.5v-.5h-2v3h2V13H11v1c0 .55-.45 1-1 1H6c-.55 0-1-.45-1-1V10c0-.55.45-1 1-1h4c.55 0 1 .45 1 1v1zm7 0h-1.5v-.5h-2v3h2V13H18v1c0 .55-.45 1-1 1h-4c-.55 0-1-.45-1-1V10c0-.55.45-1 1-1h4c.55 0 1 .45 1 1v1z" />
  </svg>
);

const VolumeIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
    <path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z" />
  </svg>
);

const VolumeMuteIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="currentColor">
    <path d="M16.5 12c0-1.77-1.02-3.29-2.5-4.03v2.21l2.45 2.45c.03-.21.05-.42.05-.63zm2.5 0c0 .94-.2 1.82-.54 2.64l1.51 1.51C20.63 14.91 21 13.5 21 12c0-4.28-2.99-7.86-7-8.77v2.06c2.89.86 5 3.54 5 6.71zM4.27 3L3 4.27 7.73 9H3v6h4l5 5v-6.73l4.25 4.25c-.67.52-1.42.93-2.25 1.18v2.06c1.38-.31 2.63-.95 3.69-1.81L19.73 21 21 19.73l-9-9L4.27 3zM12 4L9.91 6.09 12 8.18V4z" />
  </svg>
);

const SyncIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M21.5 2v6h-6M21.34 15.57a10 10 0 1 1-.57-8.38l5.67-5.67" />
  </svg>
);



const PlaylistIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <line x1="8" y1="6" x2="21" y2="6" />
    <line x1="8" y1="12" x2="21" y2="12" />
    <line x1="8" y1="18" x2="21" y2="18" />
    <line x1="3" y1="6" x2="3.01" y2="6" />
    <line x1="3" y1="12" x2="3.01" y2="12" />
    <line x1="3" y1="18" x2="3.01" y2="18" />
  </svg>
);

const FullscreenIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3" />
  </svg>
);

const SettingsIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
    <circle cx="12" cy="12" r="3" />
  </svg>
);


function fmtTime(s: number | null | undefined): string {
  if (s == null || isNaN(s)) return '--:--';
  const t = Math.max(0, Math.floor(s));
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m);
  return (h > 0 ? `${h}:` : '') + `${mm}:${String(sec).padStart(2, '0')}`;
}

// Transient volume indicator. Flashes bottom-center on ↑/↓ and auto-hides —
// the only volume feedback when chrome ('h') or the dashboard is hidden and the
// PlaybackBar's slider isn't on screen. Display-only (pointer-events:none); the
// bar fills against mpv's 0–130 range, so 100% sits short of full (boost room).
function VolumeOsd({ volume }: { volume: number | null }) {
  if (volume === null) return null;
  return (
    <div className="vol-osd" role="status" aria-live="polite">
      <span className="vol-osd-label">vol</span>
      <div className="vol-osd-bar">
        <div className="vol-osd-fill" style={{ width: `${(volume / 130) * 100}%` }} />
      </div>
      <span className="vol-osd-num">{Math.round(volume)}</span>
    </div>
  );
}

// Unified bottom-anchored control bar.
function BottomControlBar({
  status,
  pos,
  playing,
  viewing,
  onToggleView,
  viewingSettings,
  onToggleSettings,
  controlsIdle,
  onHoverChange,
  volume,
  onVolumeChange,
  onVolumeWheel,
  displayPaused,
  onRequestPause,
}: {
  status: Status;
  pos: number;
  playing: boolean;
  viewing: boolean;
  onToggleView: () => void;
  viewingSettings: boolean;
  onToggleSettings: () => void;
  controlsIdle: boolean;
  onHoverChange: (hovered: boolean) => void;
  volume: number;
  onVolumeChange: (volume: number, flash?: boolean) => void;
  onVolumeWheel: WheelEventHandler<HTMLDivElement>;
  displayPaused: boolean;
  onRequestPause: (paused: boolean) => void;
}) {
  const pb = status.playback;
  const pct = pb
    ? pb.percent_pos ?? (pb.duration && pb.time_pos != null ? (pb.time_pos / pb.duration) * 100 : 0)
    : 0;

  const [lastVolume, setLastVolume] = useState(100);

  const handleToggleMute = () => {
    if (!pb) return;
    const currentVol = volume;
    if (currentVol > 0) {
      setLastVolume(currentVol);
      onVolumeChange(0, true);
    } else {
      onVolumeChange(lastVolume, true);
    }
  };

  const handleToggleCc = () => {
    if (pb && pb.sub_tracks.length > 0) {
      const currentSid = pb.sid;
      const isOff = currentSid === null || currentSid === undefined || currentSid === 'no';
      if (isOff) {
        const firstTrack = pb.sub_tracks[0];
        if (firstTrack) {
          setSub(firstTrack.id);
        }
      } else {
        setSub('no');
      }
    }
  };

  const round = status.round ?? [];
  const nowPlayingText = round.length
    ? `${round[pos].show_name}   (${pos + 1}/${round.length})`
    : status.message || '—';

  const isMuted = pb ? volume === 0 : false;
  const ccActive = pb ? pb.sid !== 'no' && pb.sid != null : false;

  return (
    <div
      className={`bottom-controls${controlsIdle ? ' hidden' : ''}`}
      onMouseEnter={() => onHoverChange(true)}
      onMouseLeave={() => onHoverChange(false)}
      onWheel={onVolumeWheel}
    >
      {/* 1. Scrub Container */}
      <div className="scrub-container">
        <span className="time-display">{pb ? fmtTime(pb.time_pos) : '--:--'}</span>
        <div
          className={`scrub-bar${!pb ? ' disabled' : ''}`}
          onClick={(e) => {
            if (!pb) return;
            const r = e.currentTarget.getBoundingClientRect();
            seekPercent(Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100)));
          }}
        >
          <div className="scrub-fill" style={{ width: `${pct}%` }} />
          <div className="scrub-handle" style={{ left: `${pct}%` }} />
        </div>
        <span className="time-display">{pb ? fmtTime(pb.duration) : '--:--'}</span>
      </div>

      {/* 2. Controls Row */}
      <div className="controls-row">
        {/* Left: Info digital display */}
        <div className="controls-group left-display">
          <div className="hud-display">
            <div className="hud-status">
              {status.playlist && (
                <span className="hud-playlist" style={{ fontSize: '10px', color: 'var(--fg-dim)', textTransform: 'uppercase', letterSpacing: '0.05em' }}>
                  {status.playlist}
                </span>
              )}
            </div>
            <div className="display-now-playing" title={nowPlayingText}>
              {nowPlayingText}
            </div>
            {pb && !pb.paused && (pb.core_idle || pb.paused_for_cache) && (
              <div className="hud-buffering" style={{ fontSize: '10px', color: 'var(--fg-dim)', marginLeft: '12px', animation: 'pulse 1s infinite' }}>
                BUFFERING
              </div>
            )}
          </div>
        </div>

        {/* Center: Playback controls */}
        <div className="controls-group center-controls">
          <button
            className="control-btn"
            onClick={() => previous()}
            disabled={!playing}
            title="Previous Show (p)"
          >
            <PrevIcon />
          </button>
          
          <button
            className="control-btn"
            onClick={() => seekRelative(-10)}
            disabled={!pb}
            title="Rewind 10s (j / ←)"
          >
            <RewindIcon />
          </button>

          <button
            className="control-btn play-pause-btn"
            onClick={() => onRequestPause(!displayPaused)}
            disabled={!playing}
            title="Play / Pause (Space)"
          >
            {displayPaused ? <PlayIcon /> : <PauseIcon />}
          </button>

          <button
            className="control-btn"
            onClick={() => seekRelative(10)}
            disabled={!pb}
            title="Forward 10s (l / →)"
          >
            <ForwardIcon />
          </button>

          <button
            className="control-btn"
            onClick={() => skip()}
            disabled={!playing}
            title="Skip Show (n)"
          >
            <NextIcon />
          </button>
        </div>

        {/* Right: Sound, Selectors, Sync, Fullscreen, View, Hide */}
        <div className="controls-group right-controls">
          <button
            className="control-btn defer-btn"
            onClick={() => defer()}
            disabled={!playing}
            title="Defer Episode (d)"
          >
            <DeferIcon />
          </button>

          {pb && pb.sub_tracks.length > 0 && (
            <button
              className={`control-btn cc-btn${ccActive ? ' active' : ''}`}
              onClick={handleToggleCc}
              title="Toggle Captions (c)"
            >
              <CcIcon />
            </button>
          )}

          <div className="volume-control-group">
            <button
              className="control-btn volume-btn"
              onClick={handleToggleMute}
              disabled={!pb}
              title="Mute / Unmute (Up/Down Arrows or mouse wheel to adjust)"
            >
              {isMuted ? <VolumeMuteIcon /> : <VolumeIcon />}
            </button>
            <input
              type="range"
              min={0}
              max={130}
              value={Math.round(volume)}
              disabled={!pb}
              className="volume-slider"
              title="Volume (Up/Down Arrows or mouse wheel)"
              onChange={(e) => onVolumeChange(Number(e.currentTarget.value))}
            />
          </div>

          {pb && pb.sub_tracks.length > 0 && (
            <select
              className="track-select"
              title="Subtitle Track"
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

          {pb && pb.audio_tracks.length > 1 && (
            <select
              className="track-select"
              title="Audio Track"
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

          <button
            className="control-btn sync-btn"
            onClick={() => syncNow()}
            title="Sync Now"
          >
            <SyncIcon />
          </button>

          {playing && (
            <button
              className={`control-btn playlist-btn${viewing && !viewingSettings ? ' active' : ''}`}
              onClick={onToggleView}
              title="Toggle Playlist (v / Tab)"
            >
              <PlaylistIcon />
            </button>
          )}

          <button
            className={`control-btn settings-btn${viewingSettings ? ' active' : ''}`}
            onClick={onToggleSettings}
            title="Settings & Library Management"
          >
            <SettingsIcon />
          </button>

          <button
            className="control-btn fullscreen-btn"
            onClick={() => toggleFullscreen()}
            title="Fullscreen (f)"
          >
            <FullscreenIcon />
          </button>
        </div>
      </div>
    </div>
  );
}

// Flat "now playing / up next" queue. Entries are in play order; the current
// one is marked, earlier ones are done, later ones are what's next.
function Queue({ round, pos, onSelectShow }: { round: Status['round']; pos: number; onSelectShow: (showId: string) => void }) {
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
          <li
            key={r.episode_id}
            className={i === pos ? 'now' : i < pos ? 'done' : 'next'}
            onClick={() => onSelectShow(r.show_id)}
          >
            <span className="q-mark">{i < pos ? '✓' : ''}</span>
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
  onPlay,
  onMarkWatched,
  onMarkUnwatched,
  onRemove,
  onRescan,
}: {
  show: Show;
  events: HistoryEvent[];
  onClose: () => void;
  onPlay: () => void;
  onMarkWatched: () => void;
  onMarkUnwatched: () => void;
  onRemove: () => void;
  onRescan: () => void;
}) {
  return (
    <div className="section">
      <h3>
        history — {show.name}
        <button className="gb" style={{ marginLeft: 12 }} onClick={onClose} title="Back (Escape)">
          back
        </button>
        <button className="gb" style={{ marginLeft: 8 }} onClick={onPlay}>
          play show
        </button>
        <button
          className="gb"
          style={{ marginLeft: 8 }}
          onClick={onMarkWatched}
          disabled={!!show.removed_at}
        >
          mark watched
        </button>
        <button
          className="gb"
          style={{ marginLeft: 8 }}
          onClick={onMarkUnwatched}
          disabled={!show.removed_at}
        >
          mark unwatched
        </button>
        <button className="gb" style={{ marginLeft: 8 }} onClick={onRescan}>
          rescan
        </button>
        <button className="gb danger" style={{ marginLeft: 8 }} onClick={onRemove}>
          remove show
        </button>
      </h3>
      <div className="meta" style={{ margin: '0 0 12px' }}>
        {shortPath(show.root_path)}
      </div>
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

// Add a show by pointing at a local folder; the desktop scans it for episodes.
function AddShowForm({ playlist, onAdded }: { playlist: string; onAdded: () => void }) {
  const [name, setName] = useState('');
  const [path, setPath] = useState('');
  const [pl, setPl] = useState(playlist || 'nelson');
  const [msg, setMsg] = useState('');
  const [busy, setBusy] = useState(false);
  const [previewData, setPreviewData] = useState<string[] | null>(null);

  const handlePreview = async () => {
    const cleanPath = path.trim().replace(/^["']|["']$/g, '');
    if (!name.trim() || !cleanPath) {
      setMsg('name and folder are required');
      return;
    }
    setBusy(true);
    setMsg('scanning…');
    try {
      const episodes = await previewShow(cleanPath);
      setPreviewData(episodes);
      setMsg('');
    } catch (e: any) {
      setMsg(String(e.message || e));
    } finally {
      setBusy(false);
    }
  };

  const handleAdd = () => {
    const cleanPath = path.trim().replace(/^["']|["']$/g, '');
    setBusy(true);
    setMsg('saving…');
    addShow(name.trim(), cleanPath, (pl || 'nelson').trim())
      .then((r) => {
        setMsg(`added ${r.episodes} episode${r.episodes === 1 ? '' : 's'}`);
        setName('');
        setPath('');
        setPreviewData(null);
        setTimeout(() => {
          setMsg('');
        }, 1500);
        onAdded();
      })
      .catch((e) => setMsg(String(e.message || e)))
      .finally(() => setBusy(false));
  };

  return (
    <div className="add-form" style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
      <input placeholder="show name" value={name} onChange={(e) => setName(e.target.value)} disabled={!!previewData} />
      <div style={{ display: 'flex', gap: '8px' }}>
        <input
          placeholder="folder path"
          value={path}
          onChange={(e) => setPath(e.target.value)}
          style={{ flex: 1 }}
          disabled={!!previewData}
        />
        {!previewData && (
          <button className="gb" disabled={busy} onClick={() => pickFolder().then(p => { if (p) setPath(p); })}>
            browse
          </button>
        )}
      </div>
      <input placeholder="playlist" value={pl} onChange={(e) => setPl(e.target.value)} disabled={!!previewData} />
      
      {previewData && (
        <div style={{ marginTop: '16px', marginBottom: '16px' }}>
          <h4 style={{ margin: '0 0 8px 0', fontSize: '13px', color: 'var(--fg)' }}>
            Found {previewData.length} episode{previewData.length === 1 ? '' : 's'}
          </h4>
          <div style={{ maxHeight: '200px', overflowY: 'auto', border: '1px solid var(--border)', borderRadius: '4px' }}>
            <ul className="queue">
              {previewData.map((epPath, i) => (
                <li key={i} className="next">
                  <span className="q-mark" style={{ opacity: 0.5 }}>{i + 1}</span>
                  <span className="q-ep">{epPath}</span>
                </li>
              ))}
            </ul>
          </div>
        </div>
      )}

      <div className="add-actions" style={{ display: 'flex', gap: '8px', marginTop: '8px' }}>
        {!previewData ? (
          <button className="gb" disabled={busy} onClick={handlePreview}>
            preview
          </button>
        ) : (
          <button className="gb" disabled={busy} onClick={handleAdd}>
            confirm add
          </button>
        )}
        <button
          className="gb"
          onClick={() => {
            setName('');
            setPath('');
            setPreviewData(null);
            setMsg('');
          }}
        >
          clear
        </button>
      </div>
      {msg && <div className="add-msg">{msg}</div>}
    </div>
  );
}

function Heatmap({ byDay }: { byDay: Record<string, number> }) {
  const today = new Date();
  const cells = [];
  for (let i = 97; i >= 0; i--) {
    const d = new Date(today);
    d.setDate(today.getDate() - i);
    const key = d.toISOString().slice(0, 10);
    const c = byDay[key] || 0;
    const level = c === 0 ? 0 : c < 2 ? 1 : c < 4 ? 2 : c < 7 ? 3 : 4;
    cells.push(<span key={key} className="hm-cell" data-l={level} title={`${key}: ${c} watched`} />);
  }
  return <div className="heatmap">{cells}</div>;
}

// Library + watch stats: totals, a watch heatmap, per-show progress, recent.
function StatsPanel({ stats }: { stats: Stats | null }) {
  if (!stats || !stats.total_shows) return null;
  const pct = stats.episodes_total
    ? Math.round((stats.episodes_watched / stats.episodes_total) * 100)
    : 0;
  const active = stats.per_show.filter((s) => !s.removed).slice(0, 14);
  return (
    <div className="section">
      <h3>stats</h3>
      <div className="kpi">
        <div className="kpi-cell">
          <div className="kpi-key">episodes watched</div>
          <div className="kpi-val">
            {stats.episodes_watched} / {stats.episodes_total} ({pct}%)
          </div>
        </div>
        <div className="kpi-cell">
          <div className="kpi-key">shows finished</div>
          <div className="kpi-val">{stats.finished_shows}</div>
        </div>
        <div className="kpi-cell">
          <div className="kpi-key">active shows</div>
          <div className="kpi-val">{stats.active_shows}</div>
        </div>
      </div>
      <Heatmap byDay={stats.by_day} />
      <h4 className="stat-h">progress</h4>
      <ul className="progress">
        {active.map((s) => (
          <li key={s.name}>
            <span className="p-name">{s.name}</span>
            <span className="p-bar">
              <span
                className="p-fill"
                style={{ width: `${s.total ? (s.watched / s.total) * 100 : 0}%` }}
              />
            </span>
            <span className="p-num">
              {s.watched}/{s.total}
            </span>
          </li>
        ))}
      </ul>
      {stats.recent.length > 0 && (
        <>
          <h4 className="stat-h">recent</h4>
          <ul className="recent">
            {stats.recent.slice(0, 10).map((r, i) => (
              <li key={i}>
                <span className="rc-show">{r.show}</span>
                <span className="rc-ep">{shortPath(r.relative_path)}</span>
                <span className="rc-when">{relTime(r.played_at)}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
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

function SettingsPanel({
  status,
  shows,
  selected,
  setSelected,
  refreshShows,
  removeShow,
  overviewContent,
}: {
  status: Status;
  shows: Show[];
  selected: string | null;
  setSelected: (id: string | null) => void;
  refreshShows: () => void;
  removeShow: (id: string) => void;
  overviewContent: React.ReactNode;
}) {
  const [activeTab, setActiveTab] = useState<'overview' | 'library' | 'add_show' | 'next_round' | 'general' | 'appearance'>('overview');

  return (
    <div className="settings-panel">
      <div className="settings-sidebar">
        <h3>settings</h3>
        <nav className="settings-nav">
          <button className={`nav-btn${activeTab === 'overview' ? ' active' : ''}`} onClick={() => setActiveTab('overview')}>overview</button>
          <button className={`nav-btn${activeTab === 'library' ? ' active' : ''}`} onClick={() => setActiveTab('library')}>library</button>
          <button className={`nav-btn${activeTab === 'add_show' ? ' active' : ''}`} onClick={() => setActiveTab('add_show')}>add show</button>
          <button className={`nav-btn${activeTab === 'next_round' ? ' active' : ''}`} onClick={() => setActiveTab('next_round')}>next round</button>
          <button className={`nav-btn${activeTab === 'general' ? ' active' : ''}`} onClick={() => setActiveTab('general')}>general</button>
          <button className={`nav-btn${activeTab === 'appearance' ? ' active' : ''}`} onClick={() => setActiveTab('appearance')}>appearance</button>
        </nav>
      </div>

      <div className="settings-content">
        {activeTab === 'overview' && (
          <div className="settings-tab">
            {overviewContent}
          </div>
        )}

        {activeTab === 'library' && (
          <div className="settings-tab">
            <div className="section">
              <h3>library ({shows.length} shows)</h3>
              {shows.length === 0 ? (
                <div className="empty">no shows yet.</div>
              ) : (
                <ul className="queue">
                  {shows.map((sh) => (
                    <li
                      key={sh.id}
                      className={`next ${selected === sh.id ? 'now' : ''}`}
                      onClick={() => {
                        setSelected(sh.id);
                        setActiveTab('overview');
                      }}
                      style={{ cursor: 'pointer' }}
                    >
                      <span className="q-mark" style={{ opacity: 0.5 }}></span>
                      <span className="q-show">{sh.name}</span>
                      <span className="q-ep" style={{ textAlign: 'right' }}>{relTime(sh.date_added)}</span>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          </div>
        )}

        {activeTab === 'next_round' && <NextRoundTab currentRound={status.round || []} onSelectShow={(id) => { setSelected(id); setActiveTab('overview'); }} />}

        {activeTab === 'add_show' && (
          <div className="settings-tab">
            <div className="section">
              <h3>add show</h3>
              <AddShowForm playlist={status.playlist} onAdded={refreshShows} />
            </div>
          </div>
        )}

        {activeTab === 'general' && (
          <div className="settings-tab">
            <div className="section">
              <h3>general</h3>
              <div className="empty" style={{ padding: '24px 0' }}>general settings coming soon...</div>
            </div>
          </div>
        )}

        {activeTab === 'appearance' && (
          <div className="settings-tab">
            <div className="section">
              <h3>appearance</h3>
              <div className="empty" style={{ padding: '24px 0' }}>appearance settings coming soon...</div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default App;


function NextRoundTab({ currentRound, onSelectShow }: { currentRound: any[], onSelectShow: (showId: string) => void }) {
  const [round, setRound] = useState<NextRoundEpisode[] | null>(null);
  const [msg, setMsg] = useState('loading next round...');

  useEffect(() => {
    let active = true;
    getNextRound()
      .then((res) => {
        if (active) {
          setRound(res);
          setMsg('');
        }
      })
      .catch((e) => {
        if (active) setMsg(String(e.message || e));
      });
    return () => {
      active = false;
    };
  }, []);

  return (
    <div className="settings-tab">
      <div className="section">
        <h3>preview next round</h3>
        <p style={{ opacity: 0.7, marginBottom: '16px' }}>
          this is what the next round of episodes would look like if it was generated right now.
        </p>
        {msg && <div className="add-msg">{msg}</div>}
        {round && round.length === 0 && !msg && <div className="empty">no episodes found.</div>}
        {round && round.length > 0 && (
          <ul className="queue">
            {round.map((ep, i) => {
              const isNew = !currentRound.some((r: any) => r.show_id === ep.show_id);
              return (
                <li key={ep.episode_id} onClick={() => onSelectShow(ep.show_id)} className={`next ${isNew ? 'new-show' : ''}`} style={{ cursor: 'pointer' }}>
                  <span className="q-mark" style={{ opacity: 0.5 }}>{i + 1}</span>
                  <span className="q-show">{ep.show_name}</span>
                  <span className="q-ep">{shortPath(ep.relative_path)}</span>
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>
  );
}

