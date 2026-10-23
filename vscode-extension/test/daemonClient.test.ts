import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { createServer, type Server, type Socket } from 'node:net';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

import {
  DaemonClient,
  DaemonResponseError,
  CacheMissError,
  RequestCancelledError,
  TracedRequestError,
} from '../src/daemonClient.js';
import type {
  CancellationLike,
  ClientSettings,
  DaemonRequest,
  DisposableLike,
} from '../src/types.js';
import type { NativeToolBridge } from '../src/nativeTools.js';

class TestCancellation implements CancellationLike {
  private readonly listeners = new Set<() => void>();
  public isCancellationRequested = false;

  public onCancellationRequested(listener: () => void): DisposableLike {
    this.listeners.add(listener);
    return { dispose: () => this.listeners.delete(listener) };
  }

  public cancel(): void {
    this.isCancellationRequested = true;
    for (const listener of [...this.listeners]) {
      listener();
    }
  }
}

const uncancelled = (): TestCancellation => new TestCancellation();

function settings(socketPath: string, timeout = 2_000): ClientSettings {
  return {
    socketPath,
    autoStartService: false,
    requestTimeoutMs: timeout,
  };
}

function request(name = 'ping'): DaemonRequest {
  return { root: null, name, arguments: {}, compact: true };
}

async function fixture(
  handler: (requestValue: unknown, socket: Socket) => void,
): Promise<{
  path: string;
  server: Server;
  connections: () => number;
  cleanup(): Promise<void>;
}> {
  const directory = await mkdtemp(join(tmpdir(), 'cctx-native-'));
  const path = join(directory, 'daemon.sock');
  let count = 0;
  const server = createServer((socket) => {
    count += 1;
    attachFramedHandler(socket, handler);
  });
  await new Promise<void>((resolveListen, rejectListen) => {
    server.once('error', rejectListen);
    server.listen(path, resolveListen);
  });
  return {
    path,
    server,
    connections: () => count,
    cleanup: async (): Promise<void> => {
      await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
      await rm(directory, { recursive: true, force: true });
    },
  };
}

