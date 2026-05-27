import { useState } from 'react';
import { PlayTestFile } from '../wailsjs/go/main/App';
import './App.css';

// Phase 1b smoke-test UI. A single input + button that asks the Go
// backend to spawn libmpv on the given path. Replaced in Phase 3 by a
// real library view that talks to shows.romaine.life.
function App() {
  const [path, setPath] = useState(
    'D:\\Downloads\\Group-Nelson\\Dr. Katz, Professional Therapist\\Dr. Katz S06\\Dr.Katz.S06E11.Big.TV.avi',
  );
  const [status, setStatus] = useState<string>('idle');

  async function play() {
    setStatus('starting libmpv…');
    const err = await PlayTestFile(path);
    setStatus(err === '' ? 'playing' : `error: ${err}`);
  }

  return (
    <div style={{ padding: 24, fontFamily: 'monospace', color: '#eee', background: '#0a0a0a', minHeight: '100vh' }}>
      <h2 style={{ textTransform: 'uppercase', letterSpacing: '0.05em', color: '#888' }}>shows — phase 1b smoke test</h2>
      <p style={{ color: '#888' }}>load a file via libmpv. mpv currently opens its own window; reparenting into this window is phase 1c.</p>
      <input
        value={path}
        onChange={(e) => setPath(e.target.value)}
        style={{ width: '100%', padding: 8, background: '#171717', color: '#eee', border: '1px solid #333', fontFamily: 'monospace' }}
      />
      <button
        onClick={play}
        style={{ marginTop: 12, padding: '8px 16px', background: '#171717', color: '#eee', border: '1px solid #4ade80', fontFamily: 'monospace', textTransform: 'lowercase', cursor: 'pointer' }}
      >
        play
      </button>
      <div style={{ marginTop: 24, color: '#888' }}>status: {status}</div>
    </div>
  );
}

export default App;
