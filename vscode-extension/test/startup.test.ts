import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

import { RootResolutionError } from '../src/rootResolver.js';
import { CacheMissError } from '../src/daemonClient.js';
import { UsageSummary } from '../src/usage.js';

const toolNames = [
  'code-context-native_codeContext',
  'code-context-native_codeFiles',
  'code-context-native_codeGrep',
  'code-context-native_codeRead',
  'code-context-native_codeStatus',
  'code-context-native_codeSync',
  'code-context-native_codeWorkspaces',
];

async function activateFixture(trusted: boolean, registered: boolean,
  registration?: (path:string)=>Promise<void>, legacy = false): Promise<{
  calls: string[];
  messages: string[];
  events: Array<() => void>;
  prepareMissing(path:string):Promise<void>;
  invokeCommand(name: string): Promise<void>;
  executed: Array<{ name: string; args: unknown[] }>;
}> {
  const calls: string[] = [];
  const messages: string[] = [];
  const events: Array<() => void> = [];
  const commands = new Map<string, () => unknown>();
  const executed: Array<{ name: string; args: unknown[] }> = [];
  let prepare:((path:string)=>Promise<void>)|undefined;
  const disposable = { dispose(): void {} };
  const event = (listener: () => void): typeof disposable => {
    events.push(listener);
    return disposable;
  };
  class TextPart {
    public constructor(public readonly value: string) {}
  }
  class Bridge {
    public constructor(_client:unknown,_output:unknown,callback?:(path:string)=>Promise<void>) {
      prepare=callback;
    }
    public register(): void { calls.push('register-tools'); }
    public invalidateRegistry(): void {}
    public resolveRoot(_explicit: unknown, _token: unknown, allowPreparation: boolean): Promise<string> {
      assert.equal(allowPreparation, false, 'startup must never prepare or scan source');
      calls.push('resolve-existing-root');
      if (!registered) { return Promise.reject(new RootResolutionError('not registered')); }
      return Promise.resolve('/large-workspace');
    }
  }
  const vscode = {
    workspace: {
      isTrusted: trusted,
      workspaceFolders: Array.from({ length: 20 }, (_, index) => ({
        uri: { fsPath: `/large-workspace-${index}` }, index,
      })),
      getConfiguration: (): { get<T>(key: string, fallback: T): T } => ({
        get<T>(_key: string, fallback: T): T { return fallback; },
      }),
      onDidChangeConfiguration: event,
      onDidGrantWorkspaceTrust: event,
      onDidChangeWorkspaceFolders: event,
    },
    window: {
      createOutputChannel: (): object => ({ ...disposable, appendLine: (line: string): void => { messages.push(line); } }),
      createStatusBarItem: (): object => ({ ...disposable, show(): void {} }),
      showWarningMessage: (message: string): Promise<void> => {
        messages.push(message);
        return Promise.resolve();
      },
    },
    extensions: { getExtension: (id: string): { id: string } | undefined =>
      legacy && id === 'moonchoi.code-context-native' ? { id } : undefined },
    StatusBarAlignment: { Left: 1 },
    commands: {
      registerCommand: (name: string, callback: () => unknown): object => {
        commands.set(name, callback);
        return disposable;
      },
      executeCommand: (name: string, ...args: unknown[]): Promise<void> => {
        executed.push({ name, args });
        return Promise.resolve();
      },
    },
    CancellationTokenSource: class {
      public token = {};
      public dispose(): void {}
    },
    LanguageModelTextPart: TextPart,
    lm: {
      tools: toolNames.map((name) => ({ name })),
      invokeTool: async (name: string): Promise<object> => {
        calls.push(name);
        assert.equal(name, 'code-context-native_codeWorkspaces', 'startup cannot request source or index status');
        await new Promise<void>((done) => setImmediate(done));
        return { content: [
          new TextPart(JSON.stringify(registered ? [{ root: '/large-workspace' }] : [])),
          new TextPart('CCTX_TRACE {"trace_id":"startup-probe"}'),
        ] };
      },
    },
  };
  const exports: { activate?: (context: object) => void } = {};
  const source = await readFile(resolve(__dirname, '../src/extension.js'), 'utf8');
  const dependencies: Record<string, unknown> = {
    vscode,
    './daemonClient.js': {
      DaemonClient: class {
        public dispose(): void {}
        public request():Promise<never> {
          calls.push('lazy-root-request');
          return Promise.reject(new CacheMissError('CACHE_MISS root=/lazy state=queued'));
        }
      },
      CacheMissError,
      normalizeSocketPath: (): string => '/test/daemon.sock',
    },
    './nativeTools.js': { NativeToolBridge: Bridge, NATIVE_TOOL_NAMES: toolNames },
    './registration.js': {
      autoRegisterWorkspaceFolders: (): never => { throw new Error('startup source enumeration is forbidden'); },
      prepareMissingWorkspace: (path:string):Promise<void> => {
        if (!registration) {throw new Error('startup workspace preparation is forbidden');}
        return registration(path);
      },
    },
    './rootResolver.js': { RootResolutionError },
    './resultFormatter.js': {},
    './usage.js': { UsageSummary },
  };
  runInNewContext(source, {
    exports,
    performance,
    require: (name: string): unknown => {
      if (!(name in dependencies)) { throw new Error(`Unexpected startup dependency: ${name}`); }
      return dependencies[name];
    },
  }, { filename: 'extension-startup-under-test.js' });
  assert.ok(exports.activate);
  exports.activate({
    subscriptions: [], globalState: {},
    extension: { id: 'dotnetpower.code-context-native', packageJSON: { version: '0.13.0' } },
  });
  return { calls, messages, events, executed,
    invokeCommand: async (name: string): Promise<void> => {
      const callback = commands.get(name);
      assert.ok(callback, name);
      await callback();
    },
    prepareMissing:(path:string):Promise<void>=>{
    assert.ok(prepare);
    const invoke=prepare;
    return Promise.resolve().then(()=>invoke(path));
  } };
}

