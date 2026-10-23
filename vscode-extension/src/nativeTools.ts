import { resolve } from 'node:path';
import { randomUUID } from 'node:crypto';

import * as vscode from 'vscode';

import { TracedRequestError, type ClientTrace, type DaemonClient } from './daemonClient.js';
import { formatToolResult } from './resultFormatter.js';
import { resolveRegisteredRootAfterRefresh, resolveWslRoot } from './rootResolver.js';
import type { DaemonRequest, JsonValue, WorkspaceSpec } from './types.js';
import { classifyOutcome, type UsageEvent } from './usage.js';

const REGISTRY_CACHE_MS = 5_000;
export const NATIVE_TOOL_NAMES = [
  'code-context-native_codeFiles',
  'code-context-native_codeGrep',
  'code-context-native_codeContext',
  'code-context-native_codeRead',
  'code-context-native_codeStatus',
  'code-context-native_codeSync',
  'code-context-native_codeWorkspaces',
] as const;

interface RootInput {
  root?: string;
}

interface FilesInput extends RootInput {
  glob: string;
  paths?: string[];
  limit?: number;
}

interface SearchInput extends RootInput {
  pattern: string;
  regex?: boolean;
  ignore_case?: boolean;
  limit?: number;
  paths?: string[];
  glob?: string;
}

interface ContextInput extends SearchInput {
  radius?: number;
}

interface ReadInput extends RootInput {
  split_ranges?: boolean;
  page?: boolean;
  ranges: Array<{
    path: string;
    start: number;
    end: number;
  }>;
}

type EmptyInput = Record<string, never>;

interface ToolDefinition<T> {
  name: string;
  operation: string;
  invocationMessage(input: T): string;
  needsRoot: boolean;
  arguments(input: T): Record<string, JsonValue>;
}

class WorkspaceRegistry {
  private cached: WorkspaceSpec[] | undefined;
  private cachedAt = 0;

  public constructor(private readonly client: DaemonClient) {}

  public async list(
    token: vscode.CancellationToken,
    refresh = false,
  ): Promise<WorkspaceSpec[]> {
    if (!refresh && this.cached && Date.now() - this.cachedAt < REGISTRY_CACHE_MS) {
      return this.cached;
    }
    const result = await this.client.request<unknown>(
      request(null, 'code_workspaces', {}),
      token,
    );
    if (!Array.isArray(result)) {
      throw new Error('Code Context daemon returned an invalid workspace registry.');
    }
    const workspaces = result.map(parseWorkspaceSpec);
    this.cached = workspaces;
    this.cachedAt = Date.now();
    return workspaces;
  }

  public invalidate(): void {
    this.cached = undefined;
    this.cachedAt = 0;
  }
}

export class NativeToolBridge implements vscode.Disposable {
  private readonly registry: WorkspaceRegistry;
  private readonly disposables: vscode.Disposable[] = [];

  public constructor(
    private readonly client: DaemonClient,
    private readonly output: vscode.OutputChannel,
    private readonly registerMissingRoot?: (path: string) => Promise<void>,
    private readonly onUsage?: (event: UsageEvent) => void,
  ) {
    this.registry = new WorkspaceRegistry(client);
  }

  public register(): void {
    const definitions: Array<ToolDefinition<object>> = [
      filesDefinition,
      searchDefinition,
      contextDefinition,
      readDefinition,
      statusDefinition,
      syncDefinition,
      workspacesDefinition,
    ];
    for (const definition of definitions) {
      const disposable = vscode.lm.registerTool(
        definition.name,
        new NativeTool(definition, this),
      );
      this.disposables.push(disposable);
    }
  }

  public async invoke<T>(
    definition: ToolDefinition<T>,
    input: T,
    token: vscode.CancellationToken,
  ): Promise<JsonValue> {
    return (await this.invokeTraced(definition, input, token)).value;
  }

