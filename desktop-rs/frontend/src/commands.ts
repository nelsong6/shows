// The overlay's commands: what the `/` palette lists and what each shortcut runs.
// Every shortcut is an accelerator for one of these, so the palette is the one place
// a person finds both. Pure so the list and the key map are tested without a browser.

export type CommandState = {
  roundActive: boolean;
  hasPlayback: boolean;
  paused: boolean;
  hasSubtitles: boolean;
  captionsOn: boolean;
  onTop: boolean;
  fullscreen: boolean;
  maximized: boolean;
  dashboardOpen: boolean;
  playlist: string;
  playlists: string[];
  round: {show_id: string; show_name: string; playlist?: string}[];
  pos: number;
};

export type Command = {
  id: string;
  name: string;
  description: string;
  keys?: string;
  // Why the command can't run now; it stays listed so the palette can say so.
  unavailable?: string;
};

// Shortcut keys (KeyboardEvent.key values) and how a row names them.
const SHORTCUTS: {id: string; keys: string[]; label: string}[] = [
  {id: 'pause', keys: [' '], label: 'Space'},
  {id: 'next', keys: ['n'], label: 'n'},
  {id: 'previous', keys: ['p'], label: 'p'},
  {id: 'defer', keys: ['d'], label: 'd'},
  {id: 'back', keys: ['ArrowLeft', 'j'], label: '← or j'},
  {id: 'forward', keys: ['ArrowRight', 'l'], label: '→ or l'},
  {id: 'louder', keys: ['ArrowUp'], label: '↑'},
  {id: 'quieter', keys: ['ArrowDown'], label: '↓'},
  {id: 'captions', keys: ['c'], label: 'c'},
  {id: 'fullscreen', keys: ['f'], label: 'f'},
  {id: 'pin', keys: ['i'], label: 'i'},
  {id: 'dashboard', keys: ['Tab'], label: 'Tab'},
  {id: 'hide-dashboard', keys: ['Escape'], label: 'Esc'},
];

export const PALETTE_KEY = '/';

export function keyCommand(key: string): string | null {
  return SHORTCUTS.find((s) => s.keys.includes(key))?.id ?? null;
}

function label(id: string): string | undefined {
  return SHORTCUTS.find((s) => s.id === id)?.label;
}

const NOTHING_PLAYING = 'nothing is playing';

export function paletteCommands(s: CommandState): Command[] {
  const round = s.roundActive ? undefined : NOTHING_PLAYING;
  const playback = s.hasPlayback ? undefined : NOTHING_PLAYING;
  const commands: Command[] = [
    {id: 'pause', name: s.paused ? 'Play' : 'Pause', description: s.paused ? 'Resume playback' : 'Pause playback', unavailable: round},
    {id: 'next', name: 'Next show', description: 'Mark this episode watched and play the next show', unavailable: round},
    {id: 'previous', name: 'Previous show', description: 'Go back to the previous show in this round', unavailable: round},
    {id: 'defer', name: 'Defer episode', description: 'Play the next show and leave this episode unwatched', unavailable: round},
    {id: 'back', name: 'Seek back 10 seconds', description: 'Rewind this episode', unavailable: playback},
    {id: 'forward', name: 'Seek forward 10 seconds', description: 'Skip ahead in this episode', unavailable: playback},
    {id: 'louder', name: 'Volume up', description: 'Raise the volume by 5', unavailable: playback},
    {id: 'quieter', name: 'Volume down', description: 'Lower the volume by 5', unavailable: playback},
    {
      id: 'captions',
      name: s.captionsOn ? 'Turn captions off' : 'Turn captions on',
      description: 'Subtitles for this episode',
      unavailable: playback ?? (s.hasSubtitles ? undefined : 'this episode has no subtitles'),
    },
    {id: 'fullscreen', name: s.fullscreen ? 'Exit fullscreen' : 'Fullscreen', description: 'Fill the screen with the video'},
    {id: 'pin', name: s.onTop ? 'Stop staying on top' : 'Stay on top', description: 'Keep the window above other windows'},
    {id: 'dashboard', name: s.dashboardOpen ? 'Hide dashboard' : 'Show dashboard', description: 'The round, stats, library and settings'},
    {id: 'minimize', name: 'Minimize window', description: 'Send shows to the taskbar'},
    {id: 'maximize', name: s.maximized ? 'Restore window size' : 'Maximize window', description: 'Resize the window'},
    {id: 'close', name: 'Close shows', description: 'Quit the app'},
    {id: 'info', name: 'info', description: "This app's repo, version, build time and install path"},
  ];
  for (const playlist of s.playlists) {
    if (playlist === s.playlist) continue;
    commands.push({
      id: `playlist:${playlist}`,
      name: `Play the ${playlist} playlist`,
      description: 'Switch playlists; it resumes where it was left',
    });
  }
  s.round.forEach((entry, index) => {
    if (index === s.pos) return;
    commands.push({
      id: `show:${entry.show_id}`,
      name: `Play ${entry.show_name}`,
      description: entry.playlist ? `From this round, in ${entry.playlist}` : 'From this round',
      unavailable: round,
    });
  });
  for (const command of commands) {
    command.keys = command.id === 'dashboard' && s.dashboardOpen ? 'Tab or Esc' : label(command.id);
  }
  return commands;
}

// The row fzt shows: the key and, for a command that can't run, why.
export function rowDescription(command: Command): string {
  const what = command.unavailable ? `Unavailable: ${command.unavailable}` : command.description;
  return command.keys ? `${what} · ${command.keys}` : what;
}

// fzt reads YAML; JSON is YAML, and JSON.stringify escapes every name safely.
export function paletteYaml(commands: Command[]): string {
  return JSON.stringify(
    commands.map((c) => ({name: c.name, description: rowDescription(c), url: `shows:${c.id}`})),
  );
}

export function commandFromUrl(url: string | undefined): string | null {
  return url?.startsWith('shows:') ? url.slice('shows:'.length) : null;
}
