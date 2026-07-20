import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type WheelEventHandler } from 'react';
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
  setStayOnTop,
  minimizeWindow,
  maximizeWindow,
  closeWindow,
  beginWindowDrag,
  seekPercent,
  seekRelative,
  setVolume as sendVolume,
  setSub,
  setAudio,
  removeRoundEntry,
  reloadRound,
  addShow,
  previewShow,
  detectNewFolders,
  detectNewEpisodes,
  type ShowNewEpisodes,
  pickFolder,
  getNextRound,
  type NextRoundEpisode,
  removeShow,
  rescanShow,
  rescanWatchedShow,
  type Status,
  type Show,
  type HistoryEvent,
  type Playback,
  type Stats,
  type UpdateInfo,
  type ControlResult,
  fetchShowDetails,
  type ShowDetailsResponse,
} from './api';
import {PinSyncController} from './pinSync';
import './App.css';

const VOLUME_MIN = 0;
const VOLUME_MAX = 130;
const VOLUME_STEP = 5;
const CONTROLS_IDLE_MS = 2000;
const PIN_ACK_TIMEOUT_MS = 1500;

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
  // Advances for every full status snapshot, even when an individual field has
  // the same primitive value. Shell-state heartbeats use same-value snapshots
  // as acknowledgements after a coalesced pin/unpin edge.
  const [statusRevision, setStatusRevision] = useState(0);
  const [shows, setShows] = useState<Show[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [showDetails, setShowDetails] = useState<ShowDetailsResponse | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const [stats, setStats] = useState<Stats | null>(null);
  const [updateDismissed, setUpdateDismissed] = useState<string | null>(null);
  const [controlsIdle, setControlsIdle] = useState(false);
  const [controlsHovered, setControlsHovered] = useState(false);
  const [controlsPointerDown, setControlsPointerDown] = useState(false);
  // Volume to flash in the transient OSD, or null when hidden.
  const [volOsd, setVolOsd] = useState<number | null>(null);
  const [displayVolume, setDisplayVolume] = useState(100);
  const [displayPaused, setDisplayPaused] = useState(false);
  const [controlToast, setControlToast] = useState<{message: string; level: 'info' | 'danger'} | null>(null);
  const [repairedRoundEntries, setRepairedRoundEntries] = useState<Set<string>>(() => new Set());

  useEffect(() => subscribeStatus((nextStatus) => {
    setStatus(nextStatus);
    setStatusRevision((revision) => revision + 1);
  }), []);

  const showControlToast = useCallback((message: string, level: 'info' | 'danger' = 'info') => {
    setControlToast({message, level});
  }, []);

  useEffect(() => {
    if (!controlToast) return;
    const id = window.setTimeout(() => setControlToast(null), 2600);
    return () => window.clearTimeout(id);
  }, [controlToast]);

  const runControl = useCallback(
    async (action: () => Promise<ControlResult>, options?: {success?: boolean}) => {
      try {
        const result = await action();
        if (options?.success) {
          showControlToast(result.message, 'info');
        }
        return true;
      } catch (e) {
        const message = e instanceof Error ? e.message : 'control failed';
        showControlToast(message, 'danger');
        console.error(e);
        return false;
      }
    },
    [showControlToast],
  );

  // Library/watch stats for the dashboard — refresh on phase change and after
  // each advance (the watched counts move).
  useEffect(() => {
    if (status.phase === 'playing' || status.phase === 'drained' || status.phase === 'fetching') {
      getStats()
        .then(setStats)
        .catch(() => {});
    }
  }, [status.phase, status.last_advance?.advanced_count, status.database?.revision]);

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
  const controlsIdleTimer = useRef<number | undefined>(undefined);
  const wheelVolumeRemainder = useRef(0);
  const controlsIdleRef = useRef(controlsIdle);

  useEffect(() => {
    controlsIdleRef.current = controlsIdle;
  }, [controlsIdle]);

  const flashVolume = useCallback((volume: number) => {
    setVolOsd(volume);
    window.clearTimeout(volOsdTimer.current);
    volOsdTimer.current = window.setTimeout(() => setVolOsd(null), 1400);
  }, []);

  const armControlsIdle = useCallback((delayMs = CONTROLS_IDLE_MS) => {
    window.clearTimeout(controlsIdleTimer.current);
    controlsIdleTimer.current = window.setTimeout(() => {
      setControlsHovered(false);
      setControlsIdle(true);
    }, delayMs);
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
    if (flash && controlsIdleRef.current) {
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

  // Pin-mode window dragging: in pin mode the titlebar is hidden, so a press and
  // drag on the bare video surface moves the window. We only start the native
  // move once the pointer travels past a small threshold, so plain clicks and
  // double-clicks (e.g. toggle fullscreen) still work. The host owns the actual
  // move loop; the overlay just decides the gesture began on draggable surface.
  const surfaceDragStart = useRef<{ x: number; y: number } | null>(null);
  // Two distinct notions of "pinned", deliberately kept separate:
  //  - `onTopRef` / `pinned`: the optimistic INTENT — drives the button
  //    highlight and the direction of the next toggle command. Updated
  //    immediately on click so the control feels instant, and rolled back to
  //    host truth if a command is lost (see pumpOnTopQueue).
  //  - `hostOnTopRef`: the most recently echoed HOST-CONFIRMED truth. It is
  //    used for failed-command rollback; native code authorizes surface drag
  //    from its live `is_on_top`, so an SSE delay cannot remove both handles.
  const onTopRef = useRef(false);
  const hostOnTopRef = useRef(false);
  const [pinned, setPinned] = useState(false);
  const setPinnedOptimistic = useCallback((next: boolean) => {
    onTopRef.current = next;
    setPinned(next);
  }, []);
  const surfaceDragProps = {
    onPointerDown: (e: ReactPointerEvent) => {
      if (e.button !== 0) return;
      surfaceDragStart.current = { x: e.screenX, y: e.screenY };
    },
    onPointerMove: (e: ReactPointerEvent) => {
      const start = surfaceDragStart.current;
      if (!start) return;
      if (Math.abs(e.screenX - start.x) + Math.abs(e.screenY - start.y) > 4) {
        surfaceDragStart.current = null;
        void beginWindowDrag();
      }
    },
    onPointerUp: () => {
      surfaceDragStart.current = null;
    },
    onPointerCancel: () => {
      surfaceDragStart.current = null;
    },
  };

  // Pin (stay-on-top) follows ADR-0001's optimistic + idempotent-set +
  // reconcile pattern, with one correction the pause/volume controls don't
  // need: shell truth is re-echoed by the compositor heartbeat so a missed edge
  // self-heals. We therefore (a) keep ONE optimistic value
  // (`pinned`/`onTopRef`) as the single source for both highlight and command
  // direction, (b) roll it back to host truth when a command is lost, and (c)
  // track the last host-confirmed value in `hostOnTopRef` for rollback.
  const onTopSync = useRef(new PinSyncController());

  // Mirror the host-confirmed pin state into a ref the instant it is echoed, so
  // failure-rollback and the surface-drag gate always read current host truth.
  useEffect(() => {
    if (status.window_on_top != null) hostOnTopRef.current = Boolean(status.window_on_top);
  }, [status.window_on_top]);

  useEffect(() => {
    if (status.window_on_top == null) return;
    const observed = Boolean(status.window_on_top);
    const reconciled = onTopSync.current.observe(observed);
    if (reconciled !== null) setPinnedOptimistic(reconciled);
  }, [status.window_on_top, statusRevision, setPinnedOptimistic]);

  const pumpOnTopQueue = useCallback(() => {
    const sync = onTopSync.current;
    const request = sync.dispatch();
    if (request === null) return;

    const settle = (succeeded: boolean) => {
      const outcome = onTopSync.current.settle(request, succeeded);
      if (outcome.rollback) {
        // Command lost: undo the optimistic advance back to the host's
        // last-confirmed state. Without this the next click computes the
        // wrong direction (a silent no-op against the idempotent host) and
        // the surface-drag gate desyncs from the real caption state.
        setPinnedOptimistic(hostOnTopRef.current);
        showControlToast('pin failed', 'danger');
      }
      if (outcome.awaitAck) {
        window.setTimeout(() => {
          const reconciled = onTopSync.current.expireAcknowledgement(request);
          if (reconciled !== null) setPinnedOptimistic(reconciled);
        }, PIN_ACK_TIMEOUT_MS);
      }
      if (outcome.pump) pumpOnTopQueue();
    };

    void setStayOnTop(request.value).then(
      () => settle(true),
      () => settle(false),
    );
  }, [setPinnedOptimistic, showControlToast]);

  const requestStayOnTop = useCallback((onTop: boolean) => {
    setPinnedOptimistic(onTop);
    onTopSync.current.queue(onTop);
    pumpOnTopQueue();
  }, [pumpOnTopQueue, setPinnedOptimistic]);

  // Button and `i` keybind both route through the desired-state queue. Two real
  // rapid clicks are two intents: the second supersedes the first and is pumped
  // after the in-flight request, returning to the starting state.
  const togglePin = useCallback(() => {
    requestStayOnTop(!onTopRef.current);
  }, [requestStayOnTop]);

  // Keyboard controls. Bound on window so they work whenever the overlay has
  // focus (the WebView2 overlay holds focus over the video). space=pause/play,
  // n=next show, p=previous show, d=defer, f=fullscreen, i=stay on top, h=hide all chrome,
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
          void runControl(skip);
          break;
        case 'p':
          void runControl(previous);
          break;
        case 'd':
          void runControl(defer);
          break;
        case 'f':
          void runControl(toggleFullscreen);
          break;
        case 'i':
          togglePin();
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
                void setSub(firstTrack.id);
              }
            } else {
              void setSub('no');
            }
          }
          break;
        }
        case 'ArrowLeft':
        case 'j':
          e.preventDefault();
          void runControl(() => seekRelative(-10));
          break;
        case 'ArrowRight':
        case 'l':
          e.preventDefault();
          void runControl(() => seekRelative(10));
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
      window.clearTimeout(controlsIdleTimer.current);
    };
  }, [adjustVolume, runControl, togglePin]);



  // The single owner of control-bar (and cursor) visibility. During playback the
  // bar hides after a short pointer idle; movement reveals it and re-arms the
  // timer. It is pinned open while the dashboard is open, while scrubbing, or
  // while the pointer hovers the controls; the pointer leaving the window hides
  // it. See docs/feature-contracts/desktop-shell.md.
  useEffect(() => {
    // Only auto-hide while video is actually advancing. `status.phase` stays
    // 'playing' across a pause (it tracks the round, not playback), so without
    // the paused check the bar and cursor would hide while paused — leaving the
    // controls invisible and unclickable until the mouse moves. Paused = show
    // the controls, like every other player.
    const activelyAdvancing = status.phase === 'playing' && !displayPaused;
    const keepOpen = showSettings || controlsPointerDown || controlsHovered;
    window.clearTimeout(controlsIdleTimer.current);
    if (!activelyAdvancing || keepOpen) {
      lastMousePos.current = null;
      setControlsIdle(false);
      return;
    }
    // Default to hidden on entering playback; movement brings the bar back.
    setControlsIdle(true);
    lastMousePos.current = null;

    const onActivity = (e?: MouseEvent) => {
      if (e) {
        const previous = lastMousePos.current;
        const current = { x: e.screenX, y: e.screenY };
        lastMousePos.current = current;
        // Ignore synthetic same-position moves (keyboard/media can emit them),
        // so idle-hidden controls stay hidden while the mouse is physically still.
        if (!previous || (current.x === previous.x && current.y === previous.y)) {
          return;
        }
      }
      setControlsIdle(false);
      armControlsIdle();
    };
    const onLeaveWindow = () => {
      lastMousePos.current = null;
      setControlsIdle(true);
    };
    window.addEventListener('mousemove', onActivity);
    document.addEventListener('mouseleave', onLeaveWindow);
    armControlsIdle();
    return () => {
      window.clearTimeout(controlsIdleTimer.current);
      window.removeEventListener('mousemove', onActivity);
      document.removeEventListener('mouseleave', onLeaveWindow);
    };
  }, [status.phase, displayPaused, showSettings, controlsHovered, controlsPointerDown, armControlsIdle]);

  useEffect(() => {
    const clearPointerDown = () => setControlsPointerDown(false);
    window.addEventListener('mouseup', clearPointerDown);
    window.addEventListener('pointerup', clearPointerDown);
    window.addEventListener('pointercancel', clearPointerDown);
    window.addEventListener('blur', clearPointerDown);
    return () => {
      window.removeEventListener('mouseup', clearPointerDown);
      window.removeEventListener('pointerup', clearPointerDown);
      window.removeEventListener('pointercancel', clearPointerDown);
      window.removeEventListener('blur', clearPointerDown);
    };
  }, []);

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
    // Watch history and episodes for the selected show.
    if (!selected) {
      setShowDetails(null);
      return;
    }
    let alive = true;
    fetchShowDetails(selected)
      .then((d) => alive && setShowDetails(d))
      .catch(() => alive && setShowDetails(null));
    return () => {
      alive = false;
    };
  }, [selected, status.last_advance?.advanced_count]);

  const playingByShow = new Set((status.round ?? []).map((r) => r.show_id));
  const roundActive = status.phase === 'playing';
  const overlayVisible = !roundActive || showSettings;
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
  const alerts = status.alerts ?? [];
  const repairableRoundProblems = status.round_blocked
    ? (status.file_sync?.problems ?? [])
    : [];
  const hasRepairedRoundEntries = repairedRoundEntries.size > 0;

  // Library edits mutate the database but don't change phase/advance, so re-fetch
  // the sidebar shows explicitly after add/remove/rescan.
  const refreshShows = () =>
    listShows()
      .then((s) => setShows(s ?? []))
      .catch(() => {});

  const refreshHistory = (showId: string) =>
    fetchShowDetails(showId)
      .then((d) => setShowDetails(d))
      .catch(() => setShowDetails(null));

  const handlePlayShow = (showId: string) => {
    void runControl(() => playShow(showId));
  };

  const handleRemoveRoundEntry = (episodeId: string) => {
    const problem = status.file_sync?.problems.find((p) => p.episode_id === episodeId);
    if (!window.confirm(`Remove ${problem?.show_name ?? 'this entry'} from the current round?`)) {
      return;
    }
    void runControl(() => removeRoundEntry(episodeId), {success: true}).then((ok) => {
      if (!ok) return;
      setRepairedRoundEntries((prev) => {
        const next = new Set(prev);
        next.add(episodeId);
        return next;
      });
    });
  };

  const handleReloadRound = () => {
    void runControl(reloadRound, {success: true}).then((ok) => {
      if (ok) setRepairedRoundEntries(new Set());
    });
  };

  const runLibraryAction = async (action: () => Promise<void>, success?: string) => {
    try {
      await action();
      if (success) {
        showControlToast(success, 'info');
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : 'library action failed';
      showControlToast(message, 'danger');
      console.error(e);
    }
  };

  const handleMarkWatched = async (showId: string) => {
    try {
      await markShowWatched(showId);
      await refreshShows();
      await refreshHistory(showId);
      getStats().then(setStats).catch(() => {});
      showControlToast('marked watched', 'info');
    } catch (e) {
      const message = e instanceof Error ? e.message : 'mark watched failed';
      showControlToast(message, 'danger');
    }
  };

  const handleMarkUnwatched = async (showId: string) => {
    try {
      await markShowUnwatched(showId);
      await refreshShows();
      await refreshHistory(showId);
      getStats().then(setStats).catch(() => {});
      showControlToast('marked unwatched', 'info');
    } catch (e) {
      const message = e instanceof Error ? e.message : 'mark unwatched failed';
      showControlToast(message, 'danger');
    }
  };



  const overviewHeader = (
    <div className="kpi overview-kpi">
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
  );

  const overviewContent = (
    <>
      {selectedShow && showDetails ? (
        <ShowOverview
          show={selectedShow}
          details={showDetails}
          onClose={() => setSelected(null)}
          onPlay={() => handlePlayShow(selectedShow.id)}
          onMarkWatched={() => handleMarkWatched(selectedShow.id)}
          onMarkUnwatched={() => handleMarkUnwatched(selectedShow.id)}
          onRemove={() => {
            void runLibraryAction(async () => {
              await removeShow(selectedShow.id);
              setSelected(null);
              await refreshShows();
            }, 'show removed');
          }}
          onRescan={() => {
            void runLibraryAction(async () => {
              const result = await rescanShow(selectedShow.id);
              await refreshShows();
              showControlToast(`added ${result.added} episode${result.added === 1 ? '' : 's'}`, 'info');
            });
          }}
        />
      ) : (
        <>
          {alerts.length > 0 && (
            <div className="section status-alerts">
              <h3>attention</h3>
              <div className="alert-list">
                {alerts.map((alert, index) => (
                  <div className={`status-alert ${alert.level}`} key={`${alert.title}-${index}`}>
                    <div className="alert-title">{alert.title}</div>
                    <div className="alert-message">{alert.message}</div>
                    {alert.detail && <div className="alert-detail">{alert.detail}</div>}
                  </div>
                ))}
              </div>
              {repairableRoundProblems.length > 0 && (
                <div className="round-repair-list">
                  {repairableRoundProblems.map((problem) => {
                    const removed = repairedRoundEntries.has(problem.episode_id);
                    return (
                      <div className={`round-repair-item${removed ? ' removed' : ''}`} key={problem.episode_id}>
                        <div className="round-repair-copy">
                          <div className="round-repair-title">{problem.show_name}</div>
                          <div className="round-repair-path">{problem.source_path || problem.local_path}</div>
                          {removed && <div className="round-repair-state">removed, reload round</div>}
                        </div>
                        <button
                          className="round-repair-btn"
                          disabled={removed}
                          onClick={() => handleRemoveRoundEntry(problem.episode_id)}
                        >
                          {removed ? 'removed' : 'remove from round'}
                        </button>
                      </div>
                    );
                  })}
                  {hasRepairedRoundEntries && (
                    <button className="round-reload-btn" onClick={handleReloadRound}>
                      reload round
                    </button>
                  )}
                </div>
              )}
            </div>
          )}
          <Queue round={round} pos={pos} onSelectShow={setSelected} />
          {round.length === 0 && (
            <div className="section">
              <h3>status</h3>
              <div style={{ color: status.phase === 'error' ? 'var(--state-danger-fg)' : 'var(--fg-secondary)' }}>
                {status.message || '—'}
              </div>
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
        </>
      )}
    </>
  );

  return (
    <div className={`overlay-root${controlsIdle ? ' cursor-hidden' : ''}${status.window_maximized ? ' window-maximized' : ''}${status.window_fullscreen ? ' window-fullscreen' : ''}`}>
      {!status.window_fullscreen && !status.window_on_top && (
        <div className="titlebar">
          <div className="titlebar-logo">
            <img src="/favicon.ico" className="titlebar-logo-img" alt="" />
            <span>shows</span>
          </div>
          <div className="titlebar-title">{status.playlist ? `shows — ${status.playlist}` : 'shows'}</div>
          <div className="titlebar-actions">
            <button className="titlebar-btn min-btn" onClick={() => void runControl(minimizeWindow)} title="Minimize">
              <svg viewBox="0 0 10 10"><path d="M0 5h10v1H0z" fill="currentColor" /></svg>
            </button>
            <button className="titlebar-btn max-btn" onClick={() => void runControl(maximizeWindow)} title={status.window_maximized ? "Restore" : "Maximize"}>
              {status.window_maximized ? (
                <svg viewBox="0 0 10 10"><path d="M2 0v2H0v8h8V8h2V0H2zM7 9H1V3h6v6zm2-2H8V2H3V1h6v6z" fill="currentColor" /></svg>
              ) : (
                <svg viewBox="0 0 10 10"><path d="M0 0v10h10V0H0zm9 9H1V1h8v8z" fill="currentColor" /></svg>
              )}
            </button>
            <button className="titlebar-btn close-btn" onClick={() => void runControl(closeWindow)} title="Close">
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
        onDoubleClick={() => void runControl(toggleFullscreen)}
        onWheel={handleVolumeWheel}
        {...surfaceDragProps}
      />
      <VolumeOsd volume={volOsd} />
      {controlToast && <ControlToast message={controlToast.message} level={controlToast.level} />}
      {status.update?.available && status.update.latest !== updateDismissed && (
        <UpdateBanner
          info={status.update}
          onDismiss={() => setUpdateDismissed(status.update!.latest)}
        />
      )}
      {overlayVisible ? (
        <div className={`layout${roundActive ? ' over-video' : ''}`}>


          <main className="main">
            <SettingsPanel
              status={status}
              shows={shows}
              stats={stats}
              selected={selected}
              setSelected={setSelected}
              refreshShows={refreshShows}
              removeShow={removeShow}
              onToast={showControlToast}
              overviewHeader={overviewHeader}
              overviewContent={overviewContent}
            />
          </main>
        </div>
      ) : (
        <div
          style={{ flex: 1 }}
          onDoubleClick={() => void runControl(toggleFullscreen)}
          onWheel={handleVolumeWheel}
          {...surfaceDragProps}
        />
      )}
      <BottomControlBar
        status={status}
        pos={pos}
        roundActive={roundActive}
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
        onPointerActiveChange={setControlsPointerDown}
        volume={displayVolume}
        onVolumeChange={requestVolume}
        onVolumeWheel={handleVolumeWheel}
        displayPaused={displayPaused}
        pinned={pinned}
        onRequestPause={requestPause}
        onPrevious={() => runControl(previous)}
        onSeekRelative={(seconds) => runControl(() => seekRelative(seconds))}
        onSkip={() => runControl(skip)}
        onDefer={() => runControl(defer)}
        onToggleStayOnTop={togglePin}
        onToggleFullscreen={() => runControl(toggleFullscreen)}
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

// Stay-on-top (pin) toggle icon — a thumbtack.
const PinIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M9 3h6M10 3v6l-3 3v2h10v-2l-3-3V3" />
    <path d="M12 14v7" />
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

function playbackProgressPercent(pb: Playback | undefined): number {
  if (!pb) return 0;
  const pct = pb.percent_pos ?? (
    pb.duration && pb.duration > 0 && pb.time_pos != null
      ? (pb.time_pos / pb.duration) * 100
      : 0
  );
  if (!Number.isFinite(pct)) return 0;
  return Math.min(100, Math.max(0, pct));
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

function ControlToast({ message, level }: { message: string; level: 'info' | 'danger' }) {
  return (
    <div className={`control-toast ${level}`} role="status" aria-live="polite">
      {message}
    </div>
  );
}

// Unified bottom-anchored control bar.
function BottomControlBar({
  status,
  pos,
  roundActive,
  viewing,
  onToggleView,
  viewingSettings,
  onToggleSettings,
  controlsIdle,
  onHoverChange,
  onPointerActiveChange,
  volume,
  onVolumeChange,
  onVolumeWheel,
  displayPaused,
  pinned,
  onRequestPause,
  onPrevious,
  onSeekRelative,
  onSkip,
  onDefer,
  onToggleStayOnTop,
  onToggleFullscreen,
}: {
  status: Status;
  pos: number;
  roundActive: boolean;
  viewing: boolean;
  onToggleView: () => void;
  viewingSettings: boolean;
  onToggleSettings: () => void;
  controlsIdle: boolean;
  onHoverChange: (hovered: boolean) => void;
  onPointerActiveChange: (active: boolean) => void;
  volume: number;
  onVolumeChange: (volume: number, flash?: boolean) => void;
  onVolumeWheel: WheelEventHandler<HTMLDivElement>;
  displayPaused: boolean;
  pinned: boolean;
  onRequestPause: (paused: boolean) => void;
  onPrevious: () => void;
  onSeekRelative: (seconds: number) => void;
  onSkip: () => void;
  onDefer: () => void;
  onToggleStayOnTop: () => void;
  onToggleFullscreen: () => void;
}) {
  const pb = status.playback;
  const pct = playbackProgressPercent(pb);

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
          void setSub(firstTrack.id);
        }
      } else {
        void setSub('no');
      }
    }
  };

  const isMuted = pb ? volume === 0 : false;
  const round = status.round ?? [];
  const currentEntry =
    round.length > 0 && pos >= 0 && pos < round.length ? round[pos] : null;
  const nowPlayingState = displayPaused ? 'paused' : isMuted ? 'muted' : null;
  const nowPlayingText = currentEntry
    ? `${nowPlayingState ? `${nowPlayingState} - ` : ''}${currentEntry.show_name}   (${pos + 1}/${round.length})`
    : status.message || '—';

  const ccActive = pb ? pb.sid !== 'no' && pb.sid != null : false;
  // Optimistic intent (echoed-state-independent) so the pin button highlights
  // the instant it is clicked, instead of waiting a full status round-trip —
  // the lag is what made it feel like the toggle "didn't take".
  const onTop = pinned;
  // The compact control layout follows window *width*, not any window mode — a
  // small window gets it whether or not it is pinned on top.
  const [compactViewport, setCompactViewport] = useState(() => window.innerWidth <= 900);

  useEffect(() => {
    const onResize = () => setCompactViewport(window.innerWidth <= 900);
    window.addEventListener('resize', onResize);
    onResize();
    return () => window.removeEventListener('resize', onResize);
  }, []);

  const renderScrub = (compact: boolean) => (
    <div className={`scrub-container${compact ? ' mini-scrub-container' : ''}`}>
      {!compact && <span className="time-display">{pb ? fmtTime(pb.time_pos) : '--:--'}</span>}
      <div
        className={`scrub-bar${!pb ? ' disabled' : ''}`}
        onClick={(e) => {
          if (!pb) return;
          const r = e.currentTarget.getBoundingClientRect();
          void seekPercent(Math.max(0, Math.min(100, ((e.clientX - r.left) / r.width) * 100)));
        }}
      >
        <div className="scrub-fill" style={{ width: `${pct}%` }} />
        <div className="scrub-handle" style={{ left: `${pct}%` }} />
      </div>
      {!compact && <span className="time-display">{pb ? fmtTime(pb.duration) : '--:--'}</span>}
    </div>
  );

  if (compactViewport) {
    return (
      <div
        className={`bottom-controls mini-controls${controlsIdle ? ' hidden' : ''}`}
        onMouseEnter={() => onHoverChange(true)}
        onMouseLeave={() => onHoverChange(false)}
        onPointerDown={() => onPointerActiveChange(true)}
        onPointerUp={() => onPointerActiveChange(false)}
        onPointerCancel={() => onPointerActiveChange(false)}
        onWheel={onVolumeWheel}
      >
        {renderScrub(true)}

        <div className="mini-controls-row">
          <button
            className="control-btn"
            onClick={onPrevious}
            disabled={!roundActive}
            title="Previous Show (p)"
          >
            <PrevIcon />
          </button>

          <button
            className="control-btn"
            onClick={() => onSeekRelative(-10)}
            disabled={!pb}
            title="Rewind 10s (j / ←)"
          >
            <RewindIcon />
          </button>

          <button
            className="control-btn play-pause-btn"
            onClick={() => onRequestPause(!displayPaused)}
            disabled={!roundActive}
            title="Play / Pause (Space)"
          >
            {displayPaused ? <PlayIcon /> : <PauseIcon />}
          </button>

          <button
            className="control-btn"
            onClick={() => onSeekRelative(10)}
            disabled={!pb}
            title="Forward 10s (l / →)"
          >
            <ForwardIcon />
          </button>

          <button
            className="control-btn"
            onClick={onSkip}
            disabled={!roundActive}
            title="Skip Show (n)"
          >
            <NextIcon />
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

          <button
            className="control-btn volume-btn"
            onClick={handleToggleMute}
            disabled={!pb}
            title="Mute / Unmute (Up/Down Arrows or mouse wheel to adjust)"
          >
            {isMuted ? <VolumeMuteIcon /> : <VolumeIcon />}
          </button>

          <button
            className={`control-btn stay-on-top-btn${onTop ? ' active' : ''}`}
            onClick={onToggleStayOnTop}
            title={onTop ? 'Stop Staying on Top (i)' : 'Stay on Top (i)'}
          >
            <PinIcon />
          </button>
        </div>
      </div>
    );
  }

  return (
    <div
      className={`bottom-controls${controlsIdle ? ' hidden' : ''}`}
      onMouseEnter={() => onHoverChange(true)}
      onMouseLeave={() => onHoverChange(false)}
      onPointerDown={() => onPointerActiveChange(true)}
      onPointerUp={() => onPointerActiveChange(false)}
      onPointerCancel={() => onPointerActiveChange(false)}
      onWheel={onVolumeWheel}
    >
      {/* 1. Scrub Container */}
      {renderScrub(false)}

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
            onClick={onPrevious}
            disabled={!roundActive}
            title="Previous Show (p)"
          >
            <PrevIcon />
          </button>
          
          <button
            className="control-btn"
            onClick={() => onSeekRelative(-10)}
            disabled={!pb}
            title="Rewind 10s (j / ←)"
          >
            <RewindIcon />
          </button>

          <button
            className="control-btn play-pause-btn"
            onClick={() => onRequestPause(!displayPaused)}
            disabled={!roundActive}
            title="Play / Pause (Space)"
          >
            {displayPaused ? <PlayIcon /> : <PauseIcon />}
          </button>

          <button
            className="control-btn"
            onClick={() => onSeekRelative(10)}
            disabled={!pb}
            title="Forward 10s (l / →)"
          >
            <ForwardIcon />
          </button>

          <button
            className="control-btn"
            onClick={onSkip}
            disabled={!roundActive}
            title="Skip Show (n)"
          >
            <NextIcon />
          </button>
        </div>

        {/* Right: Sound, Selectors, Sync, Fullscreen, View, Hide */}
        <div className="controls-group right-controls">
          <button
            className="control-btn defer-btn"
            onClick={onDefer}
            disabled={!roundActive}
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
                void setSub(e.currentTarget.value === 'no' ? 'no' : Number(e.currentTarget.value))
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
              onChange={(e) => void setAudio(Number(e.currentTarget.value))}
            >
              {pb.audio_tracks.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.title}
                </option>
              ))}
            </select>
          )}

          {roundActive && (
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
            className={`control-btn stay-on-top-btn${onTop ? ' active' : ''}`}
            onClick={onToggleStayOnTop}
            title={onTop ? 'Stop Staying on Top (i)' : 'Stay on Top (i)'}
          >
            <PinIcon />
          </button>

          <button
            className="control-btn fullscreen-btn"
            onClick={onToggleFullscreen}
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

function ShowOverview({
  show,
  details,
  onClose,
  onPlay,
  onMarkWatched,
  onMarkUnwatched,
  onRemove,
  onRescan,
}: {
  show: Show;
  details: ShowDetailsResponse;
  onClose: () => void;
  onPlay: () => void;
  onMarkWatched: () => void;
  onMarkUnwatched: () => void;
  onRemove: () => void;
  onRescan: () => void;
}) {
  const [view, setView] = useState<'all' | 'previous' | 'upcoming' | 'history'>('all');

  return (
    <div className="section">
      <h3>
        overview - {show.name}
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

      <div style={{ display: 'flex', gap: '8px', marginBottom: '16px' }}>
        <button className="gb" style={{ opacity: view === 'all' ? 1 : 0.5 }} onClick={() => setView('all')}>
          all episodes ({details.all_episodes.length})
        </button>
        <button className="gb" style={{ opacity: view === 'previous' ? 1 : 0.5 }} onClick={() => setView('previous')}>
          previous episodes ({details.previous_episodes.length})
        </button>
        <button className="gb" style={{ opacity: view === 'upcoming' ? 1 : 0.5 }} onClick={() => setView('upcoming')}>
          upcoming episodes ({details.upcoming_episodes.length})
        </button>
        <button className="gb" style={{ opacity: view === 'history' ? 1 : 0.5 }} onClick={() => setView('history')}>
          history ({details.history.length})
        </button>
      </div>

      {view === 'history' && (
        <>
          {details.history.length === 0 ? (
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
                {details.history.map((e) => (
                  <tr key={e.episode_id + e.played_at}>
                    <td>{shortPath(e.relative_path)}</td>
                    <td style={{ color: 'var(--fg-dim)' }}>{relTime(e.played_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </>
      )}

      {view !== 'history' && (
        <>
          {view === 'all' && details.all_episodes.length === 0 && <div className="empty">no episodes found.</div>}
          {view === 'previous' && details.previous_episodes.length === 0 && <div className="empty">no previous episodes.</div>}
          {view === 'upcoming' && details.upcoming_episodes.length === 0 && <div className="empty">no upcoming episodes.</div>}
          
          <table className="runs">
            <thead>
              <tr>
                <th>episode</th>
                <th>status</th>
              </tr>
            </thead>
            <tbody>
              {(view === 'all' ? details.all_episodes : view === 'previous' ? details.previous_episodes : details.upcoming_episodes).map((ep) => (
                <tr key={ep.id}>
                  <td>{shortPath(ep.relative_path)}</td>
                  <td style={{ color: 'var(--fg-dim)' }}>{ep.watched_at ? 'watched' : 'unwatched'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  );
}

// Add a show by pointing at a local folder; the desktop scans it for episodes.
function AddShowForm({
  playlist,
  onAdded,
  onToast,
}: {
  playlist: string;
  onAdded: () => void;
  onToast?: (message: string, level?: 'info' | 'danger') => void;
}) {
  const [mode, setMode] = useState<'manual' | 'detect' | 'detect-episodes'>('detect');
  
  // Manual state
  const [name, setName] = useState('');
  const [path, setPath] = useState('');
  const [pl, setPl] = useState(playlist || 'nelson');
  const [msg, setMsg] = useState('');
  const [busy, setBusy] = useState(false);
  const [previewData, setPreviewData] = useState<string[] | null>(null);

  // Detect state
  const PRESET_FOLDER = "S:\\Group-Nelson";
  const [detectedFolders, setDetectedFolders] = useState<string[] | null>(null);
  const [detecting, setDetecting] = useState(false);
  const [lastRefreshed, setLastRefreshed] = useState<Date | null>(null);

  // Detect episodes state
  const [detectedEpisodes, setDetectedEpisodes] = useState<ShowNewEpisodes[] | null>(null);
  const [detectingEpisodes, setDetectingEpisodes] = useState(false);
  const [lastRefreshedEpisodes, setLastRefreshedEpisodes] = useState<Date | null>(null);
  const [selectedEpisodeShow, setSelectedEpisodeShow] = useState<ShowNewEpisodes | null>(null);
  const [checkedEpisodes, setCheckedEpisodes] = useState<Set<string>>(new Set());

  const handlePreview = async (overridePath?: string, overrideName?: string) => {
    const p = overridePath || path;
    const n = overrideName !== undefined ? overrideName : name;
    const cleanPath = p.trim().replace(/^["']|["']$/g, '');
    if (!n.trim() || !cleanPath) {
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
      const message = String(e.message || e);
      setMsg(message);
      onToast?.(message, 'danger');
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
        const message = `added ${r.episodes} episode${r.episodes === 1 ? '' : 's'}`;
        setMsg(message);
        onToast?.(message, 'info');
        setName('');
        setPath('');
        setPreviewData(null);
        setTimeout(() => {
          setMsg('');
        }, 1500);
        setDetectedFolders(null);
        onAdded();
      })
      .catch((e) => {
        const message = String(e.message || e);
        setMsg(message);
        onToast?.(message, 'danger');
      })
      .finally(() => setBusy(false));
  };

  const handleDetect = async () => {
    setDetecting(true);
    setMsg('detecting…');
    try {
      const folders = await detectNewFolders(PRESET_FOLDER);
      setDetectedFolders(folders);
      setLastRefreshed(new Date());
      setMsg('');
    } catch (e: any) {
      const message = String(e.message || e);
      setMsg(message);
      onToast?.(message, 'danger');
    } finally {
      setDetecting(false);
    }
  };

  const handleDetectEpisodes = async () => {
    setDetectingEpisodes(true);
    setDetectedEpisodes(null);
    setMsg('detecting new episodes.');
    try {
      const shows = await detectNewEpisodes();
      setDetectedEpisodes(shows);
      setLastRefreshedEpisodes(new Date());
      setSelectedEpisodeShow(null);
      setMsg('');
    } catch (e: any) {
      const message = String(e.message || e);
      setMsg(message);
      onToast?.(message, 'danger');
    } finally {
      setDetectingEpisodes(false);
    }
  };

  const handleAddEpisodes = async (showId: string, markWatched: boolean = false) => {
    setBusy(true);
    setMsg(markWatched ? 'adding and marking as watched…' : 'adding episodes…');
    try {
      const episodesToPass = Array.from(checkedEpisodes);
      const res = markWatched ? await rescanWatchedShow(showId, episodesToPass) : await rescanShow(showId, episodesToPass);
      setSelectedEpisodeShow(null);
      const message = `added ${res.added} episode${res.added === 1 ? '' : 's'}${markWatched ? ' as watched' : ''}`;
      setMsg(message);
      onToast?.(message, 'info');
      handleDetectEpisodes();
      onAdded();
      setBusy(false);
    } catch(e: any) {
      const message = String(e.message || e);
      setMsg(message);
      onToast?.(message, 'danger');
      setBusy(false);
    }
  };

  const selectDetected = (folderPath: string) => {
    // Extract the folder name to use as default show name
    const parts = folderPath.split(/[\\/]/);
    const folderName = parts[parts.length - 1] || '';
    
    setName(folderName);
    setPath(folderPath);
    setMode('manual');
    setMsg('');
    // Auto-preview once the state settles
    setTimeout(() => {
      handlePreview(folderPath, folderName);
    }, 50);
  };

  useEffect(() => {
    if (mode === 'detect' && detectedFolders === null && !detecting) {
      handleDetect();
    }
  }, [mode, detectedFolders, detecting]);

  useEffect(() => {
    if (mode === 'detect-episodes' && detectedEpisodes === null && !detectingEpisodes) {
      handleDetectEpisodes();
    }
  }, [mode, detectedEpisodes, detectingEpisodes]);

  return (
    <div className="add-form" style={{ display: 'flex', flexDirection: 'column', gap: '8px', flex: 1, minHeight: 0 }}>
      <div style={{ display: 'flex', gap: '8px', marginBottom: '8px' }}>
        <button 
          className="gb" 
          style={{ flex: 1, textAlign: 'center', padding: '8px', opacity: mode === 'detect' ? 1 : 0.5 }}
          onClick={() => { 
            if (mode === 'detect') {
              handleDetect();
            } else {
              setMode('detect'); 
            }
            setMsg(''); 
          }}
        >
          detect new folders
        </button>
        <button 
          className="gb" 
          style={{ flex: 1, textAlign: 'center', padding: '8px', opacity: mode === 'detect-episodes' ? 1 : 0.5 }}
          onClick={() => { 
            if (mode === 'detect-episodes') {
              handleDetectEpisodes();
            } else {
              setMode('detect-episodes'); 
              handleDetectEpisodes();
            }
            setMsg(''); 
          }}
        >
          detect new episodes
        </button>
        <button 
          className="gb" 
          style={{ flex: 1, textAlign: 'center', padding: '8px', opacity: mode === 'manual' ? 1 : 0.5 }}
          onClick={() => { setMode('manual'); setMsg(''); }}
        >
          manually add
        </button>
      </div>

      {mode === 'detect' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '8px', flex: 1, minHeight: 0 }}>
          <div style={{ fontSize: '13px', opacity: 0.8, marginBottom: '4px' }}>
            Scanning folder: <strong>{PRESET_FOLDER}</strong>
          </div>
          
          {detectedFolders === null && detecting && (
            <div style={{ padding: '8px', opacity: 0.7 }}>Scanning NAS for new folders...</div>
          )}

          {detectedFolders !== null && (
            <div style={{ marginTop: '16px', marginBottom: '8px', display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', marginBottom: '8px' }}>
                <h4 style={{ margin: 0, fontSize: '13px', color: 'var(--fg)' }}>
                  Found {detectedFolders.length} new folder{detectedFolders.length === 1 ? '' : 's'}
                </h4>
                {lastRefreshed && (
                  <span style={{ fontSize: '12px', opacity: 0.6 }}>
                    Refreshed at {lastRefreshed.toLocaleTimeString()}
                  </span>
                )}
              </div>
              {detectedFolders.length > 0 && (
                <div style={{ border: '1px solid var(--border)', borderRadius: '4px', overflowY: 'auto', flex: '0 1 auto', minHeight: 0 }}>
                  <ul className="queue">
                    {detectedFolders.map((folder, i) => {
                      const parts = folder.split(/[\\/]/);
                      const folderName = parts[parts.length - 1] || folder;
                      return (
                        <li key={i} className="next" style={{ cursor: 'pointer' }} onClick={() => selectDetected(folder)}>
                          <span className="q-mark" style={{ opacity: 0.5 }}>+</span>
                          <span className="q-show">{folderName}</span>
                          <span className="q-ep">{folder}</span>
                        </li>
                      );
                    })}
                  </ul>
                </div>
              )}
            </div>
          )}
        </div>
      )}

      {mode === 'detect-episodes' && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: '8px', flex: 1, minHeight: 0 }}>
          <div style={{ fontSize: '13px', opacity: 0.8, marginBottom: '4px' }}>
            Scanning all shows for new episodes...
          </div>
          
          {detectedEpisodes === null && detectingEpisodes && (
            <div style={{ padding: '8px', opacity: 0.7 }}>Scanning NAS for new episodes...</div>
          )}

          {detectedEpisodes !== null && !selectedEpisodeShow && (
            <div style={{ marginTop: '16px', marginBottom: '8px', display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline', marginBottom: '8px' }}>
                <h4 style={{ margin: 0, fontSize: '13px', color: 'var(--fg)' }}>
                  Found new episodes for {detectedEpisodes.length} show{detectedEpisodes.length === 1 ? '' : 's'}
                </h4>
                {lastRefreshedEpisodes && (
                  <span style={{ fontSize: '12px', opacity: 0.6 }}>
                    Refreshed at {lastRefreshedEpisodes.toLocaleTimeString()}
                  </span>
                )}
              </div>
              {detectedEpisodes.length > 0 && (
                <div style={{ border: '1px solid var(--border)', borderRadius: '4px', overflowY: 'auto', flex: '0 1 auto', minHeight: 0 }}>
                  <ul className="queue">
                    {detectedEpisodes.map((show, i) => (
                      <li key={i} className="next" style={{ cursor: 'pointer' }} onClick={() => { setSelectedEpisodeShow(show); setCheckedEpisodes(new Set(show.new_episodes)); }}>
                        <span className="q-mark" style={{ opacity: 0.5 }}>+</span>
                        <span className="q-show">{show.show_name}</span>
                        <span className="q-ep">{show.new_episodes.length} new episode{show.new_episodes.length === 1 ? '' : 's'}</span>
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </div>
          )}

          {selectedEpisodeShow && (
            <div style={{ marginTop: '16px', display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
              <div style={{ display: 'flex', gap: '8px', marginBottom: '8px', alignItems: 'center' }}>
                <button className="gb" onClick={() => setSelectedEpisodeShow(null)}>← back</button>
                <h4 style={{ margin: 0, fontSize: '13px', color: 'var(--fg)' }}>{selectedEpisodeShow.show_name}</h4>
                <div style={{ flex: 1 }} />
                <button className="gb" disabled={busy} onClick={() => handleAddEpisodes(selectedEpisodeShow.show_id, false)}>
                  add to show
                </button>
                <button className="gb" disabled={busy} onClick={() => handleAddEpisodes(selectedEpisodeShow.show_id, true)}>
                  add as watched
                </button>
              </div>
              <div style={{ border: '1px solid var(--border)', borderRadius: '4px', overflowY: 'auto', flex: '1 1 auto', minHeight: 0 }}>
                <ul style={{ margin: 0, padding: '12px', fontSize: '13px', color: 'var(--fg-secondary)', listStyleType: 'none' }}>
                  {selectedEpisodeShow.new_episodes.map((ep, j) => (
                    <li key={j} style={{ padding: '4px 0', borderBottom: j < selectedEpisodeShow.new_episodes.length - 1 ? '1px solid var(--border)' : 'none', wordBreak: 'break-all', display: 'flex', gap: '12px', alignItems: 'center' }}>
                      <input 
                        type="checkbox" 
                        checked={checkedEpisodes.has(ep)}
                        onChange={(e) => {
                          const next = new Set(checkedEpisodes);
                          if (e.target.checked) next.add(ep);
                          else next.delete(ep);
                          setCheckedEpisodes(next);
                        }}
                      />
                      <span style={{ opacity: 0.5, flexShrink: 0, minWidth: '24px', textAlign: 'right' }}>{j + 1}.</span>
                      <span>{ep}</span>
                    </li>
                  ))}
                </ul>
              </div>
            </div>
          )}
        </div>
      )}

      {mode === 'manual' && (
        <form onSubmit={(e) => { e.preventDefault(); handleAdd(); }} style={{ display: 'flex', flexDirection: 'column', gap: '8px', flex: 1, minHeight: 0 }}>
          <input placeholder="show name" value={name} onChange={(e) => setName(e.target.value)} />
          <div style={{ display: 'flex', gap: '8px' }}>
            <input
              placeholder="folder path"
              value={path}
              onChange={(e) => setPath(e.target.value)}
              style={{ flex: 1 }}
            />
            <button className="gb" type="button" disabled={busy} onClick={() => pickFolder().then(p => { if (p) setPath(p); })}>
              browse
            </button>
          </div>
          <input placeholder="playlist" value={pl} onChange={(e) => setPl(e.target.value)} />
          
          <div className="add-actions" style={{ display: 'flex', gap: '8px', marginTop: '8px' }}>
            {!previewData ? (
              <button className="gb" disabled={busy} onClick={() => handlePreview()}>
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

          {previewData && (
            <div style={{ marginTop: '16px', marginBottom: '8px', display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
              <h4 style={{ margin: '0 0 8px 0', fontSize: '13px', color: 'var(--fg)', flexShrink: 0 }}>
                Found {previewData.length} episode{previewData.length === 1 ? '' : 's'}
              </h4>
              <div style={{ border: '1px solid var(--border)', borderRadius: '4px', overflowY: 'auto', flex: '0 1 auto', minHeight: 0 }}>
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
        </form>
      )}

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
  stats,
  selected,
  setSelected,
  refreshShows,
  removeShow,
  onToast,
  overviewHeader,
  overviewContent,
}: {
  status: Status;
  shows: Show[];
  stats: Stats | null;
  selected: string | null;
  setSelected: (id: string | null) => void;
  refreshShows: () => void;
  removeShow: (id: string) => void | Promise<void>;
  onToast: (message: string, level?: 'info' | 'danger') => void;
  overviewHeader: React.ReactNode;
  overviewContent: React.ReactNode;
}) {
  const [activeTab, setActiveTab] = useState<'overview' | 'stats' | 'library' | 'add_show' | 'next_round' | 'general' | 'appearance'>('overview');

  const statsHeader = stats && stats.total_shows ? (() => {
    const pct = stats.episodes_total
      ? Math.round((stats.episodes_watched / stats.episodes_total) * 100)
      : 0;
    return (
      <div className="kpi overview-kpi">
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
    );
  })() : null;

  const libraryHeader = (
    <div className="kpi overview-kpi">
      <div className="kpi-cell">
        <div className="kpi-key">total shows</div>
        <div className="kpi-val">{shows.length}</div>
      </div>
    </div>
  );

  const nextRoundHeader = (
    <div className="kpi overview-kpi">
      <div className="kpi-cell" style={{ flex: 1 }}>
        <div className="kpi-key">preview info</div>
        <div className="kpi-val" style={{ fontSize: '14.5px', whiteSpace: 'normal', fontWeight: 400 }}>
          this is what the next round of episodes would look like if it was generated right now.
        </div>
      </div>
    </div>
  );

  return (
    <div className="settings-panel">
      <div className="settings-sidebar">
        <h3>settings</h3>
        <nav className="settings-nav" aria-label="Settings sections">
          <button aria-current={activeTab === 'overview' ? 'page' : undefined} className={`nav-btn${activeTab === 'overview' ? ' active' : ''}`} onClick={() => setActiveTab('overview')}>overview</button>
          <button aria-current={activeTab === 'stats' ? 'page' : undefined} className={`nav-btn${activeTab === 'stats' ? ' active' : ''}`} onClick={() => setActiveTab('stats')}>stats</button>
          <button aria-current={activeTab === 'library' ? 'page' : undefined} className={`nav-btn${activeTab === 'library' ? ' active' : ''}`} onClick={() => setActiveTab('library')}>library</button>
          <button aria-current={activeTab === 'add_show' ? 'page' : undefined} className={`nav-btn${activeTab === 'add_show' ? ' active' : ''}`} onClick={() => setActiveTab('add_show')}>add show</button>
          <button aria-current={activeTab === 'next_round' ? 'page' : undefined} className={`nav-btn${activeTab === 'next_round' ? ' active' : ''}`} onClick={() => setActiveTab('next_round')}>next round</button>
          <button aria-current={activeTab === 'general' ? 'page' : undefined} className={`nav-btn${activeTab === 'general' ? ' active' : ''}`} onClick={() => setActiveTab('general')}>general</button>
          <button aria-current={activeTab === 'appearance' ? 'page' : undefined} className={`nav-btn${activeTab === 'appearance' ? ' active' : ''}`} onClick={() => setActiveTab('appearance')}>appearance</button>
        </nav>
      </div>

      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', minWidth: 0 }}>
        {activeTab === 'overview' && overviewHeader}
        {activeTab === 'stats' && statsHeader}
        {activeTab === 'library' && libraryHeader}
        {activeTab === 'next_round' && nextRoundHeader}
        <div className="settings-content">
          {activeTab === 'overview' && (
            <div className="settings-tab">
              {overviewContent}
            </div>
          )}

        {activeTab === 'stats' && (
          <div className="settings-tab">
            <StatsPanel stats={stats} />
          </div>
        )}

        {activeTab === 'library' && (
          <div className="settings-tab">
            <div className="section">

              {shows.length === 0 ? (
                <div className="empty">no shows yet.</div>
              ) : (
                <ul className="queue">
                  {[...shows].sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' })).map((sh) => (
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

        {activeTab === 'next_round' && (
          <div className="settings-tab">
            <NextRoundTab currentRound={status.round || []} onSelectShow={(id) => { setSelected(id); setActiveTab('overview'); }} />
          </div>
        )}

        {activeTab === 'add_show' && (
          <div className="settings-tab" style={{ display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0 }}>
            <div className="section" style={{ display: 'flex', flexDirection: 'column', flex: 1, minHeight: 0, marginBottom: 0 }}>
              <h3 style={{ flexShrink: 0 }}>add show</h3>
              <AddShowForm playlist={status.playlist} onAdded={refreshShows} onToast={onToast} />
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
              <div className="empty">no appearance settings yet.</div>
            </div>
          </div>
        )}
        </div>
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
    <>
      <div className="section">
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
    </>
  );
}

