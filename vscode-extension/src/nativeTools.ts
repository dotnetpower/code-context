import { existsSync } from 'node:fs';
import { basename, join, posix, resolve } from 'node:path';
import { randomUUID } from 'node:crypto';

import * as vscode from 'vscode';

import { TracedRequestError, type ClientTrace, type DaemonClient } from './daemonClient.js';
import { formatToolResult } from './resultFormatter.js';
import { formatTrace, outcomeLevel, singleLine } from './logFormatter.js';
import {
  resolveRegisteredRootAfterRefresh,
  resolveWslRoot,
  wslCandidateToLinuxPath,
} from './rootResolver.js';
import type { DaemonRequest, JsonValue, WorkspaceSpec } from './types.js';
import { classifyOutcome, type UsageEvent } from './usage.js';

const REGISTRY_CACHE_MS = 5_000;
const STICKY_ROOT_TTL_MS = 300_000;
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
  glob?: string;
  pattern?: string;
  paths?: string[] | string;
  path?: string;
  limit?: number;
}

interface SearchInput extends RootInput {
  pattern?: string;
  query?: string;
  regex?: boolean;
  ignore_case?: boolean;
  limit?: number;
  max_matches_per_file?: number;
  paths?: string[] | string;
  path?: string;
  glob?: string;
}

interface ContextInput extends SearchInput {
  radius?: number;
}

interface ReadRangeInput {
  path?: string;
  filePath?: string;
  start?: number;
  end?: number;
  start_line?: number;
  end_line?: number;
  startLine?: number;
  endLine?: number;
  view_range?: readonly [number, number];
}

interface ReadInput extends RootInput {
  split_ranges?: boolean;
  page?: boolean;
  ranges?: ReadRangeInput[];
  path?: string;
  filePath?: string;
  start?: number;
  end?: number;
  start_line?: number;
  end_line?: number;
  startLine?: number;
  endLine?: number;
  view_range?: readonly [number, number];
}

type EmptyInput = Record<string, never>;

interface ToolDefinition<T> {
  name: string;
  operation: string;
  invocationMessage(input: T): string;
  needsRoot: boolean;
  arguments(input: T, root?: string | null): Record<string, JsonValue>;
}

class WorkspaceRegistry {
  private cached: WorkspaceSpec[] | undefined;
  private cachedAt = 0;
  private inFlight: Promise<WorkspaceSpec[]> | undefined;

  public constructor(private readonly client: DaemonClient) {}

  public async list(
    token: vscode.CancellationToken,
    refresh = false,
  ): Promise<WorkspaceSpec[]> {
    if (!refresh && this.cached && Date.now() - this.cachedAt < REGISTRY_CACHE_MS) {
      return this.cached;
    }
    if (this.inFlight) {
      return this.inFlight;
    }
    const pending = (async (): Promise<WorkspaceSpec[]> => {
      const result = await this.client.request<unknown>(
        request(null, 'code_workspaces', {}),
        token,
      );
      return this.seed(result);
    })().finally(() => {
      this.inFlight = undefined;
    });
    this.inFlight = pending;
    return pending;
  }

  public seed(result: unknown): WorkspaceSpec[] {
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
  private lastResolvedRoot: { root: string; at: number } | undefined;

  public constructor(
    private readonly client: DaemonClient,
    private readonly output: vscode.LogOutputChannel,
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
    const result = await this.invokeTraced(definition, input, token);
    this.recordTrace(result.trace, token);
    return result.value;
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
      const explicit = normalizeExplicitRootInput((input as RootInput).root, this.client.usesWslBridge);
      root = definition.needsRoot
        ? await this.resolveRoot(
            explicit,
            token,
            true,
            extractInputPathCandidates(input),
            extractRelativePathTargets(input),
          )
        : null;
      if (root) {
        this.lastResolvedRoot = { root, at: Date.now() };
      }
      const result = await this.client.requestTraced<JsonValue>(
        request(root, definition.operation, definition.arguments(input, root)),
        token,
        traceId,
        performance.now() - started,
      );
      if (definition.operation === 'code_workspaces' && Array.isArray(result.value)) {
        try {
          this.registry.seed(result.value);
        } catch {
          // Best-effort cache seeding; validation happens when registry is consumed.
        }
      }
      result.trace.total_ms = performance.now() - started;
      result.trace.started_unix_ms = startedUnixMs;
      result.trace.completed_unix_ms = Date.now();
      result.trace.outcome = 'success';
      this.onUsage?.({ operation: definition.operation, root });
      if (definition.operation === 'code_sync') {
        this.registry.invalidate();
      }
      return result;
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : String(error);
      this.onUsage?.({ operation: definition.operation, root, error: message });
      const elapsed = performance.now() - started;
      const trace: ClientTrace = error instanceof TracedRequestError ? error.trace : {
        trace_id: traceId, origin: 'native', operation: definition.operation,
        root: root ?? (definition.needsRoot ? (input as RootInput).root ?? null : null),
        queue_ms: 0, prepare_ms: elapsed, ipc_ms: 0, serialize_ms: 0, write_ms: 0,
        total_ms: elapsed, success: false, available: false, daemon: null,
        started_unix_ms: startedUnixMs, completed_unix_ms: Date.now(),
      };
      trace.outcome = classifyOutcome(message);
      trace.started_unix_ms = startedUnixMs;
      trace.completed_unix_ms = Date.now();
      this.recordTrace(trace, token, message);
      throw new Error(message.includes(`[cctx_trace=${traceId}]`)
        ? message : `${message} [cctx_trace=${traceId}]`, { cause: error });
    }
  }

