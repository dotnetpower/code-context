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
