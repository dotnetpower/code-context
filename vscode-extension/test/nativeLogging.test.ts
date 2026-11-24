import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import test from 'node:test';
import { runInNewContext } from 'node:vm';
import { TracedRequestError, type ClientTrace } from '../src/daemonClient.js';
import type { JsonValue } from '../src/types.js';

interface Tool {
  invoke(options: { input: object }, token: object): Promise<{ content: Array<{ value: string }> }>;
}

async function fixture(failure?: string): Promise<{
  tools: Map<string, Tool>;
  requests: string[];
  logs: Array<{ level: string; text: string }>;
  bridge: { resolveRoot(explicit: string | undefined, token: object): Promise<string> };
}> {
  const tools = new Map<string, Tool>();
  const requests: string[] = [];
  const logs: Array<{ level: string; text: string }> = [];
  const client = {
    usesWslBridge: true,
    request: (request: { name: string }): Promise<JsonValue> => {
      requests.push(request.name);
      return Promise.resolve(request.name === 'code_workspaces' ?
        [{ root: '/first', scopes: ['.'], globs: ['*.rs'] },
          { root: '/second', scopes: ['.'], globs: ['*.rs'] }] : {});
    },
    requestTraced: (request: { name: string; root: string }, _token: object,
      id: string): Promise<{ value: JsonValue; trace: ClientTrace }> => {
      requests.push(request.name);
      const trace: ClientTrace = { trace_id: id, operation: request.name, root: request.root, success: failure === undefined, available: true,
          outcome: 'success', total_ms: 1, prepare_ms: 0, queue_ms: 0, ipc_ms: 1,
          serialize_ms: 0, write_ms: 0, started_unix_ms: 0, completed_unix_ms: 1 };
      if (failure !== undefined) { return Promise.reject(new TracedRequestError(trace, new Error(failure))); }
      return Promise.resolve({ value: { paths: ['a.rs'], total_matches: 1, truncated: false }, trace });
    },
  };
  const file = resolve(__dirname, '../src/nativeTools.js');
  const nativeRequire = createRequire(file);
  const exports: { NativeToolBridge?: new (client: object, output: object) => {
    register(): void; resolveRoot(explicit: string | undefined, token: object): Promise<string>;
  } } = {};
  runInNewContext(await readFile(file, 'utf8'), {
    exports, performance,
    require: (name: string): unknown => name === 'vscode' ? {
      workspace: { isTrusted: true, workspaceFolders: [] },
      window: {},
      lm: { registerTool: (name: string, tool: Tool): object => {
        tools.set(name, tool); return { dispose(): void {} };
      } },
      LanguageModelTextPart: class { public constructor(public readonly value: string) {} },
      LanguageModelToolResult: class { public constructor(public readonly content: unknown[]) {} },
    } : nativeRequire(name),
  });
  assert.ok(exports.NativeToolBridge);
  const output = Object.fromEntries(['info', 'warn', 'error'].map((level) =>
    [level, (text: string): void => { logs.push({ level, text }); }]));
  const bridge = new exports.NativeToolBridge(client, output);
  bridge.register();
  return { tools, requests, logs, bridge };
}

void test('explicit Windows Linux roots avoid registry calls but retain normalization and validation', async () => {
  const f = await fixture();
  assert.equal(await f.bridge.resolveRoot('/unprepared/src/..', {}), '/unprepared');
  assert.deepEqual(f.requests, []);
  await assert.rejects(f.bridge.resolveRoot('C:\\repo', {}), /absolute Linux path/);
  assert.deepEqual(f.requests, []);
});

void test('pre-request root failures produce one warning and do not fabricate daemon telemetry', async () => {
  const f = await fixture();
  const tool = f.tools.get('code-context-native_codeFiles');
  assert.ok(tool);
  await assert.rejects(tool.invoke({ input: { glob: '*.rs' } }, {}), /explicit absolute Linux root/);
  assert.deepEqual(f.requests, ['code_workspaces']);
  assert.equal(f.logs.length, 1);
  assert.equal(f.logs[0]?.level, 'warn');
  assert.match(f.logs[0]?.text ?? '', /code_files invalid_request.*trace=.*reason=/);
});

void test('successful calls log one plain line while preserving JSON trace footer and daemon reporting', async () => {
  const f = await fixture();
  const tool = f.tools.get('code-context-native_codeFiles');
  assert.ok(tool);
  const result = await tool.invoke({ input: { root: '/fixture', glob: '*.rs' } }, {});
  assert.deepEqual(f.requests, ['code_files', 'code_trace']);
  assert.equal(f.logs.length, 1);
  assert.equal(f.logs[0]?.level, 'info');
  assert.match(f.logs[0]?.text ?? '', /^code_files success /);
  assert.ok(!f.logs[0]?.text.includes('{'));
  assert.match(result.content[0]?.value ?? '', /a\.rs/);
  const trace = JSON.parse((result.content[1]?.value ?? '').replace(/^CCTX_TRACE /, '')) as ClientTrace;
  assert.equal(trace.operation, 'code_files');
  assert.equal(trace.outcome, 'success');
  assert.ok(Math.abs(Date.now() - trace.started_unix_ms) < 60_000);
  assert.equal(typeof trace.total_ms, 'number');
  assert.equal(result.content.length, 2);
  await tool.invoke({ input: { root: '/first', glob: '*.rs' } }, {});
  const followUp = await tool.invoke({ input: { pattern: '**/*.rs' } }, {});
  assert.match(followUp.content[0]?.value ?? '', /a\.rs/);
});

