import * as vscode from 'vscode';

import { CacheMissError, DaemonClient } from './daemonClient.js';
import { NATIVE_TOOL_NAMES, NativeToolBridge } from './nativeTools.js';
import {
  registerCurrentWorkspace,
  prepareMissingWorkspace,
} from './registration.js';
import { RootResolutionError } from './rootResolver.js';
import { validateDaemonStatus } from './resultFormatter.js';
import type { ClientSettings, JsonValue } from './types.js';
import { UsageSummary } from './usage.js';

let client: DaemonClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel('Code Context Native', {
    log: true,
  });
  client = new DaemonClient(readSettings);
  const pending = new Map<string, Promise<void>>();
  const usage = new UsageSummary();
  let renderUsage = (): void => {};
  const preparationFailures = new Map<string, { at: number; message: string }>();
  const bridge = new NativeToolBridge(client, output, (path) => {
    const failure=preparationFailures.get(path);
    if (failure && performance.now()-failure.at<60_000) {
      throw new Error(`CACHE_LOAD_FAILED root=${path}: ${failure.message}. Use a permitted host read; retry cooldown is60s, do not poll or widen access.`);
    }
    preparationFailures.delete(path);
    if (!pending.has(path)) {
      if (pending.size >= 8) {
        throw new CacheMissError(`CACHE_MISS root=${path} state=registration_queue_full background_load=false: skip CCTX and use a permitted host read; do not poll or repeat this request.`);
      }
      const operation = prepareMissingWorkspace(path, bridge, output, context.globalState)
        .then(async () => {
          if (!client) {
            throw new Error('Code Context client was disposed before lazy registration completed.');
          }
          const source = new vscode.CancellationTokenSource();
          try {
            await client.request({
              root:path, name:'code_files', arguments:{glob:'*',limit:1},compact:true,
            }, source.token);
          } catch (error: unknown) {
            if (!(error instanceof CacheMissError)) {
              throw error;
            }
            output.appendLine(`CACHE_NOT_READY root=${path}: ${error.message}`);
          } finally {
            source.dispose();
          }
        })
        .catch((error: unknown) => {
          if (preparationFailures.size>=32) {
            const oldest=preparationFailures.keys().next().value;
            if (oldest!==undefined) { preparationFailures.delete(oldest); }
          }
          preparationFailures.set(path,{at:performance.now(),message:String(error)});
          output.appendLine(`CACHE_REGISTRATION_FAILED root=${path}: ${String(error)}`);
        })
        .finally(() => { pending.delete(path); });
      pending.set(path, operation);
    }
    throw new CacheMissError(`CACHE_MISS root=${path} state=registering background_load=true: source is not ready. Continue with a permitted host search/read under the same policy; do not poll, repeat this lookup, switch CCTX transports or run code_sync to wait for warmup.`);
  }, (event) => {
    usage.record(event);
    renderUsage();
  });
  bridge.register();

  const status = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    20,
  );
  status.name = 'Code Context Native';
  status.command = 'codeContextNative.showStatus';
  status.text = '$(database) CCTX Native';
  status.tooltip = 'Show Code Context native tool status';
  status.show();
  renderUsage = (): void => {
    if (usage.last) {
      status.text = usage.text();
      status.tooltip = usage.tooltip();
    }
  };

  let probing: Promise<void> | undefined;
  const probe = (): Promise<void> => {
    probing ??= runStartupProbe(bridge, status, output).finally(() => {
      probing = undefined;
      renderUsage();
    });
    return probing;
  };

  context.subscriptions.push(
    output,
    status,
    bridge,
    client,
    vscode.commands.registerCommand('codeContextNative.gettingStarted', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openWalkthrough', 'moonchoi.code-context-native#getting-started', false,
      );
    }),
    vscode.commands.registerCommand('codeContextNative.configureConnection', async () => {
      await vscode.commands.executeCommand('workbench.action.openSettings', '@ext:moonchoi.code-context-native');
    }),
    vscode.commands.registerCommand('codeContextNative.openSetupGuide', async () => {
      const opened = await vscode.env.openExternal(vscode.Uri.parse(
        'https://github.com/dotnetpower/code-context/blob/main/vscode-extension/docs/SETUP.md',
      ));
      if (!opened) {
        await vscode.window.showErrorMessage('Could not open the Code Context setup guide.');
      }
    }),
    vscode.commands.registerCommand('codeContextNative.showStatus', async () => {
      await showStatus(bridge, status, output, usage.lastRoot);
    }),
    vscode.commands.registerCommand('codeContextNative.selfTest', async (root?: string) => {
      await runSelfTest(bridge, output, root);
    }),
    vscode.commands.registerCommand(
      'codeContextNative.registerWorkspace',
      async () => {
        if (client?.usesWslBridge) {
          const message = 'Register manual workspaces with cctx register inside WSL. Authorized worktrees are prepared lazily by the daemon.';
          output.appendLine(`REGISTRATION_REQUIRED: ${message}`);
          await vscode.window.showErrorMessage(message);
          return;
        }
        if (await registerCurrentWorkspace(bridge, output)) {
          await probe();
        }
      },
    ),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('codeContextNative')) {
        client?.reset();
        bridge.invalidateRegistry();
        output.appendLine('Code Context native settings changed; applying on next request.');
        if (event.affectsConfiguration('codeContextNative.autoRegisterWorkspace')) {
          void probe();
        }
      }
    }),
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      void probe();
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      bridge.invalidateRegistry();
      if (vscode.workspace.isTrusted) {
        void probe();
      }
    }),
  );

  output.appendLine(
    `Activated native tools; endpoint=${client.endpoint}`,
  );
  if (vscode.workspace.isTrusted) {
    void probe();
  } else {
    status.text = '$(lock) CCTX Native';
    status.tooltip = 'Trust this workspace to enable native Code Context tools';
    output.appendLine('NATIVE_DISABLED workspace is not trusted');
  }
}

