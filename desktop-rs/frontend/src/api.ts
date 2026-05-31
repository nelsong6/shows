// Data layer. The desktop client's control server exposes a same-origin HTTP
// surface — GET /status, GET /shows, GET /history?show=<id>, POST /pause,
// POST /skip, POST /defer — and serves this page, so all of these are relative
// to its origin.

export type RoundEntry = {
  show_id: string;
  show_name: string;
  episode_id: string;
  absolute_path: string;
  order_value: number;
  // Set on cross-playlist rounds so each entry shows which playlist it's from.
  playlist?: string;
};

export type RemovedShow = {
  id: string;
  name: string;
  date_added: string;
  last_played_at: string;
};

export type AdvanceResult = {
  advanced_count: number;
  removed_shows?: RemovedShow[];
};

export type Phase =
  | 'initializing'
  | 'auth'
  | 'fetching'
  | 'playing'
  | 'drained'
  | 'error';

export type Track = {
  id: number;
  title: string;
  lang?: string | null;
  selected: boolean;
};

// Live mpv playback state, read fresh on each /status poll.
export type Playback = {
  time_pos: number | null;
  duration: number | null;
  percent_pos: number | null;
  volume: number | null;
  paused: boolean;
  sub_tracks: Track[];
  audio_tracks: Track[];
  sid: number | string | null;
  aid: number | string | null;
};

// Offline-first sync state: are we reaching the server, and how many local
// changes are queued to push (the git "ahead" count).
export type SyncState = {
  online: boolean;
  pending: number;
};

// Set by the launch update-check when this build is behind the latest release.
export type UpdateInfo = {
  available: boolean;
  latest: string;
  current: string;
  url: string;
};

export type Status = {
  phase: Phase;
  message: string;
  playlist: string;
  round?: RoundEntry[];
  // Index into `round` of the entry currently playing (mpv playlist-pos).
  round_pos?: number;
  last_advance?: AdvanceResult;
  playback?: Playback;
  sync?: SyncState;
  update?: UpdateInfo;
};

export type Show = {
  id: string;
  playlist: string;
  name: string;
  root_path: string;
  date_added: string;
  removed_at?: string;
};

export type HistoryEvent = {
  episode_id: string;
  relative_path: string;
  played_at: string;
};

export async function getStatus(): Promise<Status> {
  const r = await fetch('/status');
  if (!r.ok) throw new Error(`/status ${r.status}`);
  return r.json();
}

export async function listShows(): Promise<Show[]> {
  const r = await fetch('/shows');
  if (!r.ok) throw new Error(`/shows ${r.status}`);
  return r.json();
}

export async function listHistory(showId: string): Promise<HistoryEvent[]> {
  const r = await fetch(`/history?show=${encodeURIComponent(showId)}`);
  if (!r.ok) throw new Error(`/history ${r.status}`);
  return r.json();
}

export type ShowProgress = {
  name: string;
  playlist: string;
  watched: number;
  total: number;
  removed: boolean;
};

export type Stats = {
  total_shows: number;
  active_shows: number;
  finished_shows: number;
  episodes_total: number;
  episodes_watched: number;
  per_show: ShowProgress[];
  recent: {show: string; relative_path: string; played_at: string}[];
  by_day: Record<string, number>;
};

export async function getStats(): Promise<Stats> {
  const r = await fetch('/stats');
  if (!r.ok) throw new Error(`/stats ${r.status}`);
  return r.json();
}

export function pause(): void {
  void fetch('/pause', {method: 'POST'});
}

export function skip(): void {
  void fetch('/skip', {method: 'POST'});
}

export function prev(): void {
  void fetch('/prev', {method: 'POST'});
}

export function playShow(showId: string): void {
  void fetch('/play-show', {method: 'POST', body: JSON.stringify({show_id: showId})});
}

export async function markShowWatched(showId: string): Promise<void> {
  const r = await fetch('/library/mark-watched', {method: 'POST', body: JSON.stringify({show_id: showId})});
  if (!r.ok) throw new Error(`Failed to mark watched: ${r.status}`);
}

export async function markShowUnwatched(showId: string): Promise<void> {
  const r = await fetch('/library/mark-unwatched', {method: 'POST', body: JSON.stringify({show_id: showId})});
  if (!r.ok) throw new Error(`Failed to mark unwatched: ${r.status}`);
}

// Re-roll the current show's next-round pick without marking it watched
// (server contract D1-D3). The runner jumps to the next entry too.
export function defer(): void {
  void fetch('/defer', {method: 'POST'});
}

// Toggle the Qt window between windowed and fullscreen.
export function toggleFullscreen(): void {
  void fetch('/fullscreen', {method: 'POST'});
}

export function seekPercent(percent: number): void {
  void fetch('/seek', {method: 'POST', body: JSON.stringify({percent})});
}

export function seekRelative(seconds: number): void {
  void fetch('/seek', {method: 'POST', body: JSON.stringify({seconds})});
}

export function setVolume(volume: number): void {
  void fetch('/volume', {method: 'POST', body: JSON.stringify({volume})});
}

export function setSub(sid: number | string): void {
  void fetch('/sub', {method: 'POST', body: JSON.stringify({sid})});
}

export function setAudio(aid: number | string): void {
  void fetch('/audio', {method: 'POST', body: JSON.stringify({aid})});
}

// Manual "check connectivity" / reconcile — push queued changes + pull.
export function syncNow(): void {
  void fetch('/sync-now', {method: 'POST'});
}

// ── library management (desktop scans the dir, the change syncs up) ──
export async function addShow(
  name: string,
  root_path: string,
  playlist: string,
): Promise<{id: string; episodes: number}> {
  const r = await fetch('/library/add', {
    method: 'POST',
    body: JSON.stringify({name, root_path, playlist}),
  });
  if (!r.ok) {
    const msg = await r.json().catch(() => ({}));
    throw new Error(msg.error || `add failed (${r.status})`);
  }
  return r.json();
}

export function removeShow(show_id: string): void {
  void fetch('/library/remove', {method: 'POST', body: JSON.stringify({show_id})});
}

export function updateShow(
  show_id: string,
  fields: {name?: string; root_path?: string; playlist?: string},
): void {
  void fetch('/library/update', {method: 'POST', body: JSON.stringify({show_id, ...fields})});
}

export async function rescanShow(show_id: string): Promise<{added: number}> {
  const r = await fetch('/library/rescan', {method: 'POST', body: JSON.stringify({show_id})});
  return r.json();
}

// Poll /status on an interval, invoking `onStatus` with each successful
// read. Returns an unsubscribe that stops the polling — same lifecycle
// contract as the old EventsOn('status', …) subscription.
export function subscribeStatus(
  onStatus: (s: Status) => void,
  intervalMs = 700,
): () => void {
  let stopped = false;
  const tick = async () => {
    try {
      const s = await getStatus();
      if (!stopped) onStatus(s);
    } catch {
      // transient (server still coming up / between requests) — keep polling
    }
  };
  void tick();
  const id = setInterval(tick, intervalMs);
  return () => {
    stopped = true;
    clearInterval(id);
  };
}
