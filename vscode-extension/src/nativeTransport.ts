import { spawn, execFile, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createConnection } from 'node:net';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve, posix, win32 } from 'node:path';
import { Duplex } from 'node:stream';
import { promisify } from 'node:util';

import type { CancellationLike, ClientSettings, DisposableLike } from './types.js';

const CONNECT_TIMEOUT_MS = 5_000;
const BRIDGE_READY = Buffer.from('CCTXN1\n', 'ascii');
const execFileAsync = promisify(execFile);

export class RequestCancelledError extends Error {
  public constructor() {
    super('Code Context native tool request was cancelled.');
    this.name = 'RequestCancelledError';
  }
}

export function defaultSocketPath(): string {
  return process.env.CCTX_HOME
    ? join(process.env.CCTX_HOME, 'daemon.sock')
    : join(homedir(), '.local/state/code-context/daemon.sock');
}

export function normalizeSocketPath(configured: string): string {
  const value = configured.trim();
  if (value.length === 0) {
    return defaultSocketPath();
  }
  if (value.startsWith('~/')) {
    return join(homedir(), value.slice(2));
  }
  if (!isAbsolute(value)) {
    throw new Error('codeContextNative.socketPath must be absolute.');
  }
  return resolve(value);
}

export function defaultExecutablePath(platform = process.platform): string {
  return join(homedir(), '.local/bin', platform === 'win32' ? 'cctx.exe' : 'cctx');
}

export function normalizeExecutablePath(configured = '', platform = process.platform): string {
  const value = configured.trim();
  const target =
    value.length === 0
      ? defaultExecutablePath(platform)
      : value.startsWith('~/') || (platform === 'win32' && value.startsWith('~\\'))
        ? join(homedir(), value.slice(2))
        : value;
  const absolute = platform === 'win32' ? win32.isAbsolute(target) : isAbsolute(target);
  if (!absolute || /[\0\r\n]/u.test(target)) {
    throw new Error('codeContextNative.executablePath must be absolute.');
  }
  return platform === 'win32' ? win32.resolve(target) : resolve(target);
}

export function usesWslTransport(settings: ClientSettings, platform = process.platform): boolean {
  return (
    platform === 'win32' &&
    Boolean((settings.wslDistribution ?? '').trim() || (settings.wslExecutablePath ?? '').trim())
  );
}

export function windowsLocalLaunch(
  settings: ClientSettings,
  operation: 'bridge' | 'service' = 'bridge',
): { command: string; args: string[] } {
  const command = normalizeExecutablePath(settings.executablePath, 'win32');
  return {
    command,
    args: [operation === 'bridge' ? 'native-bridge' : 'daemon'],
  };
}

export function wslLaunch(
  settings: ClientSettings,
  operation: 'bridge' | 'service' = 'bridge',
): { command: string; args: string[] } {
  const distribution = settings.wslDistribution ?? '';
  const executable = settings.wslExecutablePath ?? '';
  if (!distribution || distribution !== distribution.trim()
      || distribution.startsWith('-') || /[\0\r\n]/u.test(distribution)) {
    throw new Error('Configure codeContextNative.wslDistribution for this Windows window.');
  }
  if (!posix.isAbsolute(executable) || /[\0\r\n]/u.test(executable)) {
    throw new Error('codeContextNative.wslExecutablePath must be an absolute Linux cctx path.');
  }
  if (settings.socketPath.trim()) {
    throw new Error('Windows WSL native IPC uses the WSL daemon runtime; leave socketPath empty.');
  }
  const command = win32.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'wsl.exe');
  return {
    command,
    args: ['--distribution', distribution, '--exec', ...(operation === 'bridge'
      ? [executable, 'native-bridge']
      : ['systemctl', '--user', 'start', 'code-context.service'])],
  };
}

export function nativeEndpoint(settings: ClientSettings, platform = process.platform): string {
  if (platform === 'win32') {
    if (usesWslTransport(settings, platform)) {
      return `wsl:${settings.wslDistribution ?? ''}:${settings.wslExecutablePath ?? ''}:${settings.socketPath}`;
    }
    return `win32:${normalizeExecutablePath(settings.executablePath, 'win32')}:${settings.socketPath}`;
  }
  return normalizeSocketPath(settings.socketPath);
}

type BridgeLauncher = (command: string, args: string[]) => ChildProcessWithoutNullStreams;

