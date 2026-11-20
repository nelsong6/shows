import assert from 'node:assert/strict';
import test from 'node:test';

import {commandFromUrl, keyCommand, paletteCommands, paletteYaml, rowDescription} from '../.test-dist/commands.js';

const playing = {
  roundActive: true,
  hasPlayback: true,
  paused: false,
  hasSubtitles: true,
  captionsOn: false,
  onTop: false,
  fullscreen: false,
  maximized: false,
  dashboardOpen: false,
  databaseReachable: true,
  playlist: 'nelson',
  playlists: ['nelson', 'kids'],
  round: [
    {show_id: 'a', show_name: 'Alpha'},
    {show_id: 'b', show_name: 'Beta', playlist: 'kids'},
  ],
  pos: 0,
};

const KEYS = [' ', 'n', 'p', 'd', 'ArrowLeft', 'j', 'ArrowRight', 'l', 'ArrowUp', 'ArrowDown', 'c', 'f', 'i', 'Tab', 'Escape'];

test('every shortcut runs a command the palette lists with that key', () => {
  for (const dashboardOpen of [false, true]) {
    const listed = paletteCommands({...playing, dashboardOpen});
    for (const key of KEYS) {
      const id = keyCommand(key);
      assert.ok(id, `${key} has a command`);
      // Esc only hides the dashboard, so it is named on the dashboard row while open.
      if (id === 'hide-dashboard') {
        if (!dashboardOpen) continue;
        assert.equal(listed.find((c) => c.id === 'dashboard').keys, 'Tab or Esc');
        continue;
      }
      const command = listed.find((c) => c.id === id);
      assert.ok(command, `${key} runs ${id}, which the palette lists`);
      assert.ok(command.keys, `${id} names its key`);
    }
  }
  assert.equal(keyCommand('/'), null);
  assert.equal(keyCommand('x'), null);
});

test('rows name their key and explain what cannot run', () => {
  const idle = paletteCommands({...playing, roundActive: false, hasPlayback: false, round: []});
  const next = idle.find((c) => c.id === 'next');
  assert.equal(next.unavailable, 'nothing is playing');
  assert.equal(rowDescription(next), 'Unavailable: nothing is playing · n');
  assert.equal(idle.find((c) => c.id === 'fullscreen').unavailable, undefined);

  const noSubs = paletteCommands({...playing, hasSubtitles: false});
  assert.equal(noSubs.find((c) => c.id === 'captions').unavailable, 'this episode has no subtitles');
  assert.equal(rowDescription(paletteCommands(playing).find((c) => c.id === 'pause')), 'Pause playback · Space');
});

test('catch up runs whenever the database answers', () => {
  const idle = paletteCommands({...playing, roundActive: false, hasPlayback: false, round: []});
  assert.equal(idle.find((c) => c.id === 'catch-up').unavailable, undefined, 'a drained or blocked playlist can catch up');
  const offline = paletteCommands({...playing, databaseReachable: false});
  assert.equal(offline.find((c) => c.id === 'catch-up').unavailable, 'the NAS database is unreachable');
});

test('names follow the current state', () => {
  const s = paletteCommands({...playing, paused: true, onTop: true, fullscreen: true, captionsOn: true, dashboardOpen: true});
  const name = (id) => s.find((c) => c.id === id).name;
  assert.equal(name('pause'), 'Play');
  assert.equal(name('pin'), 'Stop staying on top');
  assert.equal(name('fullscreen'), 'Exit fullscreen');
  assert.equal(name('captions'), 'Turn captions off');
  assert.equal(name('dashboard'), 'Hide dashboard');
});

test('other playlists and the rest of the round are commands', () => {
  const ids = paletteCommands(playing).map((c) => c.id);
  assert.ok(ids.includes('playlist:kids'));
  assert.ok(!ids.includes('playlist:nelson'), 'the playing playlist is not offered');
  assert.ok(ids.includes('show:b'));
  assert.ok(!ids.includes('show:a'), 'the playing show is not offered');
  assert.ok(ids.includes('info'));
});

test('the YAML fzt loads carries each command back as its url', () => {
  const commands = paletteCommands({...playing, playlists: ['nelson', 'say "hi": #1']});
  const rows = JSON.parse(paletteYaml(commands));
  assert.equal(rows.length, commands.length);
  const row = rows.find((r) => r.url === 'shows:playlist:say "hi": #1');
  assert.equal(row.name, 'Play the say "hi": #1 playlist');
  assert.equal(commandFromUrl(row.url), 'playlist:say "hi": #1');
  assert.equal(commandFromUrl('https://example.com'), null);
  assert.equal(commandFromUrl(undefined), null);
});