void test('persistent connection serializes multiple requests', async () => {
  const seen: unknown[] = [];
  const testServer = await fixture((value, socket) => {
    seen.push(value);
    writeResponse(socket, { result: { sequence: seen.length } });
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    const first = await client.request<{ sequence: number }>(
      request('one'),
      uncancelled(),
    );
    const second = await client.request<{ sequence: number }>(
      request('two'),
      uncancelled(),
    );
    assert.deepEqual(first, { sequence: 1 });
    assert.deepEqual(second, { sequence: 2 });
    assert.equal(testServer.connections(), 1);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('traced requests negotiate once and retain source shape with matching daemon IDs', async () => {
  let pings = 0;
  const testServer = await fixture((value, socket) => {
    const input = value as DaemonRequest;
    if (input.name === 'ping') {
      pings += 1;
      writeResponse(socket, { result: { capabilities: { request_tracing: true, trace_clock: 'unix_ms' } } });
    } else {
      writeResponse(socket, { result: {
        value: [{ path: 'a.rs', lines: ['saved'] }],
        _cctx_trace: { trace_id: input.arguments._cctx_trace, daemon_request_id: 31, queue_ms: 0, execute_ms: 1 },
      } });
    }
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    for (const id of ['native-1', 'native-2']) {
      const result = await client.requestTraced(request('code_read'), uncancelled(), id);
      assert.deepEqual(result.value, [{ path: 'a.rs', lines: ['saved'] }]);
      assert.equal(result.trace.available, true);
      assert.equal(result.trace.trace_id, id);
      assert.deepEqual(result.trace.daemon, { trace_id: id, daemon_request_id: 31, queue_ms: 0, execute_ms: 1 });
      assert.ok(result.trace.total_ms >= result.trace.ipc_ms);
      assert.ok(result.trace.completed_unix_ms >= result.trace.started_unix_ms);
    }
    assert.equal(pings, 1);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('actual native tool returns before its queued telemetry reply and exposes the same trace ID', async () => {
  let telemetrySocket: Socket | undefined;
  let sourceTrace: string | undefined;
  let onTelemetry!: () => void;
  const telemetryArrived = new Promise<void>((done) => { onTelemetry = done; });
  const testServer = await fixture((value, socket) => {
    const input = value as DaemonRequest;
    if (input.name === 'code_workspaces') {
      writeResponse(socket, { result: [{ root: '/fixture', scopes: ['.'], globs: ['*.rs'] }] });
    } else if (input.name === 'ping') {
      writeResponse(socket, { result: { capabilities: { request_tracing: true, trace_clock: 'unix_ms' } } });
    } else if (input.name === 'code_trace') {
      telemetrySocket = socket;
      onTelemetry();
    } else {
      assert.equal(input.name, 'code_read');
      const id = input.arguments._cctx_trace;
      assert.ok(typeof id === 'string');
      sourceTrace = id;
      writeResponse(socket, { result: {
        value: [{ path: 'a.rs', start: 1, end: 1, lines: ['saved'], sha256: 'a'.repeat(64) }],
        _cctx_trace: { trace_id: sourceTrace, daemon_request_id: 9, queue_ms: 0, execute_ms: 1 },
      } });
    }
  });
  const client = new DaemonClient(() => settings(testServer.path));
  interface Tool {
    invoke(options: { input: object }, token: CancellationLike): Promise<{ content: Array<{ value: string }> }>;
  }
  const tools = new Map<string, Tool>();
  const logs: string[] = [];
  const compiledPath = resolve(__dirname, '../src/nativeTools.js');
  const nativeRequire = createRequire(compiledPath);
  const exports: { NativeToolBridge?: typeof NativeToolBridge } = {};
  const workspace = { isTrusted: true, workspaceFolders: [] };
  runInNewContext(await readFile(compiledPath, 'utf8'), {
    exports, performance,
    require: (name: string): unknown => name === 'vscode' ? {
      workspace,
      window: {},
      lm: { registerTool: (name: string, tool: Tool): DisposableLike => {
        tools.set(name, tool);
        return { dispose: (): void => {} };
      } },
      LanguageModelTextPart: class { public constructor(public readonly value: string) {} },
      LanguageModelToolResult: class { public constructor(public readonly content: unknown[]) {} },
    } : nativeRequire(name),
  });
  assert.ok(exports.NativeToolBridge);
  const bridge = new exports.NativeToolBridge(client, {
    name: 'trace-test',
    append: (): void => {},
    appendLine: (line: string): void => { logs.push(line); },
    replace: (): void => {},
    clear: (): void => {},
    show: (): void => {},
    hide: (): void => {},
    dispose: (): void => {},
  });
  bridge.register();
  let timer: NodeJS.Timeout | undefined;
  try {
    const tool = tools.get('code-context-native_codeRead');
    assert.ok(tool);
    const metadataTool = tools.get('code-context-native_codeWorkspaces');
    assert.ok(metadataTool);
    workspace.isTrusted = false;
    await assert.rejects(metadataTool.invoke({ input: {} }, uncancelled()), /workspace is trusted/);
    await assert.rejects(tool.invoke({ input: { root: '/fixture' } }, uncancelled()), /workspace is trusted/);
    workspace.isTrusted = true;
    const deadline = new Promise<never>((_resolve, reject) => {
      timer = setTimeout(() => reject(new Error('native source result waited for telemetry')), 900);
    });
    const result = await Promise.race([
      tool.invoke({ input: { root: '/fixture', ranges: [{ path: 'a.rs', start: 1, end: 1 }] } }, uncancelled()),
      deadline,
    ]);
    assert.match(result.content[0]?.value ?? '', /saved/);
    const footer = result.content[1]?.value ?? '';
    assert.ok(sourceTrace);
    assert.ok(footer.includes(sourceTrace));
    assert.ok(logs.some((line) => line.includes(`trace=${sourceTrace}`)));
    await Promise.race([telemetryArrived, deadline]);
    assert.ok(telemetrySocket, 'telemetry must actually be queued');
    writeResponse(telemetrySocket, { result: { recorded: true } });
  } finally {
    clearTimeout(timer);
    bridge.dispose();
    client.dispose();
    await testServer.cleanup();
  }
});

void test('old daemons receive unchanged arguments and explicitly unavailable tracing', async () => {
  const seen: DaemonRequest[] = [];
  const testServer = await fixture((value, socket) => {
    const input = value as DaemonRequest;
    seen.push(input);
    writeResponse(socket, { result: input.name === 'ping'
      ? { capabilities: { request_tracing: true } } : { ready: true } });
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    const result = await client.requestTraced(request('code_status'), uncancelled(), 'old-server');
    assert.deepEqual(result.value, { ready: true });
    assert.equal(result.trace.available, false);
    assert.deepEqual(seen[1]?.arguments, {});
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('mismatched trace responses fail explicitly without retrying the source request', async () => {
  let sourceCalls = 0;
  const testServer = await fixture((value, socket) => {
    const input = value as DaemonRequest;
    if (input.name === 'ping') {
      writeResponse(socket, { result: { capabilities: { request_tracing: true, trace_clock: 'unix_ms' } } });
    } else {
      sourceCalls += 1;
      writeResponse(socket, { result: { value: [], _cctx_trace: { trace_id: 'different' } } });
    }
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    await assert.rejects(client.requestTraced(request('code_read'), uncancelled(), 'expected'), (error: unknown) => {
      assert.ok(error instanceof TracedRequestError);
      assert.ok(error.cause instanceof DaemonResponseError);
      assert.equal(error.trace.trace_id, 'expected');
      assert.equal(error.trace.success, false);
      return true;
    });
    assert.equal(sourceCalls, 1);
    assert.equal(testServer.connections(), 1);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('transport close reconnects once', async () => {
  let calls = 0;
  const testServer = await fixture((_value, socket) => {
    calls += 1;
    if (calls === 1) {
      socket.destroy();
    } else {
      writeResponse(socket, { result: { recovered: true } });
    }
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    const value = await client.request<{ recovered: boolean }>(
      request(),
      uncancelled(),
    );
    assert.deepEqual(value, { recovered: true });
    assert.equal(testServer.connections(), 2);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('daemon application errors are not retried', async () => {
  const testServer = await fixture((_value, socket) => {
    writeResponse(socket, { error: 'invalid source request' });
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    await assert.rejects(
      client.request(request(), uncancelled()),
      DaemonResponseError,
    );
    assert.equal(testServer.connections(), 1);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('cold cache misses stay typed, immediate and never reconnect or start the service', async () => {
  let calls=0;
  let starts=0;
  const testServer=await fixture((_value,socket)=>{
    calls+=1;
    writeResponse(socket,{error:'CACHE_MISS root=/fixture state=loading background_load=true: use a permitted host read; do not retry.'});
  });
  const client=new DaemonClient(()=>({...settings(testServer.path),autoStartService:true}),
    ()=>{starts+=1;return Promise.resolve();});
  try {
    const started=performance.now();
    await assert.rejects(client.request(request('code_read'),uncancelled()),CacheMissError);
    assert.ok(performance.now()-started<500);
    assert.equal(calls,1);
    assert.equal(starts,0);
    assert.equal(testServer.connections(),1);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('cancellation closes an in-flight request', async () => {
  const testServer = await fixture(() => undefined);
  const client = new DaemonClient(() => settings(testServer.path));
  const cancellation = new TestCancellation();
  try {
    const pending = client.request(request(), cancellation);
    setTimeout(() => cancellation.cancel(), 20);
    await assert.rejects(pending, RequestCancelledError);
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('request timeout releases the connection', async () => {
  const testServer = await fixture(() => undefined);
  const client = new DaemonClient(() => settings(testServer.path, 30));
  try {
    await assert.rejects(
      client.request(request(), uncancelled()),
      /timed out after 30 ms/,
    );
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('oversized responses are rejected before unbounded buffering', async () => {
  const testServer = await fixture((_value, socket) => {
    socket.write(Buffer.alloc(2 * 1024 * 1024 + 1, 0x78));
  });
  const client = new DaemonClient(() => settings(testServer.path));
  try {
    await assert.rejects(
      client.request(request(), uncancelled()),
      /response exceeds 2 MiB/,
    );
  } finally {
    client.dispose();
    await testServer.cleanup();
  }
});

void test('autostart hook is invoked once before reconnect', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'cctx-native-start-'));
  const path = join(directory, 'daemon.sock');
  let starts = 0;
  let server: Server | undefined;
  const client = new DaemonClient(
    () => ({ ...settings(path), autoStartService: true }),
    async () => {
      starts += 1;
      server = createServer((socket) => {
        attachFramedHandler(socket, () => {
          writeResponse(socket, { result: { started: true } });
        });
      });
      await new Promise<void>((resolveListen) => server?.listen(path, resolveListen));
    },
  );
  try {
    const value = await client.request<{ started: boolean }>(
      request(),
      uncancelled(),
    );
    assert.deepEqual(value, { started: true });
    assert.equal(starts, 1);
  } finally {
    client.dispose();
    if (server) {
      await new Promise<void>((resolveClose) => server?.close(() => resolveClose()));
    }
    await rm(directory, { recursive: true, force: true });
  }
});

void test('stale close events cannot tear down a replacement socket', async () => {
  const first = await fixture((_value, socket) => {
    writeResponse(socket, { result: { server: 'first' } });
  });
  const second = await fixture((_value, socket) => {
    writeResponse(socket, { result: { server: 'second' } });
  });
  let currentPath = first.path;
  const client = new DaemonClient(() => settings(currentPath));
  try {
    for (let index = 0; index < 20; index += 1) {
      currentPath = index % 2 === 0 ? first.path : second.path;
      const value = await client.request<{ server: string }>(
        request(),
        uncancelled(),
      );
      assert.equal(value.server, index % 2 === 0 ? 'first' : 'second');
    }
  } finally {
    client.dispose();
    await first.cleanup();
    await second.cleanup();
  }
});

function attachFramedHandler(
  socket: Socket,
  handler: (requestValue: unknown, socket: Socket) => void,
): void {
  let buffer = Buffer.alloc(0);
  let handshake = false;
  socket.on('error', () => undefined);
  socket.on('data', (chunk: Buffer) => {
    buffer = Buffer.concat([buffer, chunk]);
    if (!handshake) {
      const newline = buffer.indexOf(0x0a);
      if (newline < 0) {
        return;
      }
      assert.equal(buffer.subarray(0, newline + 1).toString('ascii'), 'CCTXJ1\n');
      buffer = buffer.subarray(newline + 1);
      handshake = true;
    }
    while (buffer.byteLength >= 4) {
      const length = buffer.readUInt32BE(0);
      if (buffer.byteLength < length + 4) {
        return;
      }
      const frame = buffer.subarray(4, length + 4);
      buffer = buffer.subarray(length + 4);
      handler(JSON.parse(frame.toString('utf8')) as unknown, socket);
    }
  });
}

function writeResponse(socket: Socket, value: unknown): void {
  const payload = Buffer.from(JSON.stringify(value), 'utf8');
  const frame = Buffer.allocUnsafe(payload.byteLength + 4);
  frame.writeUInt32BE(payload.byteLength, 0);
  payload.copy(frame, 4);
  socket.write(frame);
}