  public async invokeTraced<T>(
    definition: ToolDefinition<T>,
    input: T,
    token: vscode.CancellationToken,
  ): Promise<{ value: JsonValue; trace: ClientTrace }> {
    ensureTrustedWorkspace();
    const started = performance.now();
    const startedUnixMs = Date.now();
    const traceId = randomUUID();
    let root: string | null = null;
    try {
      root = definition.needsRoot
        ? await this.resolveRoot((input as RootInput).root, token)
        : null;
      const result = await this.client.requestTraced<JsonValue>(
        request(root, definition.operation, definition.arguments(input)),
        token,
        traceId,
        performance.now() - started,
      );
      result.trace.total_ms = performance.now() - started;
      result.trace.started_unix_ms = startedUnixMs;
      result.trace.completed_unix_ms = Date.now();
      result.trace.outcome = 'success';
      this.onUsage?.({ operation: definition.operation, root });
      this.output.appendLine(
        `${definition.operation} root=${root ?? '-'} ${result.trace.total_ms.toFixed(3)}ms trace=${traceId}`,
      );
      if (definition.operation === 'code_sync') {
        this.registry.invalidate();
      }
      return result;
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : String(error);
      this.onUsage?.({ operation: definition.operation, root, error: message });
      if (error instanceof TracedRequestError) {
        error.trace.outcome = classifyOutcome(message);
        error.trace.started_unix_ms = startedUnixMs;
        error.trace.completed_unix_ms = Date.now();
        this.recordTrace(error.trace, token);
      }
      this.output.appendLine(`${definition.operation} failed trace=${traceId}: ${message}`);
      throw new Error(message.includes(`[cctx_trace=${traceId}]`)
        ? message : `${message} [cctx_trace=${traceId}]`, { cause: error });
    }
  }

  public recordTrace(trace: ClientTrace, token: vscode.CancellationToken): void {
    this.output.appendLine(`CCTX_CLIENT_TRACE ${JSON.stringify(trace)}`);
    if (trace.available !== true) {
      return;
    }
    void this.client.request(request(null, 'code_trace', trace), token).catch((error: unknown) => {
      this.output.appendLine(`CCTX_TRACE_REPORT_FAILED trace=${trace.trace_id}: ${String(error)}`);
    });
  }

  public async listWorkspaces(token: vscode.CancellationToken): Promise<WorkspaceSpec[]> {
    ensureTrustedWorkspace();
    return this.registry.list(token, true);
  }

  public async resolveRoot(
    explicit: string | undefined,
    token: vscode.CancellationToken,
    allowPreparation = true,
  ): Promise<string> {
    ensureTrustedWorkspace();
    const candidates = workspaceCandidates();
    const workspaces = await this.registry.list(token);
    if (this.client.usesWslBridge) {
      return resolveWslRoot(workspaces, explicit);
    }
    return resolveRegisteredRootAfterRefresh(
      workspaces,
      () => this.registry.list(token, true),
      explicit,
      candidates,
      allowPreparation ? this.registerMissingRoot : undefined,
    );
  }

  public dispose(): void {
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    this.disposables.length = 0;
  }

  public invalidateRegistry(): void {
    this.registry.invalidate();
  }
}

class NativeTool<T> implements vscode.LanguageModelTool<T> {
  public constructor(
    private readonly definition: ToolDefinition<T>,
    private readonly bridge: NativeToolBridge,
  ) {}

  public async invoke(
    options: vscode.LanguageModelToolInvocationOptions<T>,
    token: vscode.CancellationToken,
  ): Promise<vscode.LanguageModelToolResult> {
    const result = await this.bridge.invokeTraced(this.definition, options.input, token);
    const started = performance.now();
    const text = formatToolResult(this.definition.operation, result.value);
    result.trace.serialize_ms = performance.now() - started;
    result.trace.total_ms += result.trace.serialize_ms;
    result.trace.completed_unix_ms = Date.now();
    this.bridge.recordTrace(result.trace, token);
    return new vscode.LanguageModelToolResult([
      new vscode.LanguageModelTextPart(text),
      new vscode.LanguageModelTextPart(`CCTX_TRACE ${JSON.stringify(result.trace)}`),
    ]);
  }

  public prepareInvocation(
    options: vscode.LanguageModelToolInvocationPrepareOptions<T>,
    _token: vscode.CancellationToken,
  ): vscode.ProviderResult<vscode.PreparedToolInvocation> {
    void _token;
    return {
      invocationMessage: this.definition.invocationMessage(options.input),
    };
  }
}

const searchDefinition: ToolDefinition<SearchInput> = {
  name: 'code-context-native_codeGrep',
  operation: 'code_grep',
  needsRoot: true,
  invocationMessage: (input) => `Searching indexed source for "${input.pattern}"`,
  arguments: searchArguments,
};

