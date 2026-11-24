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
import { formatLogDetails, singleLine } from './logFormatter.js';

let client: DaemonClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const output = vscode.window.createOutputChannel('Code Context Native', {
    log: true,
  });
  const legacy = vscode.extensions.getExtension('moonchoi.code-context-native');
  if (legacy) {
    const message = `Remove ${legacy.id} before enabling ${context.extension.id}, then reload the affected window at a safe boundary to avoid duplicate native tools.`;
    output.warn(`MIGRATION_REQUIRED: ${message}`);
    context.subscriptions.push(output);
    void vscode.window.showWarningMessage(message);
    return;
  }
  client = new DaemonClient(readSettings);
  const activeClient = client;
  const pending = new Map<string, Promise<void>>();
  const usage = new UsageSummary();
  const hasBackendUsageQuery = typeof (activeClient as { queryUsage?: unknown }).queryUsage === 'function';
  if (hasBackendUsageQuery) {
    usage.setBackendQuery((hours, force) => activeClient.queryUsage(hours, force));
  }
  let renderUsage = (): void => {};
  const preparationFailures = new Map<string, { at: number; message: string }>();
  let needsManualRegistration = false;
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
          output.error(`CACHE_REGISTRATION_FAILED root=${singleLine(path)}: ${singleLine(String(error))}`);
        })
        .finally(() => { pending.delete(path); });
      pending.set(path, operation);
    }
    throw new CacheMissError(`CACHE_MISS root=${path} state=registering background_load=true: source is not ready. Continue with a permitted host search/read under the same policy; do not poll, repeat this lookup, switch CCTX transports or run code_sync to wait for warmup.`);
  }, (event) => {
    if (event.operation !== 'code_workspaces' && !event.error) {
      needsManualRegistration = false;
    }
    usage.record(event);
    renderUsage();
  }, (operation, deliveredChars) => {
    usage.recordPayload(operation, deliveredChars, false);
  });
  bridge.register();

  const status = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    20,
  );
  status.name = 'Code Context Native';
  status.command = hasBackendUsageQuery ? 'codeContextNative.showDashboard' : 'codeContextNative.showStatus';
  status.text = '$(database) CCTX Native';
  status.tooltip = 'Show Code Context native tool status';
  status.show();
  let renderTimer: NodeJS.Timeout | undefined;
  let periodicUsageTimer: NodeJS.Timeout | undefined;
  let autoBpeTimer: NodeJS.Timeout | undefined;
  let periodicBpeTimer: NodeJS.Timeout | undefined;
  let dashboardPanel: vscode.WebviewPanel | undefined;
  const applyStatusBarUsage = (target: vscode.StatusBarItem): void => {
    if (!usage.hasRenderableData) { return; }
    if (dashboardPanel) {
      try {
        dashboardPanel.webview.html = usage.renderDashboardHtml();
      } catch {
        // Ignore disposed panel race.
      }
    }
    if (needsManualRegistration) { return; }
    if (hasBackendUsageQuery) {
      target.command = 'codeContextNative.showDashboard';
    }
    target.text = usage.text();
    if (typeof vscode.MarkdownString === 'function') {
      const md = new vscode.MarkdownString(usage.markdownTooltip(), true);
      md.isTrusted = true;
      target.tooltip = md;
    } else {
      target.tooltip = usage.tooltip();
    }
  };
  const applyTooltip = (): void => {
    applyStatusBarUsage(status);
  };
  const scheduleUsageRefresh = (force = false): void => {
    if (typeof setTimeout !== 'function') {
      void usage.refreshSessionTelemetryAsync(undefined, Date.now(), force).finally(() => {
        applyTooltip();
      });
      return;
    }
    if (!renderTimer) {
      renderTimer = setTimeout(() => {
        renderTimer = undefined;
        void usage.refreshSessionTelemetryAsync(undefined, Date.now(), force).finally(() => {
          applyTooltip();
        });
      }, 250);
      renderTimer.unref?.();
    }
  };
  renderUsage = (): void => {
    if (usage.hasRenderableData && !needsManualRegistration) {
      status.text = usage.text();
    }
    if (usage.last || hasBackendUsageQuery) {
      scheduleUsageRefresh(false);
    }
  };
  if (hasBackendUsageQuery && typeof setInterval === 'function') {
    periodicUsageTimer = setInterval(() => {
      if (!vscode.workspace.isTrusted) { return; }
      const winState = (vscode.window as { state?: { focused?: boolean } }).state;
      if (winState && winState.focused === false) { return; }
      void usage.refreshSessionTelemetryAsync().finally(() => {
        applyTooltip();
      });
    }, 60_000);
    periodicUsageTimer.unref?.();
  }
  const hasBackendBpeBuild =
    typeof (activeClient as { buildBpeProfile?: unknown }).buildBpeProfile === 'function';
  let bpeBackendUnsupported = false;
  let bpeAutoInFlight = false;
  const runAutoBpeCheck = (): void => {
    if (!hasBackendBpeBuild || bpeBackendUnsupported || bpeAutoInFlight || !vscode.workspace.isTrusted) {
      return;
    }
    const winState = (vscode.window as { state?: { focused?: boolean } }).state;
    if (winState && winState.focused === false) { return; }
    const settings = readSettings();
    if (settings.autoBuildBpeProfile === false) { return; }
    const includeWindows = process.platform === 'win32' && activeClient.usesWslBridge;
    bpeAutoInFlight = true;
    void activeClient
      .buildBpeProfile({
        window: '7d',
        if_needed: true,
        ...(includeWindows ? { include_windows_homes: true } : {}),
      })
      .then((res) => {
        if (res && typeof res === 'object' && !Array.isArray(res) && res.applied === true) {
          const reason = typeof res.reason === 'string' ? res.reason : 'updated';
          output.appendLine(`BPE_AUTO_BUILT: ${singleLine(reason)}`);
        }
      })
      .catch((error: unknown) => {
        const msg = String(error);
        if (/unknown|unsupported/iu.test(msg)) {
          bpeBackendUnsupported = true;
        }
        output.debug?.(`BPE_AUTO_CHECK_SKIPPED: ${singleLine(msg)}`);
      })
      .finally(() => {
        bpeAutoInFlight = false;
      });
  };
  if (hasBackendBpeBuild && typeof setTimeout === 'function') {
    autoBpeTimer = setTimeout(() => {
      autoBpeTimer = undefined;
      runAutoBpeCheck();
    }, 45_000);
    autoBpeTimer.unref?.();
  }
  if (hasBackendBpeBuild && typeof setInterval === 'function') {
    periodicBpeTimer = setInterval(() => {
      runAutoBpeCheck();
    }, 6 * 60 * 60 * 1000);
    periodicBpeTimer.unref?.();
  }

  let probing: Promise<void> | undefined;
  const probe = (): Promise<void> => {
    probing ??= runStartupProbe(bridge, status, output).then((unregistered) => {
      needsManualRegistration = unregistered;
    }).finally(() => {
      probing = undefined;
      renderUsage();
    });
    return probing;
  };

  context.subscriptions.push(
    {
      dispose: () => {
        if (renderTimer && typeof clearTimeout === 'function') {
          clearTimeout(renderTimer);
          renderTimer = undefined;
        }
        if (periodicUsageTimer && typeof clearInterval === 'function') {
          clearInterval(periodicUsageTimer);
          periodicUsageTimer = undefined;
        }
        if (autoBpeTimer && typeof clearTimeout === 'function') {
          clearTimeout(autoBpeTimer);
          autoBpeTimer = undefined;
        }
        if (periodicBpeTimer && typeof clearInterval === 'function') {
          clearInterval(periodicBpeTimer);
          periodicBpeTimer = undefined;
        }
        dashboardPanel?.dispose();
        dashboardPanel = undefined;
      },
    },
    output,
    status,
    bridge,
    client,
    vscode.commands.registerCommand('codeContextNative.gettingStarted', async () => {
      await vscode.commands.executeCommand(
        'workbench.action.openWalkthrough', `${context.extension.id}#getting-started`, false,
      );
    }),
    vscode.commands.registerCommand('codeContextNative.configureConnection', async () => {
      await vscode.commands.executeCommand('workbench.action.openSettings', `@ext:${context.extension.id}`);
    }),
    vscode.commands.registerCommand('codeContextNative.openSetupGuide', async () => {
      const opened = await vscode.env.openExternal(vscode.Uri.parse(
        'https://github.com/dotnetpower/code-context/blob/main/vscode-extension/docs/SETUP.md',
      ));
      if (!opened) {
        await vscode.window.showErrorMessage('Could not open the Code Context setup guide.');
      }
    }),
    vscode.commands.registerCommand('codeContextNative.showDashboard', async () => {
      dashboardPanel = await showDashboard(bridge, status, output, usage, dashboardPanel, () => {
        dashboardPanel = undefined;
      });
    }),
    vscode.commands.registerCommand('codeContextNative.showStatus', async () => {
      await showStatus(bridge, status, output, usage, usage.lastRoot);
    }),
    vscode.commands.registerCommand('codeContextNative.selfTest', async (root?: string) => {
      await runSelfTest(bridge, output, context.extension, root ?? usage.lastRoot);
    }),
    vscode.commands.registerCommand(
      'codeContextNative.registerWorkspace',
      async () => {
        if (client?.usesWslBridge) {
          const message = 'Register manual workspaces with cctx register inside WSL. Authorized worktrees are prepared lazily by the daemon.';
          output.warn(`REGISTRATION_REQUIRED: ${message}`);
          await vscode.window.showErrorMessage(message);
          return;
        }
        if (await registerCurrentWorkspace(bridge, output)) {
          await probe();
        }
      },
    ),
    vscode.commands.registerCommand(
      'codeContextNative.buildBpeProfile',
      async (mode?: string) => {
        await runBuildBpeProfileCommand(activeClient, output, mode);
      },
    ),
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('codeContextNative')) {
        client?.reset();
        bridge.invalidateRegistry();
        output.appendLine('Code Context native settings changed; applying on next request.');
        if (!bridge.usesWslBridge && event.affectsConfiguration('codeContextNative.autoRegisterWorkspace')) {
          void probe();
        }
      }
    }),
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      void probe();
    }),
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      bridge.invalidateRegistry();
      if (vscode.workspace.isTrusted && !bridge.usesWslBridge) {
        void probe();
      }
    }),
    ...(typeof (vscode.window as { onDidChangeWindowState?: unknown }).onDidChangeWindowState === 'function'
      ? [
          vscode.window.onDidChangeWindowState((state) => {
            if (state.focused && vscode.workspace.isTrusted && (usage.last || hasBackendUsageQuery)) {
              scheduleUsageRefresh(false);
            }
          }),
        ]
      : []),
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