export function deactivate(): void {
  client?.dispose();
  client = undefined;
}

async function showStatus(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
  explicitRoot?: string,
): Promise<void> {
  try {
    const source = new vscode.CancellationTokenSource();
    try {
      const root = await bridge.resolveRoot(explicitRoot, source.token, false);
      const value = await bridge.invoke(
        {
          name: 'command_status',
          operation: 'code_status',
          needsRoot: true,
          invocationMessage: () => 'Reading status',
          arguments: () => ({}),
        },
        { root },
        source.token,
      );
      output.appendLine(JSON.stringify(value, null, 2));
      output.show(true);
    } finally {
      source.dispose();
    }
  } catch (error: unknown) {
    if (error instanceof RootResolutionError) {
      if (await registerCurrentWorkspace(bridge, output)) {
        await runStartupProbe(bridge, statusBar, output);
      }
      return;
    }
    await showFailure(error, output);
  }
}

async function runSelfTest(
  bridge: NativeToolBridge,
  output: vscode.OutputChannel,
  explicitRoot?: string,
): Promise<void> {
  try {
    await vscode.commands.executeCommand('setContext', 'codeContextNative.selfTestPassed', false);
    const started = performance.now();
    const source = new vscode.CancellationTokenSource();
    try {
      const workspaces = await bridge.listWorkspaces(source.token);
      let requestedRoot = explicitRoot;
      if (!requestedRoot && client?.usesWslBridge && workspaces.length > 1) {
        requestedRoot = await vscode.window.showQuickPick(
          workspaces.map((workspace) => workspace.root),
          { title: 'Code Context: Select a registered Linux root', placeHolder: 'The self-test observes status; it does not load source.' },
        );
        if (!requestedRoot) {
          output.appendLine('SELF_TEST_CANCELLED no registered root selected');
          return;
        }
      }
      const root = await bridge.resolveRoot(requestedRoot, source.token, false);
      const registered = registeredNativeTools();
      if (registered.length !== NATIVE_TOOL_NAMES.length) {
        throw new Error(
          `Expected ${NATIVE_TOOL_NAMES.length} native tools, found ${registered.length}.`,
        );
      }
      const toolResult = await vscode.lm.invokeTool(
        'code-context-native_codeStatus',
        {
          input: { root },
          toolInvocationToken: undefined,
        },
        source.token,
      );
      const statusText = toolResultText(toolResult);
      const daemonStatus = JSON.parse(statusText) as JsonValue;
      validateDaemonStatus(daemonStatus);
      await vscode.commands.executeCommand('setContext', 'codeContextNative.selfTestPassed', true);
      const metadata: unknown = vscode.extensions.getExtension('moonchoi.code-context-native')?.packageJSON;
      const version = typeof metadata === 'object' && metadata !== null && 'version' in metadata
        && typeof metadata.version === 'string' ? metadata.version : 'unknown';
      const report: JsonValue = {
        extensionCallable: true,
        extensionVersion: version,
        chatPickerEnablement: 'unverified',
        nextStep: 'Enable Code Context tools in the agent tool settings and test #cctxNativeFiles in this chat.',
        nativeTools: registered,
        registeredWorkspaces: workspaces.length,
        resolvedRoot: root,
        elapsedMs: Number((performance.now() - started).toFixed(3)),
        invocationResult: daemonStatus,
      };
      output.appendLine(JSON.stringify(report, null, 2));
      output.show(true);
      await vscode.window.showInformationMessage(
        `Code Context extension self-test passed: ${root}. Chat picker enablement is not verified.`,
      );
    } finally {
      source.dispose();
    }
  } catch (error: unknown) {
    await showFailure(error, output);
  }
}