void test('publisher migration blocks duplicate native registration until the old extension is removed', async () => {
  const fixture = await activateFixture(true, true, undefined, true);
  assert.deepEqual(fixture.calls, []);
  assert.ok(fixture.messages.some((message) => message.startsWith('MIGRATION_REQUIRED:')));
  assert.ok(fixture.messages.some((message) => message.includes('safe boundary')));
});

void test('publisher-specific UI commands use the running extension identity', async () => {
  const fixture = await activateFixture(false, false);
  await fixture.invokeCommand('codeContextNative.gettingStarted');
  await fixture.invokeCommand('codeContextNative.configureConnection');
  assert.deepEqual(fixture.executed, [
    { name: 'workbench.action.openWalkthrough', args: ['dotnetpower.code-context-native#getting-started', false] },
    { name: 'workbench.action.openSettings', args: ['@ext:dotnetpower.code-context-native'] },
  ]);
});

void test('reloads never scan source or build indexes even with twenty workspace folders', async () => {
  for (const registered of [true, false]) {
    for (let reload = 0; reload < 3; reload += 1) {
      const fixture = await activateFixture(true, registered);
      await new Promise<void>((done) => setImmediate(done));
      await new Promise<void>((done) => setImmediate(done));
      assert.deepEqual(fixture.calls, [
        'register-tools', 'code-context-native_codeWorkspaces', 'resolve-existing-root',
      ]);
      assert.ok(fixture.messages.some((message) => message.includes(
        registered ? 'source_scan=none' : 'NATIVE_UNREGISTERED_DEFERRED',
      )));
    }
  }
});

void test('trust and workspace event bursts share one in-flight readiness probe', async () => {
  const fixture = await activateFixture(true, true);
  for (let index = 0; index < 10; index += 1) {
    fixture.events[1]?.();
    fixture.events[2]?.();
  }
  await new Promise<void>((done) => setImmediate(done));
  await new Promise<void>((done) => setImmediate(done));
  assert.equal(fixture.calls.filter((name) => name === 'code-context-native_codeWorkspaces').length, 1);
});

void test('untrusted workspace only registers tools and never probes the daemon', async () => {
  const fixture = await activateFixture(false, false);
  await new Promise<void>((done) => setImmediate(done));
  assert.deepEqual(fixture.calls, ['register-tools']);
});

void test('first-use registration returns a cache miss without waiting and coalesces the root', async()=>{
  let complete!:()=>void;
  let registrations=0;
  const pending=new Promise<void>((done)=>{complete=done;});
  const fixture=await activateFixture(true,false,()=>{
    registrations+=1;
    return pending;
  });
  let timer:NodeJS.Timeout|undefined;
  try {
    await assert.rejects(Promise.race([
      fixture.prepareMissing('/lazy'),
      new Promise<never>((_resolve,reject)=>{
        timer=setTimeout(()=>reject(new Error('first-use registration blocked the tool')),500);
      }),
    ]),CacheMissError);
    await assert.rejects(fixture.prepareMissing('/lazy'),CacheMissError);
    assert.equal(registrations,1);
    assert.ok(!fixture.calls.includes('lazy-root-request'));
    complete();
    await new Promise<void>((done)=>setImmediate(done));
    await new Promise<void>((done)=>setImmediate(done));
    assert.equal(fixture.calls.filter((call)=>call==='lazy-root-request').length,1);
    assert.ok(fixture.messages.some((message)=>message.includes('CACHE_NOT_READY root=/lazy')));
  } finally {
    clearTimeout(timer);
    complete();
  }
});

void test('background registration failures are visible and suppress repeated preparation', async()=>{
  let registrations=0;
  const fixture=await activateFixture(true,false,()=>{
    registrations+=1;
    return Promise.reject(new Error('registration policy denied'));
  });
  await assert.rejects(fixture.prepareMissing('/denied'),CacheMissError);
  await new Promise<void>((done)=>setImmediate(done));
  assert.ok(fixture.messages.some((line)=>line.includes('CACHE_REGISTRATION_FAILED root=/denied')));
  await assert.rejects(fixture.prepareMissing('/denied'),/CACHE_LOAD_FAILED.*registration policy denied/);
  assert.equal(registrations,1);
  assert.ok(!fixture.calls.includes('lazy-root-request'));
});
