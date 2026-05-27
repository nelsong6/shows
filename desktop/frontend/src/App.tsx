import { useEffect, useState } from 'react';
import { GetStatus } from '../wailsjs/go/main/App';
import { EventsOn } from '../wailsjs/runtime/runtime';
import './App.css';

// Phase 2 status surface. mpv covers the Wails window once playback
// starts; before that, this view shows the auth + fetch progression
// so the user knows what's happening. Phase 3 replaces this with the
// real library UI rendered as an overlay on top of the libmpv render
// surface.
type Status = {
  phase: 'initializing' | 'auth' | 'fetching' | 'playing' | 'drained' | 'error';
  message: string;
};

function App() {
  const [status, setStatus] = useState<Status>({ phase: 'initializing', message: '' });

  useEffect(() => {
    GetStatus().then((s: any) => setStatus(s));
    const unsub = EventsOn('status', (s: any) => setStatus(s));
    return () => unsub();
  }, []);

  return (
    <div
      style={{
        background: '#0a0a0a',
        color: '#eee',
        fontFamily: 'monospace',
        minHeight: '100vh',
        padding: 32,
      }}
    >
      <h2 style={{ textTransform: 'uppercase', letterSpacing: '0.05em', color: '#888' }}>shows</h2>
      <div style={{ marginTop: 24 }}>
        <span style={{ color: '#666' }}>phase: </span>
        <span style={{ color: '#4ade80' }}>{status.phase}</span>
      </div>
      <div style={{ marginTop: 8, color: '#aaa' }}>{status.message}</div>
      {status.phase === 'auth' && (
        <div style={{ marginTop: 24, color: '#888' }}>
          a browser tab should have opened. approve there, then come back.
        </div>
      )}
    </div>
  );
}

export default App;