const filesDefinition: ToolDefinition<FilesInput> = {
  name: 'code-context-native_codeFiles',
  operation: 'code_files',
  needsRoot: true,
  invocationMessage: (input) => `Finding indexed files matching "${input.glob}"`,
  arguments: (input) => {
    const args: Record<string, JsonValue> = { glob: input.glob };
    if (input.paths !== undefined) {
      args.paths = input.paths;
    }
    if (input.limit !== undefined) {
      args.limit = input.limit;
    }
    return args;
  },
};

const contextDefinition: ToolDefinition<ContextInput> = {
  name: 'code-context-native_codeContext',
  operation: 'code_context',
  needsRoot: true,
  invocationMessage: (input) =>
    `Searching indexed source with context for "${input.pattern}"`,
  arguments: (input) => {
    const args = searchArguments(input);
    if (input.radius !== undefined) {
      args.radius = input.radius;
    }
    return args;
  },
};

const readDefinition: ToolDefinition<ReadInput> = {
  name: 'code-context-native_codeRead',
  operation: 'code_read',
  needsRoot: true,
  invocationMessage: (input) => `Reading ${input.ranges.length} indexed source range(s)`,
  arguments: (input) => ({
    split_ranges: input.split_ranges ?? true,
    page: input.page ?? false,
    ranges: input.ranges.map((range) => ({
      path: range.path,
      start: range.start,
      end: range.end,
    })),
  }),
};

const statusDefinition: ToolDefinition<RootInput> = {
  name: 'code-context-native_codeStatus',
  operation: 'code_status',
  needsRoot: true,
  invocationMessage: () => 'Reading Code Context index status',
  arguments: () => ({}),
};

const syncDefinition: ToolDefinition<RootInput> = {
  name: 'code-context-native_codeSync',
  operation: 'code_sync',
  needsRoot: true,
  invocationMessage: () => 'Synchronizing saved source with the Code Context index',
  arguments: () => ({}),
};

const workspacesDefinition: ToolDefinition<EmptyInput> = {
  name: 'code-context-native_codeWorkspaces',
  operation: 'code_workspaces',
  needsRoot: false,
  invocationMessage: () => 'Listing registered Code Context workspaces',
  arguments: () => ({}),
};

function searchArguments(input: SearchInput): Record<string, JsonValue> {
  const args: Record<string, JsonValue> = { pattern: input.pattern };
  if (input.regex !== undefined) {
    args.regex = input.regex;
  }
  if (input.ignore_case !== undefined) {
    args.ignore_case = input.ignore_case;
  }
  if (input.limit !== undefined) {
    args.limit = input.limit;
  }
  if (input.paths !== undefined) {
    args.paths = input.paths;
  }
  if (input.glob !== undefined) {
    args.glob = input.glob;
  }
  return args;
}

function request(
  root: string | null,
  name: string,
  argumentsValue: Record<string, JsonValue>,
): DaemonRequest {
  return {
    root,
    name,
    arguments: argumentsValue,
    compact: true,
  };
}

function parseWorkspaceSpec(value: unknown): WorkspaceSpec {
  if (!isRecord(value) || typeof value.root !== 'string') {
    throw new Error('Code Context workspace registry contains an invalid root.');
  }
  if (!stringArray(value.scopes) || !stringArray(value.globs)) {
    throw new Error(`Code Context workspace ${value.root} has invalid scopes or globs.`);
  }
  const spec: WorkspaceSpec = {
    root: resolve(value.root),
    scopes: value.scopes,
    globs: value.globs,
  };
  if (stringArray(value.excludes)) {
    spec.excludes = value.excludes;
  }
  if (typeof value.default_excludes === 'boolean') {
    spec.default_excludes = value.default_excludes;
  }
  if (typeof value.prewarm === 'boolean') {
    spec.prewarm = value.prewarm;
  }
  if (typeof value.allow_worktrees === 'boolean') {
    spec.allow_worktrees = value.allow_worktrees;
  }
  if (typeof value.managed_parent === 'string' || value.managed_parent === null) {
    spec.managed_parent = value.managed_parent;
  }
  return spec;
}

function workspaceCandidates(): string[] {
  const candidates: string[] = [];
  const active = vscode.window.activeTextEditor?.document.uri;
  if (active?.scheme === 'file' || active?.scheme === 'vscode-remote') {
    candidates.push(active.fsPath);
  }
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    candidates.push(folder.uri.fsPath);
  }
  return candidates;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === 'string');
}

function ensureTrustedWorkspace(): void {
  if (!vscode.workspace.isTrusted) {
    throw new Error(
      'Code Context native tools are disabled until this workspace is trusted.',
    );
  }
}
