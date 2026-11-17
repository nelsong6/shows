// Downloads the latest fzt-browser release into public/fzt, where Vite serves it in
// development and copies it into the embedded bundle. The release's wasm_exec.js
// comes from the Go toolchain that built its fzt.wasm, so the two always match.
import {execFileSync} from 'node:child_process';
import {mkdir, rm, writeFile} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const ASSETS = [
  'fzt.wasm',
  'fzt-terminal.js',
  'fzt-terminal.css',
  'fzt-web.js',
  'wasm_exec.js',
  'PerfectDOSVGA437.ttf',
  'SymbolsNerdFontMono-Regular.ttf',
];
const API = 'https://forgejo.romaine.life/api/v1/repos/romaine-life/fzt-browser';
const target = join(dirname(fileURLToPath(import.meta.url)), '..', 'public', 'fzt');

// CI's suites pass their Forgejo read token as PRIVATE_GIT_TOKEN; on a home PC the Forgejo
// credential git already holds works.
function token() {
  if (process.env.PRIVATE_GIT_TOKEN) return process.env.PRIVATE_GIT_TOKEN;
  try {
    const filled = execFileSync('git', ['credential', 'fill'], {
      input: 'protocol=https\nhost=forgejo.romaine.life\n\n',
      encoding: 'utf8',
      stdio: ['pipe', 'pipe', 'ignore'],
      env: {...process.env, GIT_TERMINAL_PROMPT: '0'},
    });
    const password = filled.split('\n').find((line) => line.startsWith('password='));
    if (password) return password.slice('password='.length);
  } catch {
    // Reported below.
  }
  throw new Error('fetch-fzt needs PRIVATE_GIT_TOKEN or a Forgejo credential in git to read fzt-browser releases');
}

const headers = {Authorization: `token ${token()}`};
const response = await fetch(`${API}/releases/latest`, {headers});
if (!response.ok) throw new Error(`fzt-browser release lookup: HTTP ${response.status}`);
const release = await response.json();

await rm(target, {recursive: true, force: true});
await mkdir(target, {recursive: true});
for (const name of ASSETS) {
  const asset = release.assets.find((a) => a.name === name);
  if (!asset) throw new Error(`fzt-browser ${release.tag_name} has no ${name}`);
  const download = await fetch(asset.browser_download_url, {headers});
  if (!download.ok) throw new Error(`${name}: HTTP ${download.status}`);
  const bytes = Buffer.from(await download.arrayBuffer());
  if (bytes.length !== asset.size) throw new Error(`${name}: downloaded ${bytes.length} of ${asset.size} bytes`);
  await writeFile(join(target, name), bytes);
}
await writeFile(join(target, 'version.json'), JSON.stringify({release: release.tag_name}) + '\n');
console.log(`Fetched fzt-browser ${release.tag_name} into public/fzt`);
