// Data layer. The desktop client's control server exposes a same-origin HTTP
// surface — GET /status, GET /shows, GET /history?show=<id>, POST /pause,
// POST /skip — and serves this page, so all of these are relative to its
// origin.

export type RoundEntry = {
  show_id: string;
  show_name: string;
  episode_id: string;
  absolute_path: string;
  order_value: number;
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

export type Status = {
  phase: Phase;
  message: string;
  playlist: string;
  round?: RoundEntry[];
  // Index into `round` of the entry currently playing (mpv playlist-pos).
  round_pos?: number;
  last_advance?: AdvanceResult;
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

export function pause(): void {
  void fetch('/pause', {method: 'POST'});
}

export function skip(): void {
  void fetch('/skip', {method: 'POST'});
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
