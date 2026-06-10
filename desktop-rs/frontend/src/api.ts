// Data layer. The desktop client's control server exposes a same-origin HTTP
// surface. Live state arrives over GET /status/stream; one-shot reads use
// GET /status, GET /shows, GET /history?show=<id>, while controls POST to
// /pause, /skip, /defer, etc. The server serves this page, so all URLs are
// relative to its origin.

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

// Live mpv playback state, pushed by the desktop status stream.
export type Playback = {
  time_pos: number | null;
  duration: number | null;
  percent_pos: number | null;
  volume: number | null;
  paused: boolean;
  core_idle?: boolean;
  paused_for_cache?: boolean;
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
  round_id?: number | null;
  last_advance?: AdvanceResult;
  playback?: Playback;
  sync?: SyncState;
  update?: UpdateInfo;
  window_maximized?: boolean;
  window_fullscreen?: boolean;
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

export async function pause(paused?: boolean): Promise<void> {
  let url = '/pause';
  if (paused !== undefined) {
    url += `?state=${paused}`;
  }
  const r = await fetch(url, { method: 'POST' });
  if (!r.ok) throw new Error(`/pause ${r.status}`);
}

export function skip(): void {
  void fetch('/skip', {method: 'POST'});
}

// Step back to the previous show in the current round (navigation only — going
// back never marks anything watched).
export function previous(): void {
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

export async function setVolume(volume: number): Promise<void> {
  const r = await fetch('/volume', {method: 'POST', body: JSON.stringify({volume})});
  if (!r.ok) throw new Error(`/volume ${r.status}`);
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

export async function previewShow(root_path: string): Promise<string[]> {
  const r = await fetch('/library/preview', {
    method: 'POST',
    body: JSON.stringify({root_path}),
  });
  if (!r.ok) {
    const msg = await r.json().catch(() => ({}));
    throw new Error(msg.error || `preview failed (${r.status})`);
  }
  const data = await r.json();
  return data.episodes;
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

export type BrowseItem = {
  name: string;
  path: string;
  is_drive?: boolean;
};

export async function browseDirectory(path?: string): Promise<BrowseItem[]> {
  const url = path ? `/library/browse?path=${encodeURIComponent(path)}` : '/library/browse';
  const r = await fetch(url);
  if (!r.ok) {
    const msg = await r.json().catch(() => ({}));
    throw new Error(msg.error || `browse failed (${r.status})`);
  }
  return r.json();
}

export async function pickFolder(): Promise<string | null> {
  const r = await fetch('/library/pick-folder');
  if (!r.ok) throw new Error(`pick-folder failed (${r.status})`);
  const data = await r.json();
  return data.path;
}

// Subscribe to the desktop status stream. The control server sends an initial
// snapshot immediately, then publishes runner/window/mpv updates as they occur.
export function subscribeStatus(onStatus: (s: Status) => void): () => void {
  let stopped = false;
  const events = new EventSource('/status/stream');

  events.addEventListener('status', (event) => {
    if (stopped) return;
    try {
      onStatus(JSON.parse((event as MessageEvent<string>).data) as Status);
    } catch {
      // Ignore malformed frames; EventSource will keep the stream alive.
    }
  });

  return () => {
    stopped = true;
    events.close();
  };
}

export function minimizeWindow(): void {
  void fetch('/window/minimize', {method: 'POST'});
}

export function maximizeWindow(): void {
  void fetch('/window/maximize', {method: 'POST'});
}

export function closeWindow(): void {
  void fetch('/window/close', {method: 'POST'});
}