  public recordTrace(trace: ClientTrace, token: vscode.CancellationToken, reason?: string): void {
    const outcome = trace.success ? 'success' : classifyOutcome(reason ?? 'Unknown native request failure');
    this.output[outcomeLevel(outcome)](formatTrace(trace, reason));
    if (trace.available !== true) {
      return;
    }
    void this.client.request(request(null, 'code_trace', trace), token).catch((error: unknown) => {
      this.output.warn(`CCTX_TRACE_REPORT_FAILED trace=${trace.trace_id}: ${singleLine(String(error))}`);
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
    extraCandidates: readonly string[] = [],
    relativeTargets: readonly string[] = [],
  ): Promise<string> {
    ensureTrustedWorkspace();
    const normalizedExplicit = normalizeExplicitRootInput(explicit, this.client.usesWslBridge);
    if (
      this.client.usesWslBridge &&
      normalizedExplicit !== undefined &&
      (normalizedExplicit.startsWith('/') || /[\\:]/u.test(normalizedExplicit))
    ) {
      // The daemon remains authoritative for registration and access policy.
      return resolveWslRoot([], normalizedExplicit);
    }
    const stickyCandidate =
      allowPreparation &&
      normalizedExplicit === undefined &&
      this.lastResolvedRoot &&
      Date.now() - this.lastResolvedRoot.at < STICKY_ROOT_TTL_MS
        ? [this.lastResolvedRoot.root]
        : [];
    const candidates = [...workspaceCandidates(), ...extraCandidates, ...stickyCandidate];
    const workspaces = await this.registry.list(token);
    if (this.client.usesWslBridge) {
      return resolveWslRoot(workspaces, normalizedExplicit, candidates, relativeTargets);
    }
    return resolveRegisteredRootAfterRefresh(
      workspaces,
      () => this.registry.list(token, true),
      normalizedExplicit,
      candidates,
      allowPreparation ? this.registerMissingRoot : undefined,
      relativeTargets,
    );
  }

  public get usesWslBridge(): boolean { return this.client.usesWslBridge; }

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
    const compactTrace = {
      trace_id: result.trace.trace_id,
      operation: result.trace.operation,
      outcome: result.trace.outcome,
    };
    return new vscode.LanguageModelToolResult([
      new vscode.LanguageModelTextPart(text),
      new vscode.LanguageModelTextPart(`CCTX_TRACE ${JSON.stringify(compactTrace)}`),
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
  invocationMessage: (input) =>
    `Searching indexed source${input.regex ? ' (regex)' : ''} for "${input.pattern ?? input.query ?? ''}"${rootSuffix(input.root)}`,
  arguments: (input, root) => searchArguments(input, root, 1000),
};

const filesDefinition: ToolDefinition<FilesInput> = {
  name: 'code-context-native_codeFiles',
  operation: 'code_files',
  needsRoot: true,
  invocationMessage: (input) =>
    `Finding indexed files matching "${input.glob ?? input.pattern ?? '*'}"${rootSuffix(input.root)}`,
  arguments: (input, root) => {
    const rawGlob = input.glob ?? input.pattern;
    const glob = normalizeGlobPattern(
      typeof rawGlob === 'string' && rawGlob.trim().length > 0 ? rawGlob.trim() : '*',
    );
    const args: Record<string, JsonValue> = { glob };
    const normalizedPaths = normalizeInputPaths(input.paths ?? input.path, root);
    if (normalizedPaths !== undefined) {
      args.paths = normalizedPaths;
    }
    if (typeof input.limit === 'number' && Number.isFinite(input.limit)) {
      args.limit = Math.min(1000, Math.max(1, Math.trunc(input.limit)));
    }
    return args;
  },
};

const contextDefinition: ToolDefinition<ContextInput> = {
  name: 'code-context-native_codeContext',
  operation: 'code_context',
  needsRoot: true,
  invocationMessage: (input) =>
    `Searching indexed source with context${input.regex ? ' (regex)' : ''} for "${input.pattern ?? input.query ?? ''}"${rootSuffix(input.root)}`,
  arguments: (input, root) => {
    const args = searchArguments(input, root, 64);
    if (typeof input.radius === 'number' && Number.isFinite(input.radius)) {
      args.radius = Math.min(100, Math.max(0, Math.trunc(input.radius)));
    }
    return args;
  },
};

const readDefinition: ToolDefinition<ReadInput> = {
  name: 'code-context-native_codeRead',
  operation: 'code_read',
  needsRoot: true,
  invocationMessage: (input) => {
    const count = Array.isArray(input.ranges) ? input.ranges.length : 1;
    return `Reading ${count} indexed source range(s)${rootSuffix(input.root)}`;
  },
  arguments: (input, root) => {
    const rawRanges: ReadRangeInput[] =
      Array.isArray(input.ranges) && input.ranges.length > 0
        ? input.ranges
        : typeof (input.path ?? input.filePath) === 'string'
          ? [input]
          : [];
    const normalized = rawRanges.map((range) => {
      const rawPath = range.path ?? range.filePath ?? '';
      const extracted = extractLineRangeFromPath(rawPath);
      const tupleStart = Array.isArray(range.view_range) ? range.view_range[0] : undefined;
      const tupleEnd = Array.isArray(range.view_range) ? range.view_range[1] : undefined;
      const rawStart =
        range.start ?? range.start_line ?? range.startLine ?? tupleStart ?? extracted.start ?? 1;
      const rawEndCandidate =
        range.end ??
        range.end_line ??
        range.endLine ??
        (tupleEnd !== undefined && tupleEnd >= 0 ? tupleEnd : undefined) ??
        extracted.end ??
        Math.max(rawStart, rawStart + 119);
      const clampedStart = Math.max(1, Math.trunc(rawStart));
      const clampedEnd = Math.max(1, Math.trunc(rawEndCandidate));
      return {
        path: normalizeRelativeTargetPath(extracted.path, root),
        start: Math.min(clampedStart, clampedEnd),
        end: Math.max(clampedStart, clampedEnd),
      };
    });
    return {
      split_ranges: input.split_ranges ?? true,
      page: input.page ?? false,
      ranges: deduplicateAndMergeRanges(normalized),
    };
  },
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

function rootSuffix(root: string | undefined): string {
  if (!root || typeof root !== 'string') {
    return '';
  }
  const clean = root.replaceAll('\\', '/').replace(/\/+$/u, '');
  const name = posix.basename(clean);
  return name ? ` in ${name}` : '';
}

function normalizeGlobPattern(glob: string): string {
  if (/^\*\*\/\*\.([a-zA-Z0-9_+-]+)$/u.test(glob)) {
    return glob.slice(3);
  }
  return glob;
}

function searchArguments(
  input: SearchInput,
  root?: string | null,
  maxLimit = 1000,
): Record<string, JsonValue> {
  const args: Record<string, JsonValue> = { pattern: input.pattern ?? input.query ?? '' };
  if (input.regex !== undefined) {
    args.regex = input.regex;
  }
  if (input.ignore_case !== undefined) {
    args.ignore_case = input.ignore_case;
  }
  if (typeof input.limit === 'number' && Number.isFinite(input.limit)) {
    args.limit = Math.min(maxLimit, Math.max(1, Math.trunc(input.limit)));
  }
  if (typeof input.max_matches_per_file === 'number' && Number.isFinite(input.max_matches_per_file)) {
    args.max_matches_per_file = Math.min(64, Math.max(1, Math.trunc(input.max_matches_per_file)));
  }
  const normalizedPaths = normalizeInputPaths(input.paths ?? input.path, root);
  if (normalizedPaths !== undefined) {
    args.paths = normalizedPaths;
  }
  if (input.glob !== undefined) {
    args.glob = normalizeGlobPattern(input.glob);
  }
  return args;
}

function normalizeUriOrPath(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith('file://')) {
    try {
      const parsed = new URL(trimmed);
      let decoded = decodeURIComponent(parsed.pathname);
      if (/^\/[a-zA-Z]:\//u.test(decoded)) {
        decoded = decoded.slice(1);
      }
      return decoded;
    } catch {
      return trimmed;
    }
  }
  const remoteMatch = /^vscode-remote:\/\/[^/]+(\/.*)$/iu.exec(trimmed);
  if (remoteMatch?.[1]) {
    try {
      return decodeURIComponent(remoteMatch[1]);
    } catch {
      return remoteMatch[1];
    }
  }
  return trimmed;
}

function normalizeExplicitRootInput(
  rawRoot: string | undefined,
  usesWslBridge: boolean,
): string | undefined {
  if (rawRoot === undefined) {
    return undefined;
  }
  const normalized = normalizeUriOrPath(rawRoot);
  if (usesWslBridge && /^\\\\wsl\.localhost\\/iu.test(normalized)) {
    return wslCandidateToLinuxPath(normalized) ?? normalized;
  }
  return normalized;
}

function extractLineRangeFromPath(rawPath: string): {
  path: string;
  start?: number;
  end?: number;
} {
  const normalized = normalizeUriOrPath(rawPath);
  const hashMatch = /^(.+?\.[^#/:]+)#L(\d+)(?:-L?(\d+))?$/u.exec(normalized);
  if (hashMatch?.[1] && hashMatch[2]) {
    const start = Number.parseInt(hashMatch[2], 10);
    const end = hashMatch[3] ? Number.parseInt(hashMatch[3], 10) : start + 40;
    return { path: hashMatch[1], start, end };
  }
  const colonMatch = /^(.+?\.[^/:]+):(\d+)(?:-(\d+))?$/u.exec(normalized);
  if (colonMatch?.[1] && colonMatch[2]) {
    const start = Number.parseInt(colonMatch[2], 10);
    const end = colonMatch[3] ? Number.parseInt(colonMatch[3], 10) : start + 40;
    return { path: colonMatch[1], start, end };
  }
  return { path: normalized };
}

function deduplicateAndMergeRanges(
  ranges: Array<{ path: string; start: number; end: number }>,
): Array<{ path: string; start: number; end: number }> {
  if (ranges.length <= 1) {
    return ranges;
  }
  const merged: Array<{ path: string; start: number; end: number }> = [];
  for (const current of ranges) {
    const previous = merged.at(-1);
    if (
      previous &&
      previous.path === current.path &&
      current.start >= previous.start &&
      current.start <= previous.end + 1 &&
      Math.max(previous.end, current.end) - previous.start < 500
    ) {
      previous.end = Math.max(previous.end, current.end);
    } else if (
      !merged.some(
        (item) =>
          item.path === current.path && item.start <= current.start && item.end >= current.end,
      )
    ) {
      merged.push({ ...current });
    }
  }
  return merged;
}

function normalizeRelativeTargetPath(rawPath: string, root?: string | null): string {
  let normalized = extractLineRangeFromPath(rawPath).path;
  if (!normalized.startsWith('\\\\') && !/^[a-zA-Z]:\\/u.test(normalized)) {
    normalized = normalized.replaceAll('\\', '/');
  }
  if (root) {
    const cleanRoot = root.replaceAll('\\', '/').replace(/\/+$/u, '');
    if (normalized === cleanRoot) {
      return '.';
    }
    if (normalized.startsWith(`${cleanRoot}/`)) {
      normalized = normalized.slice(cleanRoot.length + 1);
    } else {
      const rootName = posix.basename(cleanRoot) || basename(cleanRoot);
      if (rootName && normalized.startsWith(`${rootName}/`)) {
        const stripped = normalized.slice(rootName.length + 1);
        if (stripped && !existsSync(join(root, normalized)) && existsSync(join(root, stripped))) {
          normalized = stripped;
        }
      }
    }
  }
  while (normalized.startsWith('./') && normalized.length > 2) {
    normalized = normalized.slice(2);
  }
  if (normalized.length > 1 && normalized.endsWith('/')) {
    normalized = normalized.replace(/\/+$/u, '');
  }
  return normalized;
}

function normalizeInputPaths(
  rawPaths: string[] | string | undefined,
  root?: string | null,
): string[] | undefined {
  if (rawPaths === undefined) {
    return undefined;
  }
  const items = Array.isArray(rawPaths)
    ? rawPaths
    : typeof rawPaths === 'string' && rawPaths.trim().length > 0
      ? [rawPaths]
      : [];
  return items.map((item) => normalizeRelativeTargetPath(item, root));
}

function collectRawInputPaths(input: unknown): string[] {
  if (!isRecord(input)) {
    return [];
  }
  const raw: string[] = [];
  if (typeof input.path === 'string') {
    raw.push(extractLineRangeFromPath(input.path).path);
  }
  if (typeof input.filePath === 'string') {
    raw.push(extractLineRangeFromPath(input.filePath).path);
  }
  if (typeof input.paths === 'string') {
    raw.push(extractLineRangeFromPath(input.paths).path);
  } else if (Array.isArray(input.paths)) {
    for (const item of input.paths) {
      if (typeof item === 'string') {
        raw.push(extractLineRangeFromPath(item).path);
      }
    }
  }
  if (Array.isArray(input.ranges)) {
    for (const range of input.ranges) {
      if (isRecord(range)) {
        const rangePath = typeof range.path === 'string' ? range.path : typeof range.filePath === 'string' ? range.filePath : undefined;
        if (rangePath) {
          raw.push(extractLineRangeFromPath(rangePath).path);
        }
      }
    }
  }
  return raw;
}

function extractInputPathCandidates(input: unknown): string[] {
  return collectRawInputPaths(input).filter(
    (item) =>
      (item.startsWith('/') && !item.startsWith('//') && item.indexOf('/', 1) > 1) ||
      /^\\\\(?:wsl\.localhost|wsl\$)\\/iu.test(item) ||
      /^[a-zA-Z]:[\\/]/u.test(item),
  );
}

function extractRelativePathTargets(input: unknown): string[] {
  return collectRawInputPaths(input).filter(
    (item) =>
      item.length > 0 &&
      !item.startsWith('/') &&
      !item.startsWith('\\') &&
      !/^[a-zA-Z]:/u.test(item),
  );
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

function pushUriCandidate(candidates: string[], seen: Set<string>, uri: vscode.Uri | undefined): void {
  if (!uri || (uri.scheme !== 'file' && uri.scheme !== 'vscode-remote')) {
    return;
  }
  if (uri.scheme === 'vscode-remote' && typeof uri.path === 'string' && uri.path.startsWith('/')) {
    if (!seen.has(uri.path)) {
      seen.add(uri.path);
      candidates.push(uri.path);
    }
  }
  if (typeof uri.fsPath === 'string' && uri.fsPath.length > 0 && !seen.has(uri.fsPath)) {
    seen.add(uri.fsPath);
    candidates.push(uri.fsPath);
  }
}

function extractTabUri(tab: unknown): vscode.Uri | undefined {
  if (!isRecord(tab) || !isRecord(tab.input)) {
    return undefined;
  }
  const input = tab.input;
  if (isRecord(input.uri) && typeof input.uri.scheme === 'string') {
    return input.uri as unknown as vscode.Uri;
  }
  if (isRecord(input.modified) && typeof input.modified.scheme === 'string') {
    return input.modified as unknown as vscode.Uri;
  }
  return undefined;
}

function workspaceCandidates(): string[] {
  const candidates: string[] = [];
  const seen = new Set<string>();
  pushUriCandidate(candidates, seen, vscode.window.activeTextEditor?.document.uri);
  for (const editor of vscode.window.visibleTextEditors ?? []) {
    pushUriCandidate(candidates, seen, editor.document?.uri);
  }
  const tabGroups: vscode.TabGroups | undefined = vscode.window.tabGroups;
  if (tabGroups?.activeTabGroup?.activeTab) {
    pushUriCandidate(candidates, seen, extractTabUri(tabGroups.activeTabGroup.activeTab));
  }
  for (const group of tabGroups?.all ?? []) {
    if (group.activeTab) {
      pushUriCandidate(candidates, seen, extractTabUri(group.activeTab));
    }
  }
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    pushUriCandidate(candidates, seen, folder.uri);
  }
  for (const doc of vscode.workspace.textDocuments ?? []) {
    pushUriCandidate(candidates, seen, doc.uri);
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