function orderedWorkspaceRoots(
  bridge: NativeToolBridge,
  workspaces: ReadonlyArray<{ root: string; scopes: string[]; globs: string[] }>,
): string[] {
  if (typeof (bridge as { prioritizeOpenWslRoots?: unknown }).prioritizeOpenWslRoots === 'function') {
    return bridge.prioritizeOpenWslRoots(workspaces);
  }
  return workspaces.map((workspace) => workspace.root);
}

function isAgentsWindowWithoutVisibleEditors(): boolean {
  const envObj = (vscode as { env?: { appName?: string } }).env;
  if (typeof envObj?.appName === 'string' && /agents/iu.test(envObj.appName)) {
    return true;
  }
  const folders = vscode.workspace.workspaceFolders ?? [];
  const hasAgentHostFolder = folders.some(
    (f) => typeof f.uri?.scheme === 'string' && f.uri.scheme === 'vscode-agent-host',
  );
  const visibleEditors = vscode.window.visibleTextEditors ?? [];
  return hasAgentHostFolder && visibleEditors.length === 0;
}

async function showDashboard(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.LogOutputChannel,
  usage: UsageSummary,
  existingPanel: vscode.WebviewPanel | undefined,
  onPanelDisposed: () => void,
): Promise<vscode.WebviewPanel | undefined> {
  try {
    await usage.refreshSessionTelemetryAsync(undefined, Date.now(), true);
    if (usage.hasRenderableData) {
      statusBar.text = usage.text();
      if (typeof vscode.MarkdownString === 'function') {
        const md = new vscode.MarkdownString(usage.markdownTooltip(), true);
        md.isTrusted = true;
        statusBar.tooltip = md;
      } else {
        statusBar.tooltip = usage.tooltip();
      }
    }
    let daemonStatus: unknown = usage.backendUsage?.rawDaemon;
    if (!daemonStatus && usage.lastRoot) {
      const source = new vscode.CancellationTokenSource();
      try {
        const root = await bridge.resolveRoot(usage.lastRoot, source.token, false);
        daemonStatus = await bridge.invoke(
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
      } catch {
        // Optional per-root status fallback; dashboard still renders cleanly without a root.
      } finally {
        source.dispose();
      }
    }
    output.appendLine(`\n${usage.formatDashboard(daemonStatus)}`);
    if (daemonStatus) {
      output.info(formatLogDetails('Code Context status', daemonStatus as JsonValue));
    }

    const winWithWebview = vscode.window as {
      createWebviewPanel?: typeof vscode.window.createWebviewPanel;
    };
    if (!isAgentsWindowWithoutVisibleEditors() && typeof winWithWebview.createWebviewPanel === 'function') {
      try {
        const col = vscode.ViewColumn?.Active ?? 1;
        let panel = existingPanel;
        if (panel) {
          panel.reveal(col, false);
        } else {
          panel = winWithWebview.createWebviewPanel(
            'codeContextDashboard',
            'Code Context — 24h Dashboard',
            col,
            { enableScripts: false, retainContextWhenHidden: false },
          );
          panel.onDidDispose(() => {
            onPanelDisposed();
          });
        }
        panel.webview.html = usage.renderDashboardHtml(daemonStatus);
        if (typeof setTimeout === 'function') {
          await new Promise<void>((resolveDelay) => {
            const t = setTimeout(resolveDelay, 30);
            t.unref?.();
          });
        }
        if (panel.visible !== false) {
          return panel;
        }
        panel.dispose();
      } catch {
        // Fall back to focusing the Output channel if editor tabs/webviews are unavailable in this window.
      }
    }
    output.show(false);
    return undefined;
  } catch (error: unknown) {
    await showFailure(error, output);
    return existingPanel;
  }
}

async function showStatus(
  bridge: NativeToolBridge,
  statusBar: vscode.StatusBarItem,
  output: vscode.LogOutputChannel,
  usage: UsageSummary,
  explicitRoot?: string,
): Promise<void> {
  try {
    const source = new vscode.CancellationTokenSource();
    try {
      let requestedRoot = explicitRoot;
      if (!requestedRoot && bridge.usesWslBridge) {
        const workspaces = await bridge.listWorkspaces(source.token);
        const candidates = orderedWorkspaceRoots(bridge, workspaces);
        requestedRoot = candidates.length === 1 ? candidates[0] :
          await vscode.window.showQuickPick(candidates,
            { title: 'Code Context: Select a registered Linux root', placeHolder: 'Status does not load source.' });
        if (!requestedRoot) {
          output.info('STATUS_CANCELLED no registered root selected; register a Linux root if the list is empty.');
          return;
        }
      }
      const root = await bridge.resolveRoot(requestedRoot, source.token, false);
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
      await usage.refreshSessionTelemetryAsync(undefined, Date.now(), true);
      if (usage.hasRenderableData) {
        statusBar.text = usage.text();
        if (typeof vscode.MarkdownString === 'function') {
          const md = new vscode.MarkdownString(usage.markdownTooltip(), true);
          md.isTrusted = true;
          statusBar.tooltip = md;
        } else {
          statusBar.tooltip = usage.tooltip();
        }
      }
      output.appendLine(`\n${usage.formatDashboard(value)}`);
      output.info(formatLogDetails('Code Context status', value));
      output.show(false);
    } finally {
      source.dispose();
    }
  } catch (error: unknown) {
    if (error instanceof RootResolutionError) {
      if (bridge.usesWslBridge) {
        await showFailure(error, output);
        return;
      }
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
  output: vscode.LogOutputChannel,
  extension: vscode.Extension<unknown>,
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
          orderedWorkspaceRoots(bridge, workspaces),
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
      const metadata: unknown = extension.packageJSON;
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
      output.info(formatLogDetails('Code Context native self-test', report));
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
  output: vscode.LogOutputChannel,
): Promise<boolean> {
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
    const root = bridge.usesWslBridge && workspaces.length !== 1
      ? 'explicit Linux root required'
      : await bridge.resolveRoot(undefined, source.token, false);
    statusBar.text = '$(database) CCTX Available';
    statusBar.tooltip = `${registered.length} native tools registered and callable for ${root}; Chat picker enablement is user-controlled`;
    output.appendLine(
      `EXTENSION_CALLABLE tools=${registered.length} workspaces=${workspaces.length} root=${root} probe=codeWorkspaces index_initialization=on-demand source_scan=none elapsed_ms=${(performance.now() - started).toFixed(3)} agent_picker=unverified; enable Code Context tools in the agent tool settings`,
    );
    return false;
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    if (error instanceof RootResolutionError) {
      statusBar.command = 'codeContextNative.registerWorkspace';
      statusBar.text = '$(add) CCTX Register';
      statusBar.tooltip = 'Index initialization is deferred until the first source lookup; click for manual registration';
      output.appendLine(`NATIVE_UNREGISTERED_DEFERRED ${message}`);
      return true;
    }
    statusBar.text = '$(warning) CCTX Native';
    statusBar.tooltip = message;
    output.error(`NATIVE_NOT_READY ${singleLine(message)}`);
    return false;
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
    .filter((text) => !text.startsWith('CCTX_TRACE ') && !text.startsWith('CCTX_SLOW '))
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
  const localWindowsWorkspace =
    process.platform === 'win32' &&
    (vscode.workspace.workspaceFolders ?? []).some(
      (folder) => folder.uri.scheme === 'file' && /^[A-Za-z]:[\\/]/u.test(folder.uri.fsPath),
    );
  return {
    socketPath: config.get<string>('socketPath', ''),
    autoStartService: config.get<boolean>('autoStartService', true),
    requestTimeoutMs: config.get<number>('requestTimeoutMs', 40_000),
    executablePath: config.get<string>('executablePath', ''),
    wslDistribution: localWindowsWorkspace ? '' : config.get<string>('wslDistribution', ''),
    wslExecutablePath: localWindowsWorkspace ? '' : config.get<string>('wslExecutablePath', ''),
    autoBuildBpeProfile: config.get<boolean>('autoBuildBpeProfile', true),
  };
}

async function runBuildBpeProfileCommand(
  activeClient: DaemonClient,
  output: vscode.LogOutputChannel,
  presetMode?: string,
): Promise<void> {
  try {
    let selected = presetMode;
    if (!selected) {
      const pick = await vscode.window.showQuickPick(
        [
          {
            label: 'Build / Update Now (Last 7 days)',
            description: 'Train BPE merges & safe formula rules from Copilot sessions',
            mode: 'build-7d',
          },
          {
            label: 'Build / Update Now (Last 30 days)',
            description: 'Broader 30-day session corpus',
            mode: 'build-30d',
          },
          {
            label: 'Auto-build If Needed',
            description: 'Rebuild only when session activity or pattern drift crosses threshold',
            mode: 'if-needed',
          },
          {
            label: 'Preview (Dry Run, 7 days)',
            description: 'Inspect learned rules and token savings without saving',
            mode: 'dry-run',
          },
          {
            label: 'Show BPE Profile Status',
            description: 'Inspect current profile summary and rebuild readiness',
            mode: 'status',
          },
          {
            label: 'Rollback Previous Profile',
            description: 'Restore bpe-profile.prev.json backup',
            mode: 'rollback',
          },
          {
            label: 'Reset to Built-in Rules',
            description: 'Remove persisted bpe-profile.json',
            mode: 'reset',
          },
        ],
        {
          title: 'Code Context: Session BPE Profile Builder',
          placeHolder: 'Choose a BPE profile action (works on both WSL and Windows)',
        },
      );
      if (!pick) {
        return;
      }
      selected = pick.mode;
    }

    const includeWindows = process.platform === 'win32' && activeClient.usesWslBridge;
    const extra = includeWindows ? { include_windows_homes: true } : {};
    const result =
      selected === 'status'
        ? await activeClient.queryBpeStatus({ window: '7d', ...extra })
        : selected === 'build-30d'
          ? await activeClient.buildBpeProfile({ window: '30d', ...extra })
          : selected === 'if-needed'
            ? await activeClient.buildBpeProfile({ window: '7d', if_needed: true, ...extra })
            : selected === 'dry-run'
              ? await activeClient.buildBpeProfile({ window: '7d', dry_run: true, ...extra })
              : selected === 'rollback'
                ? await activeClient.buildBpeProfile({ rollback: true })
                : selected === 'reset'
                  ? await activeClient.buildBpeProfile({ reset: true })
                  : await activeClient.buildBpeProfile({ window: '7d', ...extra });

    output.info(formatLogDetails(`Code Context BPE (${selected})`, result));
    output.show(false);
    const rec =
      result && typeof result === 'object' && !Array.isArray(result) ? result : undefined;
    const action = typeof rec?.action === 'string' ? rec.action : selected;
    const reason = typeof rec?.reason === 'string' ? rec.reason : '';
    await vscode.window.showInformationMessage(
      `Code Context BPE (${action})${reason ? `: ${reason}` : ''}`,
    );
  } catch (error: unknown) {
    await showFailure(error, output);
  }
}

async function showFailure(
  error: unknown,
  output: vscode.LogOutputChannel,
): Promise<void> {
  const message = error instanceof Error ? error.message : String(error);
  output.error(singleLine(message));
  output.show(true);
  await vscode.window.showErrorMessage(`Code Context native tools: ${message}`);
}