async function runStartupProbe(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.OutputChannel,
): Promise<void> {
  const started = performance.now();
  const source = new vscode.CancellationTokenSource();
  try {
    const registered = registeredNativeTools();
    if (registered.length !== NATIVE_TOOL_NAMES.length) {
      throw new Error(
        `Language Model Tool registry contains ${registered.length}/${NATIVE_TOOL_NAMES.length} Code Context tools.`,
      );
    }
    const workspaceResult = await vscode.lm.invokeTool(
      'code-context-native_codeWorkspaces',
      {
        input: {},
        toolInvocationToken: undefined,
      },
      source.token,
    );
    const workspaces = JSON.parse(toolResultText(workspaceResult)) as JsonValue;
    if (!Array.isArray(workspaces)) {
      throw new Error('Native workspace probe returned a non-array result.');
    }
    const root = await bridge.resolveRoot(undefined, source.token, false);
    statusBar.text = '$(database) CCTX Available';
    statusBar.tooltip = `${registered.length} native tools registered and callable for ${root}; Chat picker enablement is user-controlled`;
    output.appendLine(
      `EXTENSION_CALLABLE tools=${registered.join(',')} workspaces=${workspaces.length} root=${root} probe=codeWorkspaces index_initialization=on-demand source_scan=none elapsed_ms=${(performance.now() - started).toFixed(3)} agent_picker=unverified; enable Code Context tools in the agent tool settings`,
    );
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    if (error instanceof RootResolutionError) {
      statusBar.text = '$(add) CCTX Register';
      statusBar.tooltip = 'Index initialization is deferred until the first source lookup; click for manual registration';
      output.appendLine(`NATIVE_UNREGISTERED_DEFERRED ${message}`);
    } else {
      statusBar.text = '$(warning) CCTX Native';
      statusBar.tooltip = message;
      output.appendLine(`NATIVE_NOT_READY ${message}`);
    }
  } finally {
    source.dispose();
  }
}

function toolResultText(result: vscode.LanguageModelToolResult): string {
  return result.content
    .filter(
      (part): part is vscode.LanguageModelTextPart =>
        part instanceof vscode.LanguageModelTextPart,
    )
    .map((part) => part.value)
    .filter((text) => !text.startsWith('CCTX_TRACE '))
    .join('');
}

function registeredNativeTools(): string[] {
  const expected = new Set<string>(NATIVE_TOOL_NAMES);
  return vscode.lm.tools
    .map((tool) => tool.name)
    .filter((name) => expected.has(name))
    .sort();
}

function readSettings(): ClientSettings {
  const config = vscode.workspace.getConfiguration('codeContextNative');
  return {
    socketPath: config.get<string>('socketPath', ''),
    autoStartService: config.get<boolean>('autoStartService', true),
    requestTimeoutMs: config.get<number>('requestTimeoutMs', 40_000),
    wslDistribution: config.get<string>('wslDistribution', ''),
    wslExecutablePath: config.get<string>('wslExecutablePath', ''),
  };
}

async function showFailure(
  error: unknown,
  output: vscode.OutputChannel,
): Promise<void> {
  const message = error instanceof Error ? error.message : String(error);
  output.appendLine(`ERROR: ${message}`);
  output.show(true);
  await vscode.window.showErrorMessage(`Code Context native tools: ${message}`);
}
