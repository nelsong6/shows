import {spawn} from 'node:child_process';
import http from 'node:http';
import net from 'node:net';

export function identity() {
  return {
    managed: process.env.DEVCTL_MANAGED === '1',
    environment: process.env.DEVCTL_ENVIRONMENT_NAME ?? '',
    project: process.env.DEVCTL_PROJECT ?? '',
    repo_dir: process.env.DEVCTL_REPO_DIR ?? '',
    revision: process.env.DEVCTL_SOURCE_REVISION ?? '',
    configuration_id: process.env.DEVCTL_CONFIGURATION_ID ?? '',
    port: Number(process.env.DEVCTL_FRONTEND_PORT ?? process.env.PORT ?? 0),
    pid: process.pid,
  };
}

function readInfo(port) {
  return new Promise((resolve, reject) => {
    const request = http.get({hostname: '127.0.0.1', port, path: '/remote/info', timeout: 2000}, response => {
      let text = '';
      response.setEncoding('utf8');
      response.on('data', chunk => { text += chunk; if (text.length > 8192) request.destroy(new Error('oversized desktop identity')); });
      response.on('end', () => {
        try {
          const info = JSON.parse(text);
          if (response.statusCode !== 200 || info.application !== 'shows-desktop' || info.protocol !== 1) throw new Error('Target is not the current Shows desktop');
          resolve(info);
        } catch (error) { reject(error); }
      });
    });
    request.on('timeout', () => request.destroy(new Error('Shows identity request timed out')));
    request.on('error', reject);
  });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const listener = net.createServer();
    listener.once('error', reject);
    listener.listen(0, '127.0.0.1', () => {
      const port = listener.address().port;
      listener.close(error => error ? reject(error) : resolve(port));
    });
  });
}

// Vite owns the SSH child; devctl's Job Object owns both processes. Closing the
// preview cannot leave an independently running tunnel or another player.
export async function showsDevelopment() {
  const target = process.env.SHOWS_DEV_TARGET ?? 'local';
  if (!/^[A-Za-z0-9][A-Za-z0-9.-]{0,252}$/.test(target)) throw new Error('SHOWS_DEV_TARGET must be a hostname or local');
  const controlPort = Number(process.env.SHOWS_CONTROL_PORT ?? 45831);
  if (!Number.isInteger(controlPort) || controlPort < 1 || controlPort > 65535) throw new Error('Invalid Shows control port');
  const port = target === 'local' ? controlPort : await freePort();
  let tunnel;
  let desktop;
  let closing = false;
  let failure;
  return {
    name: 'shows-development',
    apply: 'serve',
    config() {
      return {server: {host: '127.0.0.1', proxy: {
        '^/(status|shows|stats|history|health|pause|skip|prev|defer|seek|volume|sub|audio|fullscreen|stay-on-top|window|library|round|play-show|remote)(/|\\?|$)': {
          target: `http://127.0.0.1:${port}`,
          proxyTimeout: 0,
          timeout: 0,
        },
      }}};
    },
    configureServer(server) {
      const native = process.env.SHOWS_DEV_DESKTOP_EXE;
      if (native) {
        if (target !== 'local') throw new Error('Native preview must use the local engine');
        server.httpServer?.once('listening', () => {
          desktop = spawn(native, [], {env: {...process.env, SHOWS_DEV_URL: `http://127.0.0.1:${server.config.server.port}/`}, windowsHide: true, stdio: ['ignore','inherit','inherit']});
          desktop.once('error', error => { failure = error.message; });
        });
      }
      if (target !== 'local') {
        tunnel = spawn(process.platform === 'win32' ? 'ssh.exe' : 'ssh', [
          '-N', '-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes',
          '-o', 'ExitOnForwardFailure=yes', '-o', 'ConnectTimeout=8',
          '-o', 'ServerAliveInterval=5', '-o', 'ServerAliveCountMax=2',
          '-L', `127.0.0.1:${port}:127.0.0.1:${controlPort}`, target,
        ], {windowsHide: true, stdio: ['ignore', 'ignore', 'pipe']});
        tunnel.stderr.setEncoding('utf8');
        tunnel.stderr.on('data', text => { failure = text.trim().slice(-8192); });
        tunnel.once('error', error => { failure = error.message; });
        tunnel.once('exit', () => {
          if (!closing) {
            failure ??= 'SSH connection closed';
            server.config.logger.error(`Shows preview lost ${target}: ${failure}`);
            server.close().finally(() => process.exit(1));
          }
        });
      }
      server.httpServer?.once('close', () => { closing = true; tunnel?.kill(); });
      server.middlewares.use(async (request, response, next) => {
        const path = request.url?.split('?')[0];
        if (target !== 'local' && /^\/(window\/|fullscreen$|stay-on-top$)/.test(path ?? '')) {
          response.statusCode = 409;
          response.setHeader('Content-Type', 'application/json');
          response.end(JSON.stringify({ok: false, message: 'Window controls require the native desktop preview'}));
          return;
        }
        if (path !== '/__devctl/health' && path !== '/api/__devctl/health') return next();
        response.setHeader('Content-Type', 'application/json');
        response.setHeader('Cache-Control', 'no-store');
        try {
          const body = {...identity(), target, nativeDesktop: Boolean(native)};
          if (path === '/api/__devctl/health') {
            if (failure) throw new Error(failure);
            body.desktop = await readInfo(port);
            // This PID identifies the managed local transport to the desktop.
            body.pid = target === 'local' ? body.desktop.pid : tunnel.pid;
          }
          response.end(JSON.stringify(body));
        } catch (error) {
          response.statusCode = 503;
          response.end(JSON.stringify({...identity(), target, error: error.message}));
        }
      });
    },
    transformIndexHtml() {
      return [{tag: 'script', children: `document.title = ${JSON.stringify(`Shows preview — ${target}`)};`}];
    },
  };
}