export async function connectNativeTransport(
  settings: ClientSettings,
  cancellation: CancellationLike,
  platform = process.platform,
  launcher: BridgeLauncher = (command, args) =>
    spawn(command, args, { stdio: 'pipe', windowsHide: true }),
): Promise<Duplex> {
  if (cancellation.isCancellationRequested) {
    throw new RequestCancelledError();
  }
  if (platform === 'win32') {
    const launch = usesWslTransport(settings, platform)
      ? wslLaunch(settings)
      : windowsLocalLaunch(settings);
    return new Promise<Duplex>((resolveConnect, rejectConnect) => {
      const child = launcher(launch.command, launch.args);
      let transport: Duplex | undefined;
      let ready = Buffer.alloc(0);
      let stderr = '';
      let settled = false;
      const resources: { timer?: NodeJS.Timeout; listener?: DisposableLike } = {};
      const cleanup = (): void => {
        clearTimeout(resources.timer);
        resources.listener?.dispose();
        child.stdout.off('data', onReady);
      };
      const fail = (error: Error): void => {
        if (transport) {
          transport.destroy(error);
        } else if (!settled) {
          settled = true;
          cleanup();
          child.kill();
          rejectConnect(error);
        }
      };
      const onReady = (chunk: Buffer): void => {
        ready = Buffer.concat([ready, chunk]);
        if (ready.length > BRIDGE_READY.length
            || !BRIDGE_READY.subarray(0, ready.length).equals(ready)) {
          fail(new Error('WSL native IPC bridge returned an invalid readiness frame.'));
          return;
        }
        if (ready.length === BRIDGE_READY.length) {
          settled = true;
          cleanup();
          child.stdout.pause();
          transport = Duplex.from({ readable: child.stdout, writable: child.stdin });
          transport.once('close', () => {
            if (child.exitCode === null) {
              child.kill();
            }
          });
          resolveConnect(transport);
        }
      };
      child.stderr.on('data', (chunk: Buffer) => {
        stderr = (stderr + chunk.toString('utf8')).slice(-8192);
      });
      child.once('error', fail);
      child.once('close', (code, signal) => {
        fail(new Error(`WSL native IPC bridge closed (${code ?? signal}): ${stderr.trim()}`));
      });
      child.stdout.on('data', onReady);
      resources.timer = setTimeout(() => {
        fail(new Error('Timed out waiting for the WSL native IPC bridge.'));
      }, CONNECT_TIMEOUT_MS);
      resources.listener = cancellation.onCancellationRequested(() => {
        fail(new RequestCancelledError());
      });
    });
  }
  if (platform !== 'linux') {
    throw new Error('Native Code Context requires Linux or the configured Windows WSL bridge.');
  }
  return new Promise<Duplex>((resolveConnect, rejectConnect) => {
    const socket = createConnection(normalizeSocketPath(settings.socketPath));
    const resources: { timer?: NodeJS.Timeout; listener?: DisposableLike } = {};
    const cleanup = (): void => {
      clearTimeout(resources.timer);
      resources.listener?.dispose();
      socket.off('error', onError);
    };
    const onError = (error: Error): void => {
      cleanup();
      socket.destroy();
      rejectConnect(error);
    };
    resources.timer = setTimeout(() => {
      onError(new Error(`Timed out connecting to ${settings.socketPath}`));
    }, CONNECT_TIMEOUT_MS);
    resources.listener = cancellation.onCancellationRequested(() => {
      onError(new RequestCancelledError());
    });
    socket.once('error', onError);
    socket.once('connect', () => {
      cleanup();
      socket.setNoDelay(true);
      resolveConnect(socket);
    });
  });
}

export async function startNativeService(settings: ClientSettings): Promise<void> {
  if (process.platform === 'win32') {
    if (usesWslTransport(settings, 'win32')) {
      const launch = wslLaunch(settings, 'service');
      await execFileAsync(launch.command, launch.args, { timeout: 10_000, windowsHide: true });
    } else {
      const launch = windowsLocalLaunch(settings, 'service');
      const child = spawn(launch.command, launch.args, {
        stdio: 'ignore',
        windowsHide: true,
      });
      child.unref();
    }
  } else if (process.platform === 'linux') {
    await execFileAsync('systemctl', ['--user', 'start', 'code-context.service'], {
      timeout: 10_000, windowsHide: true,
    });
  } else {
    throw new Error('Cannot start code-context.service on this host.');
  }
}
