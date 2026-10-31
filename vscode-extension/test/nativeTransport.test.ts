import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { once } from 'node:events';
import test from 'node:test';

import {
  connectNativeTransport, nativeEndpoint, RequestCancelledError, usesWslTransport, windowsLocalLaunch, wslLaunch,
} from '../src/nativeTransport.js';
import { resolveWslRoot, wslCandidateToLinuxPath } from '../src/rootResolver.js';
import type { CancellationLike, ClientSettings, DisposableLike } from '../src/types.js';

const settings: ClientSettings = {
  socketPath: '', autoStartService: false, requestTimeoutMs: 1000,
  wslDistribution: 'Ubuntu-22.04', wslExecutablePath: '/home/test/.local/bin/cctx',
};

function cancellation(): CancellationLike & { cancel(): void } {
  const listeners = new Set<() => void>();
  let cancelled = false;
  return {
    get isCancellationRequested(): boolean { return cancelled; },
    onCancellationRequested(listener: () => void): DisposableLike {
      listeners.add(listener);
      return { dispose: (): void => { listeners.delete(listener); } };
    },
    cancel(): void {
      cancelled = true;
      for (const listener of listeners) { listener(); }
    },
  };
}

function launcher(script: string): (command: string, args: string[]) => ChildProcessWithoutNullStreams {
  return () => spawn(process.execPath, ['-e', script], { stdio: 'pipe' });
}

void test('WSL launch uses native IPC without a shell, MCP or a network port', () => {
  const launch = wslLaunch(settings);
  assert.deepEqual(launch.args, [
    '--distribution', 'Ubuntu-22.04', '--exec', '/home/test/.local/bin/cctx', 'native-bridge',
  ]);
  assert.ok(launch.command.endsWith('\\System32\\wsl.exe'));
  assert.ok(nativeEndpoint(settings, 'win32').includes('Ubuntu-22.04'));
  assert.throws(() => wslLaunch({ ...settings, wslDistribution: '--shutdown' }));
  assert.throws(() => wslLaunch({ ...settings, wslDistribution: 'bad\nname' }));
  assert.throws(() => wslLaunch({ ...settings, wslExecutablePath: 'relative/cctx' }));
  assert.throws(() => wslLaunch({ ...settings, socketPath: '/other/daemon.sock' }));
});

void test('Windows without WSL configuration launches local cctx.exe native-bridge', () => {
  const localSettings: ClientSettings = {
    socketPath: '', autoStartService: true, requestTimeoutMs: 1000,
    executablePath: 'C:\\Users\\test\\.local\\bin\\cctx.exe',
    wslDistribution: '', wslExecutablePath: '',
  };
  assert.equal(usesWslTransport(localSettings, 'win32'), false);
  assert.equal(usesWslTransport(settings, 'win32'), true);
  assert.deepEqual(windowsLocalLaunch(localSettings, 'bridge'), {
    command: 'C:\\Users\\test\\.local\\bin\\cctx.exe',
    args: ['native-bridge'],
  });
  assert.deepEqual(windowsLocalLaunch(localSettings, 'service'), {
    command: 'C:\\Users\\test\\.local\\bin\\cctx.exe',
    args: ['daemon'],
  });
  assert.ok(nativeEndpoint(localSettings, 'win32').startsWith('win32:C:\\Users\\test\\.local\\bin\\cctx.exe:'));
});

void test('WSL readiness may be fragmented and the connected stream is bidirectional', async () => {
  const stream = await connectNativeTransport(settings, cancellation(), 'win32', launcher(`
    process.stdout.write('CCT');
    setTimeout(() => process.stdout.write('XN1\\n'), 10);
    process.stdin.on('data', data => process.stdout.write(data));
    process.stdin.on('end', () => process.exit(0));
  `));
  try {
    const received = new Promise<Buffer>((resolve, reject) => {
      stream.once('data', (data: Buffer) => { resolve(data); });
      stream.once('error', reject);
    });
    stream.write(Buffer.from('native request'));
    assert.equal((await received).toString(), 'native request');
  } finally {
    stream.destroy();
  }
});

void test('invalid readiness is rejected instead of accepting another protocol', async () => {
  await assert.rejects(connectNativeTransport(settings, cancellation(), 'win32', launcher(`
    process.stdout.write('MCP READY\\n');
    process.stdin.resume();
  `)), /invalid readiness frame/);
});

void test('bridge startup failure preserves bounded stderr diagnostics', async () => {
  await assert.rejects(connectNativeTransport(settings, cancellation(), 'win32', launcher(`
    process.stderr.write('native-bridge unavailable');
    process.exit(7);
  `)), /native-bridge unavailable/);
});

void test('cancelling pending WSL readiness terminates the owned child', async () => {
  const token = cancellation();
  let child: ChildProcessWithoutNullStreams | undefined;
  const pending = connectNativeTransport(settings, token, 'win32', () => {
    child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'pipe' });
    return child;
  });
  assert.ok(child);
  const closed = once(child, 'close');
  token.cancel();
  await assert.rejects(pending, RequestCancelledError);
  await closed;
});

void test('already cancelled requests never start a WSL process', async () => {
  const token = cancellation();
  token.cancel();
  let started = false;
  await assert.rejects(connectNativeTransport(settings, token, 'win32', () => {
    started = true;
    return spawn(process.execPath, ['-e', ''], { stdio: 'pipe' });
  }), RequestCancelledError);
  assert.equal(started, false);
});

void test('WSL roots preserve Linux paths and delegate access checks to the daemon', () => {
  const roots = [
    { root: '/home/test/repo', scopes: ['.'], globs: ['*.rs'] },
    { root: '/home/test/other', scopes: ['.'], globs: ['*.rs'] },
  ];
  assert.equal(resolveWslRoot(roots, '/home/test/repo/src/..'), '/home/test/repo');
  assert.equal(resolveWslRoot(roots, '/home/test/worktree'), '/home/test/worktree');
  assert.throws(() => resolveWslRoot(roots, undefined), /explicit absolute Linux root/);
  assert.throws(() => resolveWslRoot(roots, 'C:\\Users\\test'), /absolute Linux path/);
  assert.throws(() => resolveWslRoot(roots, '\\\\wsl$\\Ubuntu\\home'), /absolute Linux path/);
  assert.equal(resolveWslRoot([roots[0]!], undefined), '/home/test/repo');
  assert.equal(resolveWslRoot(roots, undefined, ['/home/test/other/src/lib.rs']), '/home/test/other');
  assert.equal(
    resolveWslRoot(roots, undefined, ['\\\\wsl.localhost\\Ubuntu-22.04\\home\\test\\repo\\src\\main.rs']),
    '/home/test/repo',
  );
  assert.equal(
    wslCandidateToLinuxPath('\\\\wsl$\\Ubuntu\\home\\test\\repo\\src'),
    '/home/test/repo/src',
  );
});
