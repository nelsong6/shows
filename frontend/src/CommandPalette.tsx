import {useEffect, useRef, useState} from 'react';

import {getInfo, type AppInfo} from './api';
import {commandFromUrl, paletteYaml, type Command} from './commands';

// The `/` command palette: fzt's browser build (fzt-browser), fetched into public/fzt
// at build time and loaded the first time the palette opens. One fzt runtime serves
// every opening, so its container stays mounted and is only hidden.

type FztTerminal = {
  initWasm(): Promise<void>;
  loadYAML(yaml: string): boolean | undefined;
  hidePalette(): void;
  init(): boolean | undefined;
  setActive(active: boolean): void;
};

type FztWebModule = {
  createFztWeb(
    container: HTMLElement,
    options: {wasmUrl: string; onAction: (action: string, url?: string) => void},
  ): FztTerminal;
};

function fztUrl(file: string): string {
  return new URL(`fzt/${file}`, document.baseURI).href;
}

function loadStyles(): Promise<void> {
  return new Promise((resolve, reject) => {
    const link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = fztUrl('fzt-terminal.css');
    link.onload = () => resolve();
    link.onerror = () => reject(new Error('fzt-terminal.css did not load'));
    document.head.appendChild(link);
  });
}

async function loadFzt(container: HTMLElement, onAction: (action: string, url?: string) => void): Promise<FztTerminal> {
  await loadStyles();
  // wasm_exec.js defines the Go runtime fzt.wasm runs on; fzt-web.js expects it loaded.
  await import(/* @vite-ignore */ fztUrl('wasm_exec.js'));
  const web = (await import(/* @vite-ignore */ fztUrl('fzt-web.js'))) as FztWebModule;
  const terminal = web.createFztWeb(container, {wasmUrl: fztUrl('fzt.wasm'), onAction});
  await terminal.initWasm();
  terminal.hidePalette();
  return terminal;
}

function describeLoadFailure(e: unknown): string {
  const reason = e instanceof Error ? e.message : String(e);
  return `The command palette could not load fzt: ${reason}`;
}

export type PaletteAnchor = {left: number; top: number} | null;

export function CommandPalette({
  open,
  anchor,
  commands,
  onRun,
  onClose,
}: {
  open: boolean;
  anchor: PaletteAnchor;
  commands: Command[];
  onRun: (id: string) => void;
  onClose: () => void;
}) {
  const container = useRef<HTMLPreElement>(null);
  const terminal = useRef<Promise<FztTerminal> | null>(null);
  const handlers = useRef({onRun, onClose});
  handlers.current = {onRun, onClose};
  const [failure, setFailure] = useState<string | null>(null);

  useEffect(() => {
    if (!open || !container.current) return;
    let current = true;
    setFailure(null);
    terminal.current ??= loadFzt(container.current, (action, url) => {
      const id = commandFromUrl(url);
      if (id) {
        handlers.current.onClose();
        handlers.current.onRun(id);
      } else if (action === 'cancel') {
        handlers.current.onClose();
      }
    });
    terminal.current.then(
      (fzt) => {
        if (!current) return;
        // A fresh session each opening, so the list matches what is playing now.
        if (!fzt.loadYAML(paletteYaml(commands))) {
          setFailure('The command palette could not read its commands');
          return;
        }
        if (!fzt.init()) setFailure('The command palette could not start fzt');
      },
      (e) => {
        // The next opening tries again rather than repeating this failure.
        terminal.current = null;
        if (current) setFailure(describeLoadFailure(e));
      },
    );
    return () => {
      current = false;
      void terminal.current?.then((fzt) => fzt.setActive(false), () => {});
    };
    // Commands are captured when the palette opens; it does not redraw under the person.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!failure) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [failure, onClose]);

  const style = anchor ? {left: anchor.left, top: anchor.top, transform: 'none'} : undefined;
  return (
    <div className={`palette-backdrop${open ? '' : ' hidden'}`} onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="palette fzt-terminal-window" style={style} role="dialog" aria-label="Commands">
        <pre ref={container} className="palette-terminal" />
        {failure && (
          <div className="palette-failure" role="alert">
            {failure}
          </div>
        )}
      </div>
    </div>
  );
}

function pacific(unix: number): string {
  return new Intl.DateTimeFormat('en-US', {
    timeZone: 'America/Los_Angeles',
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  }).format(new Date(unix * 1000)) + ' Pacific';
}

function ago(unix: number): string {
  const seconds = Date.now() / 1000 - unix;
  const count = (n: number, unit: string) => `${n} ${unit}${n === 1 ? '' : 's'} ago`;
  if (seconds < -300) return "this PC's clock is behind the build";
  if (seconds < 60) return 'just now';
  if (seconds < 3600) return count(Math.floor(seconds / 60), 'minute');
  if (seconds < 86400) return count(Math.floor(seconds / 3600), 'hour');
  return count(Math.floor(seconds / 86400), 'day');
}

export function InfoPanel({onClose}: {onClose: () => void}) {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [fzt, setFzt] = useState<string>('reading…');

  useEffect(() => {
    getInfo().then(setInfo, (e) => setFailure(e instanceof Error ? e.message : String(e)));
    fetch(fztUrl('version.json'))
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((v: {release: string}) => setFzt(v.release))
      .catch((e) => setFzt(`could not read its version: ${e instanceof Error ? e.message : e}`));
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  const rows: [string, string][] = info
    ? [
        ['Repo', info.repo_url ?? info.repo],
        ['Version', info.version],
        ['Built', info.built_unix === null ? info.built : `${pacific(info.built_unix)} (${ago(info.built_unix)})`],
        ['Installed at', info.executable],
        ['Checkout', info.checkout],
        ['Palette', `fzt-browser ${fzt}`],
      ]
    : [];
  return (
    <div className="palette-backdrop" onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="info-panel" role="dialog" aria-label="About shows">
        <h3>shows</h3>
        {failure ? (
          <p className="palette-failure">Could not read this app's details: {failure}</p>
        ) : !info ? (
          <p className="info-loading">Reading…</p>
        ) : (
          <dl>
            {rows.map(([label, value]) => (
              <div key={label} className="info-row">
                <dt>{label}</dt>
                <dd>{value}</dd>
              </div>
            ))}
          </dl>
        )}
      </div>
    </div>
  );
}