void test('native failures retain their reason and severity in one line with compatible telemetry', async () => {
  for (const [reason, outcome, level] of [
    ['CACHE_MISS root=/fixture', 'cache_not_ready', 'info'],
    ['READ_POLICY_DENIED reason=unsupported_glob', 'policy_denied', 'warn'],
    ['regex parse error:\ninvalid expression', 'invalid_request', 'warn'],
    ['socket disconnected', 'system_error', 'error'],
  ] as const) {
    const f = await fixture(reason);
    const tool = f.tools.get('code-context-native_codeFiles');
    assert.ok(tool);
    await assert.rejects(tool.invoke({ input: { root: '/fixture', glob: '*.rs' } }, {}));
    assert.deepEqual(f.requests, ['code_files', 'code_trace']);
    assert.equal(f.logs.length, 1);
    assert.equal(f.logs[0]?.level, level);
    assert.ok(f.logs[0]?.text.startsWith(`code_files ${outcome}`));
    assert.ok(f.logs[0]?.text.includes('reason='));
    assert.ok(f.logs[0]?.text.includes(reason.replaceAll('\n', '\\n')), f.logs[0]?.text);
    assert.ok(!f.logs[0]?.text.includes('\n'));
  }
});

void test('Windows path.win32.resolve never corrupts WSL listWorkspaces Linux roots into C:\\home\\...', async () => {
  const nodePath = await import('node:path');
  const rootResolverFile = resolve(__dirname, '../src/rootResolver.js');
  const nativeToolsFile = resolve(__dirname, '../src/nativeTools.js');
  const nativeRequire = createRequire(nativeToolsFile);

  const winPathMock = {
    ...nodePath,
    resolve: (...args: string[]): string => nodePath.win32.resolve(...args),
    isAbsolute: (p: string): boolean => nodePath.win32.isAbsolute(p),
    basename: (p: string, suffix?: string): string => nodePath.win32.basename(p, suffix),
    join: (...args: string[]): string => nodePath.win32.join(...args),
    relative: (from: string, to: string): string => nodePath.win32.relative(from, to),
    sep: nodePath.win32.sep,
  };

  const rootResolverExports: Record<string, unknown> = {};
  runInNewContext(await readFile(rootResolverFile, 'utf8'), {
    exports: rootResolverExports,
    require: (name: string): unknown => (name === 'node:path' ? winPathMock : nativeRequire(name)),
  });

  const nativeExports: {
    NativeToolBridge?: new (client: object, output: object) => {
      listWorkspaces(token: object): Promise<Array<{ root: string }>>;
      resolveRoot(explicit: string | undefined, token: object, allowPreparation?: boolean): Promise<string>;
      prioritizeOpenWslRoots(workspaces: Array<{ root: string; scopes: string[]; globs: string[] }>): string[];
    };
  } = {};
  runInNewContext(await readFile(nativeToolsFile, 'utf8'), {
    exports: nativeExports,
    performance,
    require: (name: string): unknown => {
      if (name === 'node:path') { return winPathMock; }
      if (name === './rootResolver.js') { return rootResolverExports; }
      if (name === 'vscode') {
        return {
          workspace: {
            isTrusted: true,
            workspaceFolders: [{ uri: { scheme: 'vscode-remote', authority: 'wsl+Ubuntu-22.04', path: '/home/user/dev/code-context', fsPath: '\\\\wsl.localhost\\Ubuntu-22.04\\home\\user\\dev\\code-context' } }],
          },
          window: {},
          lm: { registerTool: (): object => ({ dispose(): void {} }) },
        };
      }
      return nativeRequire(name);
    },
  });

  assert.ok(nativeExports.NativeToolBridge);
  const client = {
    usesWslBridge: true,
    wslDistribution: 'Ubuntu-22.04',
    request: (): Promise<JsonValue> =>
      Promise.resolve([
        { root: '/home/user/dev/agent-san', scopes: ['src'], globs: ['*.rs'] },
        { root: '/home/user/dev/code-context', scopes: ['src'], globs: ['*.rs'], managed_parent: '/home/user/dev/parent' },
      ]),
  };
  const output = { info(): void {}, warn(): void {}, error(): void {} };
  const bridge = new nativeExports.NativeToolBridge(client, output);
  const listed = await bridge.listWorkspaces({});
  assert.deepEqual(listed.map((w) => w.root), [
    '/home/user/dev/agent-san',
    '/home/user/dev/code-context',
  ]);
  // Selecting any QuickPick item returned by listWorkspaces on Windows resolves cleanly without throwing!
  const resolved = await bridge.resolveRoot(listed[1]!.root, {}, false);
  assert.equal(resolved, '/home/user/dev/code-context');
  // Mismatched WSL distribution URIs are rejected instead of stripping authority and bypassing distro checks!
  await assert.rejects(
    bridge.resolveRoot('vscode-remote://wsl+Debian/home/user/dev/code-context', {}, false),
    /does not match configured WSL distribution/,
  );
  // Open WSL workspace is prioritized first in QuickPick ordering.
  const ordered = bridge.prioritizeOpenWslRoots(listed as Array<{ root: string; scopes: string[]; globs: string[] }>);
  assert.equal(ordered[0], '/home/user/dev/code-context');
});
