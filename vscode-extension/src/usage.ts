import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

export type Outcome = 'success' | 'cache_not_ready' | 'policy_denied' | 'invalid_request' | 'system_error';

export interface UsageEvent {
  operation: string;
  root: string | null;
  error?: string;
  totalMs?: number;
  deliveredChars?: number;
  turnCompressed?: boolean;
  model?: string;
  startedAt?: number;
  isCommand?: boolean;
}

export interface OperationMetrics {
  calls: number;
  successes: number;
  cacheMisses: number;
  deliveredTokens: number;
  savedTokens: number;
  totalMs: number;
  timedCalls: number;
}

export interface SessionModelSummary {
  model: string;
  displayName: string;
  effort: string;
  sessions: number;
  turns: number;
  inputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  usedAic: number;
  cctxCalls: number;
  cctxWarmHitPct?: number | undefined;
}

export interface SessionTelemetrySnapshot {
  updatedAt: number;
  sessions: number;
  turns: number;
  inputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  usedAic: number;
  models: SessionModelSummary[];
}

export interface BackendModelUsageEntry {
  model: string;
  displayName: string;
  effort: string;
  sessions: number;
  turns: number;
  inputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  usedAic: number;
  savedAic: number;
  savedSecs: number;
  tokensSaved: number;
  cctxCalls: number;
  lookupCalls: number;
  adoptionPercent: number | undefined;
}

export interface BackendToolUsageEntry {
  operation: string;
  calls: number;
  successes: number;
  cacheMisses: number;
  policyDenied: number;
  errors: number;
  avgPayloadTokens: number;
  medianMs: number | undefined;
  p95Ms: number | undefined;
}

export interface BackendUsageSnapshot {
  updatedAt: number;
  computedAtMs: number;
  hours: number;
  sessions: number;
  turns: number;
  inputTokens: number;
  uncachedInputTokens: number;
  cacheWriteTokens: number;
  cacheReadTokens: number;
  promptCacheHitPercent: number | undefined;
  outputTokens: number;
  reasoningTokens: number;
  usedAic: number;
  savedAic: number;
  timeSavedSecs: number;
  turnTimeSavedSecs: number;
  ioTimeSavedSecs: number;
  prefillTimeSavedSecs: number;
  effectiveAvoidedTurns: number;
  tokensSaved: number;
  directTokensSaved: number;
  compoundedTokensSaved: number;
  turnTokensSaved: number;
  turnCompressionCalls: number;
  bpeTokensSaved: number;
  selectionPercent: number | undefined;
  warmHitPercent: number | undefined;
  costSavingsPercent: number | undefined;
  avoidedPremiumRequests: number;
  cctxCalls: number;
  cctxSuccesses: number;
  cctxCacheMisses: number;
  cctxPolicyDenied: number;
  cctxErrors: number;
  sourceMedianMs: number | undefined;
  daemonState: string | undefined;
  daemonLoadedRoots: number | undefined;
  daemonRegisteredRoots: number | undefined;
  daemonEstimatedBytes: number | undefined;
  daemonCacheHits: number | undefined;
  daemonCacheMisses: number | undefined;
  daemonRecentSourceAvgMs: number | undefined;
  rawDaemon: Record<string, unknown> | undefined;
  models: BackendModelUsageEntry[];
  tools: BackendToolUsageEntry[];
  workspaces: Array<{ workspace: string; cctx: number; builtin: number; adoptionPercent: number }>;
  advisories: Array<{ code: string; message: string }>;
}

function asFiniteNumber(v: unknown, fallback = 0): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : fallback;
}

function asOptionalFiniteNumber(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

function asObj(v: unknown): Record<string, unknown> | undefined {
  return typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
}

function canonicalCctxOperation(toolName: string): string | undefined {
  const lower = toolName.trim().toLowerCase();
  if (lower.endsWith('code_context') || lower.endsWith('cctxnativecontext') || lower.endsWith('_codecontext')) {
    return 'code_context';
  }
  if (lower.endsWith('code_read') || lower.endsWith('cctxnativeread') || lower.endsWith('_coderead')) {
    return 'code_read';
  }
  if (lower.endsWith('code_grep') || lower.endsWith('cctxnativegrep') || lower.endsWith('_codegrep')) {
    return 'code_grep';
  }
  if (lower.endsWith('code_files') || lower.endsWith('cctxnativefiles') || lower.endsWith('_codefiles')) {
    return 'code_files';
  }
  if (lower.endsWith('code_workspaces') || lower.endsWith('cctxnativeworkspaces') || lower.endsWith('_codeworkspaces')) {
    return 'code_workspaces';
  }
  if (lower.endsWith('code_status') || lower.endsWith('cctxnativestatus') || lower.endsWith('_codestatus')) {
    return 'code_status';
  }
  if (lower.endsWith('code_sync') || lower.endsWith('cctxnativesync') || lower.endsWith('_codesync')) {
    return 'code_sync';
  }
  return undefined;
}

export function parseBackendUsagePayload(raw: unknown, updatedAt = Date.now()): BackendUsageSnapshot | undefined {
  const root = asObj(raw);
  const ts = asObj(root?.token_summary);
  if (!root || !ts) { return undefined; }

  const models: BackendModelUsageEntry[] = [];
  if (Array.isArray(root.model_usage)) {
    for (const item of root.model_usage) {
      const m = asObj(item);
      if (!m) { continue; }
      const mTurns = asFiniteNumber(m.turns);
      const mCctx = asFiniteNumber(m.cctx_calls);
      const mTokSaved = asFiniteNumber(m.estimated_total_tokens_saved);
      const explicitModelSecs = asOptionalFiniteNumber(m.saved_secs);
      const fallbackModelSecs =
        explicitModelSecs ??
        (mCctx > 0 || mTokSaved > 0
          ? Math.min(mCctx * 0.35, Math.max(1, mTurns * 0.5)) * 5.0 + mCctx * 0.05 + mTokSaved / 250_000
          : 0);
      models.push({
        model: typeof m.model === 'string' ? m.model : 'unknown',
        displayName: typeof m.display_name === 'string' && m.display_name ? m.display_name : 'Unknown Model',
        effort: typeof m.effort === 'string' && m.effort ? m.effort : '-',
        sessions: asFiniteNumber(m.sessions),
        turns: mTurns,
        inputTokens: asFiniteNumber(m.input_tokens),
        cacheReadTokens: asFiniteNumber(m.cache_read_tokens),
        cacheWriteTokens: asFiniteNumber(m.cache_write_tokens),
        outputTokens: asFiniteNumber(m.output_tokens),
        reasoningTokens: asFiniteNumber(m.reasoning_tokens),
        usedAic: asFiniteNumber(m.used_aic),
        savedAic: asFiniteNumber(m.saved_aic),
        savedSecs: fallbackModelSecs,
        tokensSaved: mTokSaved,
        cctxCalls: mCctx,
        lookupCalls: asFiniteNumber(m.lookup_calls),
        adoptionPercent: asOptionalFiniteNumber(m.cctx_adoption_percent),
      });
    }
  }

  const insights = asObj(root.insights);
  const tools: BackendToolUsageEntry[] = [];
  let cctxCalls = 0;
  let cctxSuccesses = 0;
  let cctxCacheMisses = 0;
  let cctxPolicyDenied = 0;
  let cctxErrors = 0;
  let exactSourceMedianMs = asOptionalFiniteNumber(insights?.cctx_source_median_ms);
  let sourceMedWeightedSum = 0;
  let sourceMedWeightedCalls = 0;

  if (Array.isArray(insights?.cctx_operations) && insights.cctx_operations.length > 0) {
    for (const item of insights.cctx_operations) {
      const opObj = asObj(item);
      if (!opObj || typeof opObj.operation !== 'string') { continue; }
      const calls = asFiniteNumber(opObj.calls);
      if (calls <= 0) { continue; }
      const failures = Math.min(calls, asFiniteNumber(opObj.failures));
      const causes = asObj(opObj.failure_causes);
      const cacheMisses = Math.min(
        failures,
        asFiniteNumber(causes?.cache_miss) + asFiniteNumber(causes?.stale_index),
      );
      const policyDenied = Math.min(failures - cacheMisses, asFiniteNumber(causes?.policy_denied));
      const successes = Math.max(0, calls - failures);
      const errors = Math.max(0, failures - cacheMisses - policyDenied);
      const medianMs = asOptionalFiniteNumber(opObj.median_ms);
      const p95Ms = asOptionalFiniteNumber(opObj.p95_ms);
      tools.push({
        operation: opObj.operation,
        calls,
        successes,
        cacheMisses,
        policyDenied,
        errors,
        avgPayloadTokens: Math.round(asFiniteNumber(opObj.avg_payload_tokens)),
        medianMs,
        p95Ms,
      });
      cctxCalls += calls;
      cctxSuccesses += successes;
      cctxCacheMisses += cacheMisses;
      cctxPolicyDenied += policyDenied;
      cctxErrors += errors;
      if (
        exactSourceMedianMs === undefined &&
        medianMs !== undefined &&
        (opObj.operation === 'code_context' || opObj.operation === 'code_read' || opObj.operation === 'code_grep' || opObj.operation === 'code_files')
      ) {
        sourceMedWeightedSum += medianMs * calls;
        sourceMedWeightedCalls += calls;
      }
    }
    tools.sort((a, b) => b.calls - a.calls);
  } else {
    const toolMap = new Map<string, {
      calls: number;
      failures: number;
      cacheMisses: number;
      policyDenied: number;
      payloadTokensTotal: number;
      weightedMedianSum: number;
      weightedMedianCalls: number;
      maxP95: number | undefined;
    }>();
    if (Array.isArray(insights?.tool_reliability)) {
      for (const item of insights.tool_reliability) {
        const tr = asObj(item);
        if (!tr || typeof tr.tool !== 'string') { continue; }
        const op = canonicalCctxOperation(tr.tool);
        if (!op) { continue; }
        const calls = asFiniteNumber(tr.calls);
        if (calls <= 0) { continue; }
        const failures = Math.min(calls, asFiniteNumber(tr.failures));
        const causes = asObj(tr.failure_causes);
        const cacheMisses = Math.min(
          failures,
          asFiniteNumber(causes?.cache_miss) + asFiniteNumber(causes?.stale_index),
        );
        const policyDenied = Math.min(failures - cacheMisses, asFiniteNumber(causes?.policy_denied));
        const avgPayload = asFiniteNumber(tr.avg_payload_tokens);
        const med = asOptionalFiniteNumber(tr.trace_median_ms) ?? asOptionalFiniteNumber(tr.median_ms);
        const p95 = asOptionalFiniteNumber(tr.trace_p95_ms) ?? asOptionalFiniteNumber(tr.p95_ms);
        const bucket = toolMap.get(op) ?? {
          calls: 0,
          failures: 0,
          cacheMisses: 0,
          policyDenied: 0,
          payloadTokensTotal: 0,
          weightedMedianSum: 0,
          weightedMedianCalls: 0,
          maxP95: undefined,
        };
        bucket.calls += calls;
        bucket.failures += failures;
        bucket.cacheMisses += cacheMisses;
        bucket.policyDenied += policyDenied;
        bucket.payloadTokensTotal += avgPayload * calls;
        if (med !== undefined && med >= 0) {
          bucket.weightedMedianSum += med * calls;
          bucket.weightedMedianCalls += calls;
        }
        if (p95 !== undefined && p95 >= 0) {
          bucket.maxP95 = bucket.maxP95 === undefined ? p95 : Math.max(bucket.maxP95, p95);
        }
        toolMap.set(op, bucket);
      }
    }
    for (const [operation, b] of toolMap.entries()) {
      const successes = Math.max(0, b.calls - b.failures);
      const errors = Math.max(0, b.failures - b.cacheMisses - b.policyDenied);
      const medianMs = b.weightedMedianCalls > 0 ? b.weightedMedianSum / b.weightedMedianCalls : undefined;
      tools.push({
        operation,
        calls: b.calls,
        successes,
        cacheMisses: b.cacheMisses,
        policyDenied: b.policyDenied,
        errors,
        avgPayloadTokens: b.calls > 0 ? Math.round(b.payloadTokensTotal / b.calls) : 0,
        medianMs,
        p95Ms: b.maxP95,
      });
      cctxCalls += b.calls;
      cctxSuccesses += successes;
      cctxCacheMisses += b.cacheMisses;
      cctxPolicyDenied += b.policyDenied;
      cctxErrors += errors;
      if (
        medianMs !== undefined &&
        (operation === 'code_context' || operation === 'code_read' || operation === 'code_grep' || operation === 'code_files')
      ) {
        sourceMedWeightedSum += medianMs * b.calls;
        sourceMedWeightedCalls += b.calls;
      }
    }
    tools.sort((a, b) => b.calls - a.calls);
  }
  if (exactSourceMedianMs === undefined && sourceMedWeightedCalls > 0) {
    exactSourceMedianMs = sourceMedWeightedSum / sourceMedWeightedCalls;
  }

  const daemon = asObj(root.daemon);
  const daemonCache = asObj(daemon?.cache);
  const daemonAdm = asObj(daemon?.daemon_admission);
  const regAdm = asObj(daemon?.registry_admission);

  const turns = asFiniteNumber(ts.total_turns);
  const directTokensSaved = asFiniteNumber(ts.direct_tokens_saved);
  const compoundedTokensSaved = asFiniteNumber(ts.compounded_history_tokens_saved);
  const turnTokensSaved = asFiniteNumber(ts.estimated_turn_tokens_saved);
  const turnCompressionCalls = asFiniteNumber(ts.turn_compression_calls);
  const bpeTokensSaved = asFiniteNumber(ts.bpe_formula_tokens_saved);
  const effectiveAvoidedTurns =
    asOptionalFiniteNumber(ts.effective_avoided_turns) ??
    Math.min(turnCompressionCalls, Math.max(1, turns * 0.6));
  const turnTimeSavedSecs =
    asOptionalFiniteNumber(ts.avoided_turn_time_saved_secs) ??
    effectiveAvoidedTurns * 5.0;
  const ioTimeSavedSecs =
    asOptionalFiniteNumber(ts.lookup_io_time_saved_secs) ??
    cctxSuccesses * 0.05;
  const prefillTimeSavedSecs =
    asOptionalFiniteNumber(ts.prefill_time_saved_secs) ??
    (directTokensSaved + compoundedTokensSaved + bpeTokensSaved) / 200_000;
  const timeSavedSecs =
    asOptionalFiniteNumber(ts.estimated_time_saved_secs) ??
    (turnTimeSavedSecs + ioTimeSavedSecs + prefillTimeSavedSecs);

  const workspaces: Array<{ workspace: string; cctx: number; builtin: number; adoptionPercent: number }> = [];
  if (Array.isArray(insights?.workspace_adoption)) {
    for (const item of insights.workspace_adoption) {
      const ws = asObj(item);
      if (!ws || typeof ws.workspace !== 'string') { continue; }
      workspaces.push({
        workspace: ws.workspace,
        cctx: asFiniteNumber(ws.cctx),
        builtin: asFiniteNumber(ws.builtin),
        adoptionPercent: asFiniteNumber(ws.adoption_percent),
      });
    }
  }
  const advisories: Array<{ code: string; message: string }> = [];
  if (Array.isArray(insights?.advisories)) {
    for (const item of insights.advisories) {
      const adv = asObj(item);
      if (!adv || typeof adv.message !== 'string') { continue; }
      advisories.push({
        code: typeof adv.code === 'string' ? adv.code : 'info',
        message: adv.message,
      });
    }
  }

  return {
    updatedAt,
    computedAtMs: asOptionalFiniteNumber(root.computed_at_unix_ms) ?? updatedAt,
    hours: asFiniteNumber(root.hours, 24),
    sessions: asFiniteNumber(root.sessions),
    turns,
    inputTokens: asFiniteNumber(ts.input_tokens),
    uncachedInputTokens: asFiniteNumber(ts.uncached_input_tokens),
    cacheWriteTokens: asFiniteNumber(ts.cache_write_tokens),
    cacheReadTokens: asFiniteNumber(ts.cache_read_tokens),
    promptCacheHitPercent: asOptionalFiniteNumber(ts.prompt_cache_hit_percent),
    outputTokens: asFiniteNumber(ts.output_tokens),
    reasoningTokens: asFiniteNumber(ts.reasoning_tokens),
    usedAic: asFiniteNumber(ts.used_aic),
    savedAic: asFiniteNumber(ts.saved_aic),
    timeSavedSecs,
    turnTimeSavedSecs,
    ioTimeSavedSecs,
    prefillTimeSavedSecs,
    effectiveAvoidedTurns,
    tokensSaved: asFiniteNumber(ts.estimated_total_tokens_saved),
    directTokensSaved,
    compoundedTokensSaved,
    turnTokensSaved,
    turnCompressionCalls,
    bpeTokensSaved,
    selectionPercent: asOptionalFiniteNumber(ts.cctx_selection_percent),
    warmHitPercent: asOptionalFiniteNumber(ts.cctx_warm_cache_hit_percent),
    costSavingsPercent: asOptionalFiniteNumber(ts.cost_savings_percent),
    avoidedPremiumRequests: asFiniteNumber(ts.avoided_premium_requests_est),
    cctxCalls,
    cctxSuccesses,
    cctxCacheMisses,
    cctxPolicyDenied,
    cctxErrors,
    sourceMedianMs: exactSourceMedianMs,
    daemonState: typeof daemon?.index_state === 'string' ? daemon.index_state : undefined,
    daemonLoadedRoots: asOptionalFiniteNumber(daemonAdm?.loaded_roots),
    daemonRegisteredRoots: asOptionalFiniteNumber(regAdm?.registered_roots),
    daemonEstimatedBytes: asOptionalFiniteNumber(daemonAdm?.estimated_bytes),
    daemonCacheHits: asOptionalFiniteNumber(daemonCache?.hits),
    daemonCacheMisses: asOptionalFiniteNumber(daemonCache?.misses),
    daemonRecentSourceAvgMs: asOptionalFiniteNumber(daemon?.recent_source_avg_ms),
    rawDaemon: daemon,
    models,
    tools,
    workspaces,
    advisories,
  };
}

export function formatCompactTokens(value: number): string {
  if (!Number.isFinite(value) || value <= 0) { return '0'; }
  if (value >= 1_000_000_000) { return `${(value / 1_000_000_000).toFixed(2)}B`; }
  if (value >= 1_000_000) { return `${(value / 1_000_000).toFixed(2)}M`; }
  if (value >= 1_000) { return `${(value / 1_000).toFixed(1)}K`; }
  return `${Math.round(value)}`;
}

export function formatShortTokens(value: number): string {
  if (!Number.isFinite(value) || value <= 0) { return '0'; }
  if (value >= 999_950_000) { return `${(value / 1_000_000_000).toFixed(1)}B`; }
  if (value >= 99_950_000) { return `${Math.round(value / 1_000_000)}M`; }
  if (value >= 999_950) { return `${(value / 1_000_000).toFixed(1)}M`; }
  if (value >= 99_950) { return `${Math.round(value / 1_000)}K`; }
  if (value >= 999.5) { return `${(value / 1_000).toFixed(1)}K`; }
  return `${Math.round(value)}`;
}

export function formatCompactDuration(seconds: number, short = false): string {
  if (!Number.isFinite(seconds) || seconds <= 0) { return '0s'; }
  if (seconds < 0.95) { return `${seconds.toFixed(1)}s`; }
  const total = Math.round(seconds);
  if (total < 60) { return `${total}s`; }
  if (total >= 3600) {
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
  }
  if (short || total >= 900 || total % 60 === 0) {
    return `${Math.max(1, Math.round(seconds / 60))}m`;
  }
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}m ${s}s`;
}

function formatSmartMs(ms: number | undefined): string {
  if (ms === undefined || !Number.isFinite(ms) || ms < 0) { return '-'; }
  if (ms < 9.95) { return `${ms.toFixed(1)}ms`; }
  if (ms < 999.5) { return `${Math.round(ms)}ms`; }
  return `${(ms / 1000).toFixed(1)}s`;
}

function escapeMdCell(value: string): string {
  return value.replace(/[\r\n]+/g, ' ').replace(/\|/g, '\\|').trim();
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function formatLocalUpdated(updatedAtMs: number, nowMs = Date.now()): string {
  const d = new Date(updatedAtMs);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ageSec = Math.max(0, Math.round((nowMs - updatedAtMs) / 1000));
  if (ageSec < 45) { return `just now (${hh}:${mm})`; }
  if (ageSec < 3600) { return `${Math.max(1, Math.round(ageSec / 60))}m ago (${hh}:${mm})`; }
  return `${Math.round(ageSec / 3600)}h ago (${hh}:${mm})`;
}

function formatToolCallBadge(t: BackendToolUsageEntry): string {
  const countStr = t.calls.toLocaleString('en-US');
  const errTotal = t.policyDenied + t.errors;
  if (t.cacheMisses === 0 && errTotal === 0) {
    return `${countStr} (\`100%\`)`;
  }
  const notes: string[] = [];
  if (t.cacheMisses > 0) { notes.push(`miss ${t.cacheMisses}`); }
  if (errTotal > 0) { notes.push(`err ${errTotal}`); }
  return `${countStr} (\`${notes.join(', ')}\`)`;
}

export function classifyOutcome(error?: string): Outcome {
  if (error === undefined) { return 'success'; }
  if (
    error.startsWith('CACHE_MISS ') ||
    error.startsWith('STALE_INDEX ') ||
    /CACHE_MISS |STALE_INDEX|reason=not_yet_indexed/u.test(error)
  ) {
    return 'cache_not_ready';
  }
  if (
    error.startsWith('READ_POLICY_DENIED ') ||
    /READ_POLICY_DENIED|reason=(outside_scope|excluded_by_rule|unsupported_glob|ignored_by_discovery|symlink_not_allowed)|only non-hidden/u.test(error)
  ) {
    return 'policy_denied';
  }
  if (
    error.startsWith('READ_RANGE_OUT_OF_BOUNDS ') ||
    /READ_RANGE_OUT_OF_BOUNDS|READ_RANGE_LIMIT|READ_PLAN_TOO_LARGE|READ_PAGE_LIMIT|RESPONSE_BUDGET_EXCEEDED|reason=missing_on_disk|regex parse error|limit 1\.\.(64|1000)|provide 1\.\.64 ranges|include\/exclude globs|invalid range|unknown field|Windows Agent Sessions require|WSL Code Context (?:root|path)|No registered Code Context workspace|root must be an absolute path/u.test(error)
  ) {
    return 'invalid_request';
  }
  return 'system_error';
}

function isCctxToolName(toolName: string): boolean {
  return (
    toolName.startsWith('cctxNative') ||
    toolName.startsWith('code-context') ||
    toolName.startsWith('mcp__code-context__') ||
    toolName === 'code_files' ||
    toolName === 'code_grep' ||
    toolName === 'code_context' ||
    toolName === 'code_read' ||
    toolName === 'code_status' ||
    toolName === 'code_sync' ||
    toolName === 'code_workspaces'
  );
}

function parseSessionTelemetryChunks(
  chunks: ReadonlyArray<{ sessionId: string; raw: string; isTailSliced?: boolean }>,
  nowMs: number,
): SessionTelemetrySnapshot {
  interface ModelAccum {
    model: string; displayName: string; effort: string; sessions: Set<string>;
    turns: number; inputTokens: number; cacheReadTokens: number; cacheWriteTokens: number;
    outputTokens: number; reasoningTokens: number; nanoAiu: number; cctxCalls: number; cctxMisses: number;
  }
  const byModel = new Map<string, ModelAccum>();
  let totalTurns = 0;
  let totalIn = 0;
  let totalCache = 0;
  let totalCacheWrite = 0;
  let totalOut = 0;
  let totalRsn = 0;
  let totalNanoAiu = 0;
  const activeSessions = new Set<string>();
  const ensureModel = (model: string, displayName: string, effort: string): ModelAccum => {
    let entry = byModel.get(model);
    if (!entry) {
      entry = {
        model, displayName, effort, sessions: new Set(),
        turns: 0, inputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0,
        outputTokens: 0, reasoningTokens: 0, nanoAiu: 0, cctxCalls: 0, cctxMisses: 0,
      };
      byModel.set(model, entry);
    }
    return entry;
  };
  for (const { sessionId, raw, isTailSliced } of chunks) {
    let lastModel = 'unknown';
    let codexEffort = 'codex';
    let prevCodexTotal: { in: number; cr: number; cw: number; out: number; rsn: number } | undefined;
    const lastUsageSigByAgent = new Map<string, { type: string; tsMs: number | undefined; model: string; inTok: number; outTok: number }>();
    const cctxCallModels = new Map<string, string>();
    let lineStart = 0;
    while (lineStart < raw.length) {
      const nlIdx = raw.indexOf('\n', lineStart);
      const lineEnd = nlIdx >= 0 ? nlIdx : raw.length;
      const line = raw.slice(lineStart, lineEnd);
      lineStart = lineEnd + 1;
      if (
        !line.includes('"session.usage_record"') &&
        !line.includes('"assistant.usage"') &&
        !line.includes('"tool.execution_') &&
        !line.includes('"cache_read_input_tokens"') &&
        !line.includes('"token_count"') &&
        !line.includes('"turn_context"') &&
        !line.includes('"session_meta"') &&
        !line.includes('"tool_use"') &&
        !line.includes('"function_call"')
      ) {
        continue;
      }
      try {
        const ev = JSON.parse(line) as Record<string, unknown>;
        const type = ev.type;
        const data = typeof ev.data === 'object' && ev.data !== null ? ev.data as Record<string, unknown> : undefined;
        if ((type === 'session.usage_record' || type === 'assistant.usage') && data) {
          const u = type === 'session.usage_record'
            ? (typeof data.usage === 'object' && data.usage !== null ? data.usage as Record<string, unknown> : undefined)
            : (typeof data.usage === 'object' && data.usage !== null ? data.usage as Record<string, unknown> : data);
          if (!u) { continue; }
          const model = typeof u.model === 'string' && u.model ? u.model : 'unknown';
          const displayName = typeof u.modelDisplayName === 'string' && u.modelDisplayName ? u.modelDisplayName : model;
          const effort = typeof u.reasoningEffort === 'string' && u.reasoningEffort ? u.reasoningEffort : '-';
          lastModel = model;
          activeSessions.add(sessionId);
          const inTok = typeof u.inputTokens === 'number' ? u.inputTokens : 0;
          const crTok = typeof u.cacheReadTokens === 'number' ? u.cacheReadTokens : 0;
          const cwTok = typeof u.cacheWriteTokens === 'number' ? u.cacheWriteTokens : 0;
          const outTok = typeof u.outputTokens === 'number' ? u.outputTokens : 0;
          const rsnTok = typeof u.reasoningTokens === 'number' ? u.reasoningTokens : 0;
          const cu = typeof u.copilotUsage === 'object' && u.copilotUsage !== null ? u.copilotUsage as Record<string, unknown> : undefined;
          const nanoAiu = typeof cu?.totalNanoAiu === 'number' ? cu.totalNanoAiu : 0;
          const agentId = typeof ev.agentId === 'string' && ev.agentId ? ev.agentId : 'main';
          const rawTs = ev.timestamp;
          const tsMs = typeof rawTs === 'number'
            ? (rawTs > 10_000_000_000 ? rawTs : rawTs * 1000)
            : (typeof rawTs === 'string' ? Date.parse(rawTs) : Number.NaN);
          const validTsMs = Number.isFinite(tsMs) ? tsMs : undefined;
          const prevSig = lastUsageSigByAgent.get(agentId);
          const isDupPair = Boolean(
            prevSig &&
            prevSig.type !== type &&
            prevSig.model === model &&
            prevSig.inTok === inTok &&
            prevSig.outTok === outTok &&
            validTsMs !== undefined &&
            prevSig.tsMs !== undefined &&
            Math.abs(validTsMs - prevSig.tsMs) <= 2_000,
          );
          if (!isDupPair) {
            lastUsageSigByAgent.set(agentId, { type, tsMs: validTsMs, model, inTok, outTok });
            totalTurns += 1;
            totalIn += inTok;
            totalCache += crTok;
            totalCacheWrite += cwTok;
            totalOut += outTok;
            totalRsn += rsnTok;
            totalNanoAiu += nanoAiu;
            const entry = ensureModel(model, displayName, effort);
            entry.sessions.add(sessionId);
            entry.turns += 1;
            entry.inputTokens += inTok;
            entry.cacheReadTokens += crTok;
            entry.cacheWriteTokens += cwTok;
            entry.outputTokens += outTok;
            entry.reasoningTokens += rsnTok;
            entry.nanoAiu += nanoAiu;
          }
        } else if (type === 'assistant' && typeof ev.message === 'object' && ev.message !== null) {
          const msg = ev.message as Record<string, unknown>;
          const model = typeof msg.model === 'string' && msg.model ? msg.model : (lastModel !== 'unknown' ? lastModel : 'claude-opus');
          lastModel = model;
          if (typeof msg.usage === 'object' && msg.usage !== null) {
            const u = msg.usage as Record<string, unknown>;
            const uncached = typeof u.input_tokens === 'number' ? u.input_tokens : 0;
            const cwTok = typeof u.cache_creation_input_tokens === 'number' ? u.cache_creation_input_tokens : 0;
            const crTok = typeof u.cache_read_input_tokens === 'number' ? u.cache_read_input_tokens : 0;
            const ccObj = typeof u.cache_creation === 'object' && u.cache_creation !== null ? u.cache_creation as Record<string, unknown> : undefined;
            const cw1h = typeof ccObj?.ephemeral_1h_input_tokens === 'number' ? ccObj.ephemeral_1h_input_tokens : 0;
            const outTok = typeof u.output_tokens === 'number' ? u.output_tokens : 0;
            const inTok = uncached + cwTok + crTok;
            const baseIn = model.includes('opus') ? 500e-6 : (model.includes('haiku') ? 100e-6 : 300e-6);
            const cwRate = cw1h > 0 && cwTok > 0 ? ((cw1h * baseIn * 2.0 + Math.max(0, cwTok - cw1h) * baseIn * 1.25) / cwTok) : baseIn * 1.25;
            const estAic = uncached * baseIn + cwTok * cwRate + crTok * (baseIn * 0.1) + outTok * (baseIn * 5.0);
            const nanoAiu = Math.round(estAic * 1e9);
            activeSessions.add(sessionId);
            totalTurns += 1;
            totalIn += inTok;
            totalCache += crTok;
            totalCacheWrite += cwTok;
            totalOut += outTok;
            totalNanoAiu += nanoAiu;
            const entry = ensureModel(model, model, 'claude-agent');
            entry.sessions.add(sessionId);
            entry.turns += 1;
            entry.inputTokens += inTok;
            entry.cacheReadTokens += crTok;
            entry.cacheWriteTokens += cwTok;
            entry.outputTokens += outTok;
            entry.nanoAiu += nanoAiu;
          }
          if (Array.isArray(msg.content)) {
            for (const b of msg.content) {
              if (typeof b === 'object' && b !== null) {
                const blk = b as Record<string, unknown>;
                if (blk.type === 'tool_use' && typeof blk.name === 'string' && isCctxToolName(blk.name)) {
                  const entry = ensureModel(model, model, 'claude-agent');
                  entry.cctxCalls += 1;
                }
              }
            }
          }
        } else if (type === 'session_meta' && typeof ev.payload === 'object' && ev.payload !== null) {
          const p = ev.payload as Record<string, unknown>;
          if (typeof p.model === 'string' && p.model) { lastModel = p.model; }
        } else if (type === 'turn_context' && typeof ev.payload === 'object' && ev.payload !== null) {
          const p = ev.payload as Record<string, unknown>;
          if (typeof p.model === 'string' && p.model) { lastModel = p.model; }
          if (typeof p.effort === 'string' && p.effort) { codexEffort = p.effort; }
          else if (typeof p.reasoning_effort === 'string' && p.reasoning_effort) { codexEffort = p.reasoning_effort; }
        } else if (type === 'event_msg' && typeof ev.payload === 'object' && ev.payload !== null) {
          const p = ev.payload as Record<string, unknown>;
          if (p.type === 'token_count' && typeof p.info === 'object' && p.info !== null) {
            const info = p.info as Record<string, unknown>;
            const tot = typeof info.total_token_usage === 'object' && info.total_token_usage !== null ? info.total_token_usage as Record<string, unknown> : undefined;
            const step = typeof info.last_token_usage === 'object' && info.last_token_usage !== null ? info.last_token_usage as Record<string, unknown> : undefined;
            const curTot = tot ? {
              in: Math.max(0, typeof tot.input_tokens === 'number' ? tot.input_tokens : 0),
              cr: Math.max(0, typeof tot.cached_input_tokens === 'number' ? tot.cached_input_tokens : 0),
              cw: Math.max(0, typeof tot.cache_write_input_tokens === 'number' ? tot.cache_write_input_tokens : 0),
              out: Math.max(0, typeof tot.output_tokens === 'number' ? tot.output_tokens : 0),
              rsn: Math.max(0, typeof tot.reasoning_output_tokens === 'number' ? tot.reasoning_output_tokens : 0),
            } : undefined;
            if (curTot && prevCodexTotal && curTot.in === prevCodexTotal.in && curTot.out === prevCodexTotal.out) {
              continue;
            }
            let rawIn = 0, crTok = 0, cwTok = 0, outTok = 0, rsnTok = 0;
            if (curTot && prevCodexTotal && curTot.in >= prevCodexTotal.in && curTot.out >= prevCodexTotal.out) {
              rawIn = curTot.in - prevCodexTotal.in;
              crTok = Math.max(0, curTot.cr - prevCodexTotal.cr);
              cwTok = Math.max(0, curTot.cw - prevCodexTotal.cw);
              outTok = curTot.out - prevCodexTotal.out;
              rsnTok = Math.max(0, curTot.rsn - prevCodexTotal.rsn);
              prevCodexTotal = curTot;
            } else if (step) {
              rawIn = Math.max(0, typeof step.input_tokens === 'number' ? step.input_tokens : 0);
              crTok = Math.max(0, typeof step.cached_input_tokens === 'number' ? step.cached_input_tokens : 0);
              cwTok = Math.max(0, typeof step.cache_write_input_tokens === 'number' ? step.cache_write_input_tokens : 0);
              outTok = Math.max(0, typeof step.output_tokens === 'number' ? step.output_tokens : 0);
              rsnTok = Math.max(0, typeof step.reasoning_output_tokens === 'number' ? step.reasoning_output_tokens : 0);
              if (curTot) { prevCodexTotal = curTot; }
            } else if (curTot) {
              prevCodexTotal = curTot;
              if (isTailSliced) {
                // On a 128KiB tail slice without `last_token_usage`, establish cumulative baseline without inflating pre-slice history.
                continue;
              }
              rawIn = curTot.in; crTok = curTot.cr; cwTok = curTot.cw; outTok = curTot.out; rsnTok = curTot.rsn;
            } else {
              continue;
            }
            const inTok = Math.max(rawIn, crTok + cwTok);
            if (inTok === 0 && outTok === 0) { continue; }
            const model = lastModel !== 'unknown' ? lastModel : 'gpt-5.3-codex';
            const uncached = Math.max(0, inTok - crTok - cwTok);
            const isGpt6 = model.includes('gpt-6');
            const baseIn = isGpt6 ? 200e-6 : 250e-6;
            const crRate = isGpt6 ? 10e-6 : 25e-6;
            const cwRate = (isGpt6 || model.includes('gpt-5.6')) ? baseIn * 1.25 : 0;
            const estAic = uncached * baseIn + crTok * crRate + cwTok * cwRate + outTok * (baseIn * 4.0);
            const nanoAiu = Math.round(estAic * 1e9);
            activeSessions.add(sessionId);
            totalTurns += 1;
            totalIn += inTok;
            totalCache += crTok;
            totalCacheWrite += cwTok;
            totalOut += outTok;
            totalRsn += rsnTok;
            totalNanoAiu += nanoAiu;
            const entry = ensureModel(model, model, codexEffort);
            entry.sessions.add(sessionId);
            entry.turns += 1;
            entry.inputTokens += inTok;
            entry.cacheReadTokens += crTok;
            entry.cacheWriteTokens += cwTok;
            entry.outputTokens += outTok;
            entry.reasoningTokens += rsnTok;
            entry.nanoAiu += nanoAiu;
          }
        } else if (type === 'response_item' && typeof ev.payload === 'object' && ev.payload !== null) {
          const p = ev.payload as Record<string, unknown>;
          if ((p.type === 'function_call' || p.type === 'custom_tool_call') && typeof p.name === 'string' && isCctxToolName(p.name)) {
            const m = lastModel !== 'unknown' ? lastModel : 'gpt-5.3-codex';
            const entry = ensureModel(m, m, codexEffort);
            entry.cctxCalls += 1;
          }
        } else if (type === 'tool.execution_start' && data) {
          const toolName = typeof data.toolName === 'string' ? data.toolName : '';
          if (isCctxToolName(toolName)) {
            const m = typeof data.model === 'string' && data.model ? data.model : lastModel;
            const callId = typeof data.toolCallId === 'string' ? data.toolCallId : '';
            if (callId) { cctxCallModels.set(callId, m); }
            const entry = byModel.get(m);
            if (entry) { entry.cctxCalls += 1; }
          }
        } else if (type === 'tool.execution_complete' && data && line.includes('CACHE_MISS ')) {
          const callId = typeof data.toolCallId === 'string' ? data.toolCallId : '';
          const m = (callId ? cctxCallModels.get(callId) : undefined)
            ?? (typeof data.model === 'string' && data.model ? data.model : lastModel);
          const entry = byModel.get(m);
          if (entry && entry.cctxMisses < entry.cctxCalls) {
            entry.cctxMisses += 1;
          }
        }
      } catch {
        // Skip malformed JSONL lines.
      }
    }
  }
  const models: SessionModelSummary[] = Array.from(byModel.values())
    .sort((a, b) => b.turns - a.turns || b.inputTokens - a.inputTokens)
    .map((m) => ({
      model: m.model,
      displayName: m.displayName,
      effort: m.effort,
      sessions: m.sessions.size,
      turns: m.turns,
      inputTokens: m.inputTokens,
      cacheReadTokens: m.cacheReadTokens,
      cacheWriteTokens: m.cacheWriteTokens,
      outputTokens: m.outputTokens,
      reasoningTokens: m.reasoningTokens,
      usedAic: Math.round((m.nanoAiu / 1e9) * 100) / 100,
      cctxCalls: m.cctxCalls,
      cctxWarmHitPct: m.cctxCalls > 0 ? Math.round((1000 * (m.cctxCalls - m.cctxMisses)) / m.cctxCalls) / 10 : undefined,
    }));
  return {
    updatedAt: nowMs,
    sessions: activeSessions.size,
    turns: totalTurns,
    inputTokens: totalIn,
    cacheReadTokens: totalCache,
    cacheWriteTokens: totalCacheWrite,
    outputTokens: totalOut,
    reasoningTokens: totalRsn,
    usedAic: Math.round((totalNanoAiu / 1e9) * 100) / 100,
    models,
  };
}

function collectRecentJsonlSync(
  dir: string,
  cutoffMs: number,
  depth: number,
  maxDepth: number,
  budget: { remaining: number },
  out: Array<{ file: string; mtimeMs: number; sessionId: string }>,
): void {
  if (depth > maxDepth || budget.remaining <= 0 || !fs.existsSync(dir)) { return; }
  let entries: fs.Dirent[];
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (budget.remaining <= 0) { return; }
    if (entry.isSymbolicLink()) { continue; }
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      collectRecentJsonlSync(full, cutoffMs, depth + 1, maxDepth, budget, out);
    } else if (entry.isFile() && entry.name.endsWith('.jsonl')) {
      budget.remaining -= 1;
      try {
        const stat = fs.statSync(full);
        if (stat.mtimeMs >= cutoffMs) {
          out.push({ file: full, mtimeMs: stat.mtimeMs, sessionId: entry.name.replace(/\.jsonl$/u, '') });
        }
      } catch {
        continue;
      }
    }
  }
}

async function collectRecentJsonlAsync(
  dir: string,
  cutoffMs: number,
  depth: number,
  maxDepth: number,
  budget: { remaining: number },
  out: Array<{ file: string; mtimeMs: number; sessionId: string }>,
): Promise<void> {
  if (depth > maxDepth || budget.remaining <= 0) { return; }
  let entries: fs.Dirent[];
  try {
    entries = await fs.promises.readdir(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (budget.remaining <= 0) { return; }
    if (entry.isSymbolicLink()) { continue; }
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      await collectRecentJsonlAsync(full, cutoffMs, depth + 1, maxDepth, budget, out);
    } else if (entry.isFile() && entry.name.endsWith('.jsonl')) {
      budget.remaining -= 1;
      try {
        const stat = await fs.promises.stat(full);
        if (stat.mtimeMs >= cutoffMs) {
          out.push({ file: full, mtimeMs: stat.mtimeMs, sessionId: entry.name.replace(/\.jsonl$/u, '') });
        }
      } catch {
        continue;
      }
    }
  }
}

function renderAsciiBoxTable(
  title: string,
  headers: readonly string[],
  rows: ReadonlyArray<readonly string[]>,
  rightAlign: readonly boolean[],
): string {
  const activeRows = rows.length > 0 ? rows : [['(none)', ...headers.slice(1).map(() => '-')]];
  const widths = headers.map((h, col) =>
    Math.max(h.length, ...activeRows.map((r) => (r[col] ?? '-').length)),
  );
  const pad = (text: string, width: number, right: boolean): string =>
    right ? text.padStart(width, ' ') : text.padEnd(width, ' ');
  const border = (left: string, mid: string, right: string): string =>
    `${left}${widths.map((w) => '─'.repeat(w + 2)).join(mid)}${right}`;
  const headLine = `│ ${headers.map((h, i) => pad(h, widths[i] ?? h.length, rightAlign[i] ?? false)).join(' │ ')} │`;
  const bodyLines = activeRows.map(
    (row) => `│ ${headers.map((_, i) => pad(row[i] ?? '-', widths[i] ?? 1, rightAlign[i] ?? false)).join(' │ ')} │`,
  );
  return [`  ${title}`, border('┌', '┬', '┐'), headLine, border('├', '┼', '┤'), ...bodyLines, border('└', '┴', '┘')].join('\n');
}

export class UsageSummary {
  public readonly counts: Record<Outcome, number> = {
    success: 0, cache_not_ready: 0, policy_denied: 0, invalid_request: 0, system_error: 0,
  };
  public last: (UsageEvent & { outcome: Outcome; at: number }) | undefined;
  public lastRoot: string | undefined;
  public readonly recentRoots: string[] = [];
  public totalMsSum = 0;
  public timedCalls = 0;
  public estimatedTokensSaved = 0;
  public deliveredTokens = 0;
  public turnCompressionCount = 0;
  public estimatedTurnTokensSaved = 0;
  public readonly byOperation: Record<string, OperationMetrics> = {};
  public sessionTelemetry: SessionTelemetrySnapshot | undefined;
  public backendUsage: BackendUsageSnapshot | undefined;
  private backendQuery: ((hours: number, force: boolean) => Promise<unknown>) | undefined;
  private sessionTelemetryFromBackend = false;
  private readonly recentLocalEvents: Array<{
    completedAt: number;
    outcome: Outcome;
    savedTokens: number;
    savedAic: number;
    savedSecs: number;
  }> = [];
  private lastTelemetryAttemptAt = 0;
  private refreshingPromise: Promise<SessionTelemetrySnapshot | undefined> | undefined;

  public setBackendQuery(query: ((hours: number, force: boolean) => Promise<unknown>) | undefined): void {
    this.backendQuery = query;
  }

  private clearBackendState(): void {
    this.backendUsage = undefined;
    if (this.sessionTelemetryFromBackend) {
      this.sessionTelemetry = undefined;
      this.sessionTelemetryFromBackend = false;
    }
  }

  private pruneRecentLocalEvents(referenceMs: number): void {
    const minTs = referenceMs - 5 * 60_000;
    while (this.recentLocalEvents.length > 0 && (this.recentLocalEvents[0]!.completedAt < minTs || this.recentLocalEvents.length > 512)) {
      this.recentLocalEvents.shift();
    }
  }

  public ingestBackendPayload(raw: unknown, updatedAt = Date.now()): BackendUsageSnapshot | undefined {
    const parsed = parseBackendUsagePayload(raw, updatedAt);
    if (!parsed) { return undefined; }
    this.backendUsage = parsed;
    this.sessionTelemetryFromBackend = true;
    this.pruneRecentLocalEvents(parsed.computedAtMs);
    this.sessionTelemetry = {
      updatedAt,
      sessions: parsed.sessions,
      turns: parsed.turns,
      inputTokens: parsed.inputTokens,
      cacheReadTokens: parsed.cacheReadTokens,
      cacheWriteTokens: parsed.cacheWriteTokens,
      outputTokens: parsed.outputTokens,
      reasoningTokens: parsed.reasoningTokens,
      usedAic: parsed.usedAic,
      models: parsed.models.map((m) => ({
        model: m.model,
        displayName: m.displayName,
        effort: m.effort,
        sessions: m.sessions,
        turns: m.turns,
        inputTokens: m.inputTokens,
        cacheReadTokens: m.cacheReadTokens,
        cacheWriteTokens: m.cacheWriteTokens,
        outputTokens: m.outputTokens,
        reasoningTokens: m.reasoningTokens,
        usedAic: m.usedAic,
        cctxCalls: m.cctxCalls,
      })),
    };
    return parsed;
  }

  public get hasRenderableData(): boolean {
    return Boolean(this.last || this.backendUsage || (this.sessionTelemetry && this.sessionTelemetry.turns > 0));
  }

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
    const op = this.byOperation[event.operation] ??= {
      calls: 0, successes: 0, cacheMisses: 0, deliveredTokens: 0, savedTokens: 0, totalMs: 0, timedCalls: 0,
    };
    op.calls += 1;
    let deltaSavedTokens = 0;
    let deltaSavedAic = 0;
    let deltaSavedSecs = 0;
    if (outcome === 'cache_not_ready') {
      op.cacheMisses += 1;
    }
    if (outcome === 'success') {
      op.successes += 1;
      let flatSaved = 0;
      if (event.operation === 'code_context') {
        flatSaved = 450;
      } else if (event.operation === 'code_grep' || event.operation === 'code_files') {
        flatSaved = 180;
      } else if (event.operation === 'code_read') {
        flatSaved = 120;
      }
      this.estimatedTokensSaved += flatSaved;
      op.savedTokens += flatSaved;
      deltaSavedTokens = flatSaved + (event.turnCompressed === true ? 1200 : 0);
      deltaSavedAic = flatSaved * 0.0004 + (event.turnCompressed === true ? 3.25 : 0);
      deltaSavedSecs = (event.turnCompressed === true ? 4.5 : (flatSaved > 0 ? 0.08 : 0)) + flatSaved / 200_000;
      if (typeof event.deliveredChars === 'number' && event.deliveredChars > 0) {
        this.recordPayload(event.operation, event.deliveredChars, false);
      }
      if (event.turnCompressed === true) {
        this.turnCompressionCount += 1;
        this.estimatedTurnTokensSaved += 1200;
      }
    }
    if (!event.isCommand) {
      this.recentLocalEvents.push({
        completedAt: at,
        outcome,
        savedTokens: deltaSavedTokens,
        savedAic: deltaSavedAic,
        savedSecs: deltaSavedSecs,
      });
      this.pruneRecentLocalEvents(at);
    }
    if (typeof event.totalMs === 'number' && Number.isFinite(event.totalMs) && event.totalMs >= 0) {
      this.totalMsSum += event.totalMs;
      this.timedCalls += 1;
      op.totalMs += event.totalMs;
      op.timedCalls += 1;
    }
    this.last = { ...event, outcome, at };
    if (event.root) {
      this.lastRoot = event.root;
      const existingIdx = this.recentRoots.indexOf(event.root);
      if (existingIdx >= 0) {
        this.recentRoots.splice(existingIdx, 1);
      }
      this.recentRoots.push(event.root);
      while (this.recentRoots.length > 4) {
        this.recentRoots.shift();
      }
    }
  }

  public recordPayload(operation: string, deliveredChars: number, turnCompressed = false): void {
    if (!Number.isFinite(deliveredChars) || deliveredChars <= 0) { return; }
    const deliveredTok = Math.ceil(deliveredChars / 4);
    this.deliveredTokens += deliveredTok;
    const op = this.byOperation[operation] ??= {
      calls: 0, successes: 0, cacheMisses: 0, deliveredTokens: 0, savedTokens: 0, totalMs: 0, timedCalls: 0,
    };
    op.deliveredTokens += deliveredTok;
    if (turnCompressed) {
      this.turnCompressionCount += 1;
      this.estimatedTurnTokensSaved += 1200;
    }
  }

  public refreshSessionTelemetry(
    customHome?: string,
    nowMs = Date.now(),
    force = false,
  ): SessionTelemetrySnapshot | undefined {
    if (!force && this.lastTelemetryAttemptAt > 0 && nowMs - this.lastTelemetryAttemptAt < 60_000) {
      return this.sessionTelemetry;
    }
    this.lastTelemetryAttemptAt = nowMs;
    try {
      const homeCandidates = customHome
        ? [customHome]
        : Array.from(new Set([os.homedir(), process.env.XDG_STATE_HOME].filter((p): p is string => Boolean(p))));
      const cutoffMs = nowMs - 12 * 3600 * 1000;
      const candidates: Array<{ file: string; mtimeMs: number; sessionId: string }> = [];
      const seenSessionIds = new Set<string>();
      for (const baseHome of homeCandidates) {
        const sessionDir = path.join(baseHome, '.copilot', 'session-state');
        if (fs.existsSync(sessionDir)) {
          for (const entry of fs.readdirSync(sessionDir, { withFileTypes: true })) {
            if (!entry.isDirectory() || seenSessionIds.has(entry.name)) { continue; }
            const eventsFile = path.join(sessionDir, entry.name, 'events.jsonl');
            try {
              const stat = fs.statSync(eventsFile);
              if (stat.mtimeMs >= cutoffMs) {
                seenSessionIds.add(entry.name);
                candidates.push({ file: eventsFile, mtimeMs: stat.mtimeMs, sessionId: entry.name });
              }
            } catch {
              // Ignore rotated or inaccessible session files.
            }
          }
        }
        const scanBudget = { remaining: 200 };
        collectRecentJsonlSync(path.join(baseHome, '.claude', 'projects'), cutoffMs, 0, 2, scanBudget, candidates);
        collectRecentJsonlSync(path.join(baseHome, '.codex', 'sessions'), cutoffMs, 0, 4, scanBudget, candidates);
      }
      if (candidates.length === 0) { return this.sessionTelemetry; }
      candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);
      const selected = candidates.slice(0, 6);
      const rawChunks: Array<{ sessionId: string; raw: string; isTailSliced: boolean }> = [];
      const maxBytes = 128 * 1024;
      for (const item of selected) {
        try {
          const stat = fs.statSync(item.file);
          let raw = '';
          let isTailSliced = false;
          if (stat.size <= maxBytes) {
            raw = fs.readFileSync(item.file, 'utf8');
          } else {
            isTailSliced = true;
            const fd = fs.openSync(item.file, 'r');
            try {
              const buf = Buffer.allocUnsafe(maxBytes);
              const read = fs.readSync(fd, buf, 0, maxBytes, stat.size - maxBytes);
              raw = buf.subarray(0, read).toString('utf8');
              const firstNl = raw.indexOf('\n');
              if (firstNl >= 0) { raw = raw.slice(firstNl + 1); }
            } finally {
              fs.closeSync(fd);
            }
          }
          rawChunks.push({ sessionId: item.sessionId, raw, isTailSliced });
        } catch {
          continue;
        }
      }
      this.sessionTelemetry = parseSessionTelemetryChunks(rawChunks, nowMs);
      this.sessionTelemetryFromBackend = false;
    } catch {
      // Ignore file-system telemetry errors in restricted environments.
    }
    return this.sessionTelemetry;
  }

  public refreshSessionTelemetryAsync(
    customHome?: string,
    nowMs = Date.now(),
    force = false,
  ): Promise<SessionTelemetrySnapshot | undefined> {
    if (!force && !customHome && this.lastTelemetryAttemptAt > 0 && nowMs - this.lastTelemetryAttemptAt < 15_000) {
      return Promise.resolve(this.sessionTelemetry);
    }
    if (!force && customHome && this.lastTelemetryAttemptAt > 0 && nowMs - this.lastTelemetryAttemptAt < 60_000) {
      return Promise.resolve(this.sessionTelemetry);
    }
    if (this.refreshingPromise) {
      return this.refreshingPromise;
    }
    const run = (async (): Promise<SessionTelemetrySnapshot | undefined> => {
      if (this.backendQuery) {
        try {
          const payload = await this.backendQuery(24, force);
          if (this.ingestBackendPayload(payload, nowMs)) {
            return this.sessionTelemetry;
          }
          this.clearBackendState();
        } catch {
          this.clearBackendState();
        }
      }
      try {
        const homeCandidates = customHome
          ? [customHome]
          : Array.from(new Set([os.homedir(), process.env.XDG_STATE_HOME].filter((p): p is string => Boolean(p))));
        const cutoffMs = nowMs - 12 * 3600 * 1000;
        const candidates: Array<{ file: string; mtimeMs: number; sessionId: string }> = [];
        const seenSessionIds = new Set<string>();
        for (const baseHome of homeCandidates) {
          const sessionDir = path.join(baseHome, '.copilot', 'session-state');
          try {
            const entries = await fs.promises.readdir(sessionDir, { withFileTypes: true });
            for (const entry of entries) {
              if (!entry.isDirectory() || seenSessionIds.has(entry.name)) { continue; }
              const eventsFile = path.join(sessionDir, entry.name, 'events.jsonl');
              try {
                const stat = await fs.promises.stat(eventsFile);
                if (stat.mtimeMs >= cutoffMs) {
                  seenSessionIds.add(entry.name);
                  candidates.push({ file: eventsFile, mtimeMs: stat.mtimeMs, sessionId: entry.name });
                }
              } catch {
                // Ignore rotated or inaccessible session files.
              }
            }
          } catch {
            // Copilot session-state directory may not exist when using Claude or Codex only.
          }
          const scanBudget = { remaining: 200 };
          await collectRecentJsonlAsync(path.join(baseHome, '.claude', 'projects'), cutoffMs, 0, 2, scanBudget, candidates);
          await collectRecentJsonlAsync(path.join(baseHome, '.codex', 'sessions'), cutoffMs, 0, 4, scanBudget, candidates);
        }
        if (candidates.length === 0) {
          this.lastTelemetryAttemptAt = nowMs;
          return this.sessionTelemetry;
        }
        candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);
        const selected = candidates.slice(0, 6);
        const rawChunks: Array<{ sessionId: string; raw: string; isTailSliced: boolean }> = [];
        const maxBytes = 128 * 1024;
        for (const item of selected) {
          try {
            const stat = await fs.promises.stat(item.file);
            let raw = '';
            let isTailSliced = false;
            if (stat.size <= maxBytes) {
              raw = await fs.promises.readFile(item.file, 'utf8');
            } else {
              isTailSliced = true;
              const handle = await fs.promises.open(item.file, 'r');
              try {
                const buf = Buffer.allocUnsafe(maxBytes);
                const { bytesRead } = await handle.read(buf, 0, maxBytes, stat.size - maxBytes);
                raw = buf.subarray(0, bytesRead).toString('utf8');
                const firstNl = raw.indexOf('\n');
                if (firstNl >= 0) { raw = raw.slice(firstNl + 1); }
              } finally {
                await handle.close();
              }
            }
            rawChunks.push({ sessionId: item.sessionId, raw, isTailSliced });
          } catch {
            continue;
          }
        }
        this.sessionTelemetry = parseSessionTelemetryChunks(rawChunks, nowMs);
        this.sessionTelemetryFromBackend = false;
      } catch {
        // Missing ~/.copilot/session-state (e.g. Windows UI client) stays cached for 60s.
      }
      return this.sessionTelemetry;
    })().finally(() => {
      this.lastTelemetryAttemptAt = nowMs;
      this.refreshingPromise = undefined;
    });
    this.refreshingPromise = run;
    return run;
  }

  public get avgLatencyMs(): number | undefined {
    return this.timedCalls > 0 ? this.totalMsSum / this.timedCalls : undefined;
  }

  public get totalEstimatedTokensSaved(): number {
    return this.estimatedTokensSaved + this.estimatedTurnTokensSaved;
  }

  public get estimatedAicSaved(): number {
    return this.estimatedTokensSaved * 0.0004 + this.turnCompressionCount * 3.25;
  }

  public get estimatedTimeSavedSecs(): number {
    return this.turnCompressionCount * 4.5 + this.counts.success * 0.08 + this.estimatedTokensSaved / 200_000;
  }

  private effectiveLatencyMs(): number | undefined {
    const bu = this.backendUsage;
    if (bu?.sourceMedianMs !== undefined) { return bu.sourceMedianMs; }
    if (bu?.daemonRecentSourceAvgMs !== undefined) { return bu.daemonRecentSourceAvgMs; }
    return this.avgLatencyMs;
  }

  private effectiveBackendTotals(bu: BackendUsageSnapshot): {
    sCount: number;
    cCount: number;
    pCount: number;
    eCount: number;
    savedTok: number;
    savedAic: number;
    savedSecs: number;
  } {
    if (bu.cctxCalls === 0 && bu.turns === 0) {
      return {
        sCount: this.counts.success,
        cCount: this.counts.cache_not_ready,
        pCount: this.counts.policy_denied,
        eCount: this.counts.invalid_request + this.counts.system_error,
        savedTok: this.totalEstimatedTokensSaved,
        savedAic: this.estimatedAicSaved,
        savedSecs: this.estimatedTimeSavedSecs,
      };
    }
    let deltaSuccess = 0;
    let deltaMiss = 0;
    let deltaDeny = 0;
    let deltaErr = 0;
    let deltaTok = 0;
    let deltaAic = 0;
    const burstSecs = new Map<number, number>();
    for (const ev of this.recentLocalEvents) {
      if (ev.completedAt <= bu.computedAtMs) { continue; }
      if (ev.outcome === 'success') {
        deltaSuccess += 1;
        deltaTok += ev.savedTokens;
        deltaAic += ev.savedAic;
        const bucket = Math.floor(ev.completedAt / 2000);
        burstSecs.set(bucket, Math.min(6.75, (burstSecs.get(bucket) ?? 0) + ev.savedSecs));
      } else if (ev.outcome === 'cache_not_ready') {
        deltaMiss += 1;
      } else if (ev.outcome === 'policy_denied') {
        deltaDeny += 1;
      } else {
        deltaErr += 1;
      }
    }
    let deltaSecs = 0;
    for (const s of burstSecs.values()) { deltaSecs += s; }
    return {
      sCount: bu.cctxSuccesses + deltaSuccess,
      cCount: bu.cctxCacheMisses + deltaMiss,
      pCount: bu.cctxPolicyDenied + deltaDeny,
      eCount: bu.cctxErrors + deltaErr,
      savedTok: bu.tokensSaved + deltaTok,
      savedAic: bu.savedAic + deltaAic,
      savedSecs: bu.timeSavedSecs + deltaSecs,
    };
  }

  public text(): string {
    const bu = this.backendUsage;
    if (bu) {
      const { sCount, cCount, pCount, eCount, savedTok: saved, savedAic, savedSecs } = this.effectiveBackendTotals(bu);
      const totalCalls = sCount + cCount + pCount + eCount;
      if (saved > 0 || savedAic > 0) {
        const parts: string[] = [`$(database) CCTX +$${(savedAic * 0.001).toFixed(2)}`];
        if (savedSecs >= 1) {
          parts.push(`~${formatCompactDuration(savedSecs, true)}`);
        }
        parts.push(`${formatShortTokens(saved)} tok`);
        if (eCount >= 5 && totalCalls > 0 && eCount / totalCalls >= 0.02) {
          parts.push(`$(warning) Err ${eCount}`);
        } else if (bu.selectionPercent !== undefined) {
          parts.push(`${Math.round(bu.selectionPercent)}%`);
        }
        return parts.join(' · ');
      }
      if (totalCalls > 0) {
        return `$(database) CCTX ${sCount} OK${eCount > 0 ? ` · Err ${eCount}` : ''}`;
      }
      return '$(database) CCTX Ready';
    }
    const counts = this.counts;
    const base = `$(database) CCTX N S:${counts.success} C:${counts.cache_not_ready} P:${counts.policy_denied} E:${counts.invalid_request + counts.system_error}`;
    const saved = this.totalEstimatedTokensSaved;
    const savedAic = this.estimatedAicSaved;
    const topModel = this.sessionTelemetry?.models[0]?.displayName;
    if (topModel && saved > 0) {
      return `${base} · ${topModel} · ~${formatCompactTokens(saved)} tok (+$${(savedAic * 0.001).toFixed(2)})`;
    }
    if (saved > 0) {
      return `${base} · ~${formatCompactTokens(saved)} tok (+$${(savedAic * 0.001).toFixed(2)})`;
    }
    return base;
  }

  public tooltip(): string {
    const bu = this.backendUsage;
    if (bu) {
      const { sCount, cCount, pCount, eCount, savedTok, savedAic, savedSecs } = this.effectiveBackendTotals(bu);
      const effMs = this.effectiveLatencyMs();
      const latLabel = formatSmartMs(effMs);
      const adoptLabel = bu.selectionPercent !== undefined ? `${bu.selectionPercent.toFixed(1)}%` : 'n/a';
      const activeModels = bu.models.filter((m) => m.inputTokens > 0 || m.outputTokens > 0 || m.usedAic > 0 || m.cctxCalls > 0);
      const modelLine = activeModels.length > 0
        ? `Models (${bu.sessions} sessions, ${bu.turns} turns, spend $${(bu.usedAic * 0.001).toFixed(2)}): ` +
          activeModels
            .slice(0, 6)
            .map((m) => `${m.displayName} (${m.effort}, ${m.turns}t, +$${(m.savedAic * 0.001).toFixed(2)}, ~${formatCompactDuration(m.savedSecs)})`)
            .join('; ')
        : undefined;
      return [
        `Code Context ${Math.round(bu.hours)}h savings (${bu.sessions} sessions · ${bu.turns} turns · adoption ${adoptLabel}).`,
        `Saved: +$${(savedAic * 0.001).toFixed(2)} · ~${formatCompactDuration(savedSecs)} · ~${formatCompactTokens(savedTok)} tok (median speed ${latLabel}; OK ${sCount}, miss ${cCount}, denied ${pCount}, err ${eCount}).`,
        ...(modelLine ? [modelLine] : []),
        'Click for full 24h dashboard, daemon status and adaptive memory.',
      ].join('\n');
    }
    const c = this.counts;
    const multiRoots =
      this.recentRoots.length > 1
        ? `Recent roots (${this.recentRoots.length}): ${this.recentRoots.join(', ')}`
        : this.lastRoot
          ? `Last requested root: ${this.lastRoot}`
          : 'Pass an explicit Linux root in Windows Agent Sessions.';
    const avgMs = this.avgLatencyMs !== undefined ? `${this.avgLatencyMs.toFixed(2)} ms` : 'n/a';
    const savedAic = this.estimatedAicSaved;
    const modelLine = this.sessionTelemetry && this.sessionTelemetry.models.length > 0
      ? `Active LLM models (${this.sessionTelemetry.sessions} recent sessions, ${this.sessionTelemetry.turns} turns, ${this.sessionTelemetry.usedAic.toFixed(1)} AIC / $${(this.sessionTelemetry.usedAic * 0.001).toFixed(2)} @$0.001): ` +
        this.sessionTelemetry.models
          .map((m) => `${m.displayName} (${m.effort}, ${m.turns}t, in ${formatCompactTokens(m.inputTokens)}, ${m.usedAic.toFixed(1)} AIC)`)
          .join('; ')
      : undefined;
    return [
      'Native calls in this extension host (including metadata), not all sessions.',
      `Success ${c.success}; cache not ready ${c.cache_not_ready}; policy denied ${c.policy_denied}; invalid input ${c.invalid_request}; system errors ${c.system_error}.`,
      `Estimated lookup tokens saved: ~${this.estimatedTokensSaved} tokens (total with turn compression: ~${this.totalEstimatedTokensSaved} tokens; ~${savedAic.toFixed(2)} AIC / ~$${(savedAic * 0.001).toFixed(3)} @$0.001 · ~$${(savedAic * 0.01).toFixed(2)} API; delivered: ~${this.deliveredTokens} tokens; avg latency: ${avgMs}).`,
      ...(modelLine ? [modelLine] : []),
      this.last ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} ${this.last.outcome}` : 'No calls observed yet.',
      multiRoots,
      'Click for daemon status, adaptive memory and aggregate native/MCP outcomes.',
    ].join('\n');
  }

  public markdownTooltip(): string {
    const bu = this.backendUsage;
    if (bu) {
      const c = this.counts;
      const { sCount, cCount, pCount, eCount, savedTok, savedAic, savedSecs } = this.effectiveBackendTotals(bu);
      const totalCalls = sCount + cCount + pCount + eCount;
      const warmHitPct = bu.warmHitPercent !== undefined
        ? `${bu.warmHitPercent.toFixed(1)}%`
        : (sCount + cCount) > 0
          ? `${((100 * sCount) / (sCount + cCount)).toFixed(1)}%`
          : '-';
      const adoptMain = bu.selectionPercent !== undefined ? `**${Math.round(bu.selectionPercent)}%**` : '-';
      const effMs = this.effectiveLatencyMs();
      const latStr = formatSmartMs(effMs);
      const costBase = bu.usedAic + savedAic;
      const costPctStr = costBase > 0 ? `-${((100 * savedAic) / costBase).toFixed(1)}%` : '0%';
      const effTurns = Math.round(bu.effectiveAvoidedTurns > 0 ? bu.effectiveAvoidedTurns : bu.turnCompressionCalls);
      const cachePct = bu.promptCacheHitPercent !== undefined
        ? `${bu.promptCacheHitPercent.toFixed(1)}%`
        : bu.inputTokens > 0
          ? `${((100 * bu.cacheReadTokens) / bu.inputTokens).toFixed(1)}%`
          : '-';

      const lines: string[] = [
        `### $(sparkle) Code Context — Last ${Math.round(bu.hours)}h  ·  ${bu.sessions} sessions · ${bu.turns.toLocaleString('en-US')} turns`,
        '',
        '| 💰 Cost Saved | ⏱️ Time Saved | 🔥 Tokens Saved | 🎯 Adopt · Speed |',
        '| :---: | :---: | :---: | :---: |',
        `| **+$${(savedAic * 0.001).toFixed(2)}** (\`${costPctStr}\`) | **~${formatCompactDuration(savedSecs)}** | **~${formatShortTokens(savedTok)} tok** | ${adoptMain} · **${latStr}** |`,
        `| _Spend $${(bu.usedAic * 0.001).toFixed(2)} (API +$${Math.round(savedAic * 0.01)})_ | _~${effTurns.toLocaleString('en-US')} turns + prefill_ | _Cache ${cachePct} hit_ | _${warmHitPct} warm (${sCount.toLocaleString('en-US')}/${totalCalls.toLocaleString('en-US')})_ |`,
        '',
      ];

      const activeModels = bu.models.filter(
        (m) => m.inputTokens > 0 || m.outputTokens > 0 || m.usedAic > 0 || m.cctxCalls > 0,
      );
      if (activeModels.length > 0) {
        const freshAndWrite = Math.max(0, bu.uncachedInputTokens + bu.cacheWriteTokens);
        lines.push(
          `**$(hubot) Models** · Input \`${formatShortTokens(bu.inputTokens)}\` (Cache \`${cachePct}\` · Fresh+Write \`${formatShortTokens(freshAndWrite)}\`)`,
          '',
          '| Model | Turns | Spend → Saved | Time | Adopt |',
          '| :--- | ---: | ---: | ---: | ---: |',
        );
        for (const m of activeModels.slice(0, 6)) {
          const safeName = escapeMdCell(m.displayName);
          const safeEffort = escapeMdCell(m.effort === 'claude-agent' ? 'agent' : m.effort);
          const effTag = safeEffort && safeEffort !== '-' ? ` \`${safeEffort}\`` : '';
          const timeCol = m.savedSecs >= 1 ? `~${formatCompactDuration(m.savedSecs, true)}` : '-';
          let adoptCol = m.cctxCalls > 0 ? `${m.cctxCalls.toLocaleString('en-US')}` : '0';
          if (m.adoptionPercent !== undefined) {
            const pctRound = Math.round(m.adoptionPercent);
            adoptCol = m.adoptionPercent < 50 && m.lookupCalls >= 5
              ? `$(warning) **${pctRound}%** (${m.cctxCalls})`
              : `${pctRound}% (${m.cctxCalls.toLocaleString('en-US')})`;
          }
          lines.push(
            `| **${safeName}**${effTag} | ${m.turns.toLocaleString('en-US')} | $${(m.usedAic * 0.001).toFixed(2)} → **+$${(m.savedAic * 0.001).toFixed(2)}** | ${timeCol} | ${adoptCol} |`,
          );
        }
        lines.push('');
      }

      if (bu.tools.length > 0) {
        const sourceOps = new Set(['code_read', 'code_grep', 'code_context', 'code_files']);
        const coreTools = bu.tools.filter((t) => sourceOps.has(t.operation));
        const adminTools = bu.tools.filter((t) => !sourceOps.has(t.operation));
        const shownTools = coreTools.length > 0 ? coreTools : bu.tools.slice(0, 4);
        const srcCalls = shownTools.reduce((acc, t) => acc + t.calls, 0);
        const srcOk = shownTools.reduce((acc, t) => acc + t.successes, 0);
        const srcOkPct = srcCalls > 0 ? ((100 * srcOk) / srcCalls).toFixed(1) : '100.0';
        lines.push(
          `**$(zap) Source Tools** · \`${srcCalls.toLocaleString('en-US')}\` calls (\`${srcOkPct}%\` OK)`,
          '',
          '| Tool | Calls | Avg Size | Speed (p50 / p95) |',
          '| :--- | ---: | ---: | ---: |',
        );
        for (const t of shownTools) {
          lines.push(
            `| \`${t.operation}\` | ${formatToolCallBadge(t)} | ~${formatShortTokens(t.avgPayloadTokens)} tok | ${formatSmartMs(t.medianMs)} / ${formatSmartMs(t.p95Ms)} |`,
          );
        }
        if (coreTools.length > 0 && adminTools.length > 0) {
          const adminSummary = adminTools.map((t) => `${t.operation} ${t.calls}`).join(' · ');
          lines.push('', `_Admin ops: ${adminSummary}_`);
        }
        lines.push('');
      } else {
        const opEntries = Object.entries(this.byOperation);
        if (opEntries.length > 0) {
          lines.push(
            '**$(zap) Extension Host Native Tool Breakdown**',
            '',
            '| Operation | Calls (OK/Miss) | Delivered | Saved Tok | Avg ms |',
            '| :--- | ---: | ---: | ---: | ---: |',
          );
          for (const [opName, stats] of opEntries) {
            const opAvg = stats.timedCalls > 0 ? `${(stats.totalMs / stats.timedCalls).toFixed(2)}ms` : '-';
            lines.push(
              `| \`${opName}\` | ${stats.calls} (${stats.successes}/${stats.cacheMisses}) | ~${formatCompactTokens(stats.deliveredTokens)} | ~${formatCompactTokens(stats.savedTokens)} | ${opAvg} |`,
            );
          }
          lines.push('');
        }
      }

      const daemonMeta = bu.daemonLoadedRoots !== undefined && bu.daemonRegisteredRoots !== undefined
        ? `🟢 Daemon \`${bu.daemonState ?? 'ready'}\` · \`${bu.daemonLoadedRoots}/${bu.daemonRegisteredRoots}\` roots (\`${Math.round((bu.daemonEstimatedBytes ?? 0) / (1024 * 1024))} MiB\`) · `
        : '🟢 ';
      const bridgeErrs = c.invalid_request + c.system_error;
      const bridgeNote = bridgeErrs > 0 ? ` · $(warning) Bridge err \`${bridgeErrs}\`` : '';
      lines.push(
        `${daemonMeta}Updated \`${formatLocalUpdated(bu.updatedAt)}\`${bridgeNote}`,
        '',
        '[$(pulse) Show Full Dashboard](command:codeContextNative.showDashboard) · [$(beaker) Run Self-Test](command:codeContextNative.selfTest)',
      );
      return lines.join('\n');
    }

    const c = this.counts;
    const avgMs = this.avgLatencyMs !== undefined ? `${this.avgLatencyMs.toFixed(2)} ms` : '-';
    const totalCalls = c.success + c.cache_not_ready + c.policy_denied + c.invalid_request + c.system_error;
    const warmHitPct = (c.success + c.cache_not_ready) > 0
      ? `${((100 * c.success) / (c.success + c.cache_not_ready)).toFixed(1)}%`
      : '-';
    const savedAic = this.estimatedAicSaved;
    const lines: string[] = [
      '### $(sparkle) Code Context Native — Model, Token & Cost Savings',
      '',
      '| 💰 Cost Saved (@$0.001) | 💎 API Value (@$0.01) | 🔥 Tokens Saved | 🎯 Warm Hit Rate | ⚡ Avg Latency |',
      '| :---: | :---: | :---: | :---: | :---: |',
      `| **+$${(savedAic * 0.001).toFixed(2)}** (\`~${savedAic.toFixed(1)} AIC\`) | **+$${(savedAic * 0.01).toFixed(2)}** | **~${formatCompactTokens(this.totalEstimatedTokensSaved)} tok** | **${warmHitPct}** (${c.success}/${totalCalls}) | **${avgMs}** |`,
      '',
    ];
    if (this.sessionTelemetry && this.sessionTelemetry.models.length > 0) {
      const st = this.sessionTelemetry;
      const cachePct = st.inputTokens > 0 ? `${((100 * st.cacheReadTokens) / st.inputTokens).toFixed(1)}%` : '-';
      const freshAndWrite = Math.max(0, st.inputTokens - st.cacheReadTokens);
      lines.push(
        `**$(hubot) Active LLM Models** (${st.sessions} sessions · ${st.turns} turns · Spend \`$${(st.usedAic * 0.001).toFixed(2)}\` / \`${st.usedAic.toFixed(1)} AIC\` · GitHub AI Credits ref \`$${(st.usedAic * 0.01).toFixed(2)}\` · Cache \`${cachePct}\` · Fresh+Write \`${formatCompactTokens(freshAndWrite)}\`)`,
        '',
        '| Model | Effort | Turns | Input (Cache%) | Out + Rsn | LLM Spend (@$0.001) | CCTX Calls |',
        '| :--- | :---: | ---: | ---: | ---: | ---: | ---: |',
      );
      for (const m of st.models.slice(0, 4)) {
        const mCache = m.inputTokens > 0 ? `${((100 * m.cacheReadTokens) / m.inputTokens).toFixed(1)}%` : '-';
        lines.push(
          `| **${m.displayName}** | \`${m.effort}\` | ${m.turns} | ${formatCompactTokens(m.inputTokens)} (${mCache}) | ${formatCompactTokens(m.outputTokens)} + ${formatCompactTokens(m.reasoningTokens)} | $${(m.usedAic * 0.001).toFixed(2)} (${m.usedAic.toFixed(1)} AIC) | ${m.cctxCalls} |`,
        );
      }
      lines.push('');
    }
    const opEntries = Object.entries(this.byOperation);
    if (opEntries.length > 0) {
      lines.push(
        '**$(zap) Extension Host Native Tool Breakdown**',
        '',
        '| Operation | Calls (OK/Miss) | Delivered | Saved Tok | Avg ms |',
        '| :--- | ---: | ---: | ---: | ---: |',
      );
      for (const [opName, stats] of opEntries) {
        const opAvg = stats.timedCalls > 0 ? `${(stats.totalMs / stats.timedCalls).toFixed(2)}ms` : '-';
        lines.push(
          `| \`${opName}\` | ${stats.calls} (${stats.successes}/${stats.cacheMisses}) | ~${formatCompactTokens(stats.deliveredTokens)} | ~${formatCompactTokens(stats.savedTokens)} | ${opAvg} |`,
        );
      }
      lines.push('');
    }
    lines.push(
      `_${this.last ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} (${this.last.outcome})` : 'No calls observed yet.'}_`,
      '',
      'Native calls in this extension host (including metadata), not all sessions.',
      '[$(pulse) Show Full Dashboard](command:codeContextNative.showDashboard) · [$(beaker) Run Self-Test](command:codeContextNative.selfTest)',
    );
    return lines.join('\n');
  }

  public formatDashboard(daemonStatus?: unknown): string {
    const bu = this.backendUsage;
    if (bu) {
      const { sCount, cCount, pCount, eCount, savedTok, savedAic, savedSecs } = this.effectiveBackendTotals(bu);
      const effMs = this.effectiveLatencyMs();
      const latStr = effMs !== undefined ? `${effMs.toFixed(2)}ms` : '-';
      const adoptStr = bu.selectionPercent !== undefined ? `${bu.selectionPercent.toFixed(1)}%` : '-';
      const lines: string[] = [
        '╔══════════════════════════════════════════════════════════════════════════════════╗',
        `║  ⚡ CODE CONTEXT — UNIFIED ${Math.round(bu.hours)}H MODEL, TIME, TOKEN & COST SAVINGS DASHBOARD       ║`,
        '╠══════════════════════════════════════════════════════════════════════════════════╣',
        `║  💰 COST SAVED (@$0.001/AIC) : +$${(savedAic * 0.001).toFixed(2)} USD (~${savedAic.toFixed(1)} AIC across ${bu.sessions} sessions / ${bu.turns} turns)`,
        `║  ⏱️ TIME SAVED (EST.)        : ~${formatCompactDuration(savedSecs)} (Turns ~${formatCompactDuration(bu.turnTimeSavedSecs)} + Prefill ~${formatCompactDuration(bu.prefillTimeSavedSecs)} + Index ~${formatCompactDuration(bu.ioTimeSavedSecs)})`,
        `║  💎 API LIST VALUE (@$0.01)  : +$${(savedAic * 0.01).toFixed(2)} USD (GitHub AI Credits / API list ref.)`,
        `║  🔥 TOKENS SAVED             : ~${formatCompactTokens(savedTok)} tok (Direct ~${formatCompactTokens(bu.directTokensSaved)} + Cache ~${formatCompactTokens(bu.compoundedTokensSaved)} + ${bu.turnCompressionCalls}x Turn ~${formatCompactTokens(bu.turnTokensSaved)})`,
        `║  ⚡ UNIFIED CCTX CALLS       : S:${sCount} C:${cCount} P:${pCount} E:${eCount} · Adopt ${adoptStr} · Median ${latStr}`,
        '╚══════════════════════════════════════════════════════════════════════════════════╝',
      ];
      const ds = asObj(daemonStatus);
      if (ds) {
        const indexState = typeof ds.index_state === 'string' ? ds.index_state : 'online';
        const cache = asObj(ds.cache);
        const adm = asObj(ds.daemon_admission);
        const reg = asObj(ds.registry_admission);
        const hits = asFiniteNumber(cache?.hits);
        const misses = asFiniteNumber(cache?.misses);
        const hitPct = hits + misses > 0 ? `${((100 * hits) / (hits + misses)).toFixed(1)}%` : '-';
        const loaded = asFiniteNumber(adm?.loaded_roots);
        const registered = asFiniteNumber(reg?.registered_roots);
        const mib = ((asFiniteNumber(adm?.estimated_bytes)) / (1024 * 1024)).toFixed(1);
        lines.push(`  • Shared Daemon: ${indexState}   • Resident Roots: ${loaded}/${registered} (${mib} MiB)   • Daemon Cache Hit: ${hitPct} (${hits}/${hits + misses})`);
      }
      if (bu.models.length > 0) {
        lines.push('');
        const modelRows = bu.models.map((m) => {
          const cachePct = m.inputTokens > 0 ? `${((100 * m.cacheReadTokens) / m.inputTokens).toFixed(1)}%` : '-';
          const adoptPct = m.adoptionPercent !== undefined ? `${m.cctxCalls} (${m.adoptionPercent.toFixed(0)}%)` : `${m.cctxCalls}`;
          return [
            `${m.displayName} (${m.effort})`,
            `${m.sessions}/${m.turns}`,
            formatCompactTokens(m.inputTokens),
            cachePct,
            `${formatCompactTokens(m.outputTokens)}+${formatCompactTokens(m.reasoningTokens)}`,
            `$${(m.usedAic * 0.001).toFixed(2)}`,
            `+$${(m.savedAic * 0.001).toFixed(2)}`,
            m.savedSecs >= 1 ? `~${formatCompactDuration(m.savedSecs, true)}` : '-',
            adoptPct,
          ];
        });
        lines.push(renderAsciiBoxTable(
          `🤖 Active LLM Models & Session Token Usage (Recent ${Math.round(bu.hours)}h Window)`,
          ['Model (Effort)', 'Sess/Turns', 'Input Tok', 'Prompt Cache', 'Out+Rsn Tok', 'Spend (@$0.001)', 'Saved (@$0.001)', 'Time Saved', 'CCTX (Adopt%)'],
          modelRows,
          [false, true, true, true, true, true, true, true, true],
        ));
      }
      if (bu.tools.length > 0) {
        lines.push('');
        const toolRows = bu.tools.map((t) => [
          t.operation,
          `${t.calls}`,
          `${t.successes}`,
          `${t.cacheMisses}`,
          `${t.policyDenied + t.errors}`,
          `~${formatCompactTokens(t.avgPayloadTokens)}`,
          t.medianMs !== undefined ? `${t.medianMs.toFixed(2)}ms` : '-',
          t.p95Ms !== undefined ? `${t.p95Ms.toFixed(2)}ms` : '-',
        ]);
        lines.push(renderAsciiBoxTable(
          `⚡ Unified CCTX Tool Breakdown (${Math.round(bu.hours)}h MCP + Native)`,
          ['Operation', 'Calls', 'Success', 'Cache Miss', 'Err/Deny', 'Avg Payload', 'Median ms', 'P95 ms'],
          toolRows,
          [false, true, true, true, true, true, true, true],
        ));
      }
      return lines.join('\n');
    }

    this.refreshSessionTelemetry();
    const c = this.counts;
    const avgMs = this.avgLatencyMs !== undefined ? `${this.avgLatencyMs.toFixed(2)}ms` : '-';
    const savedAic = this.estimatedAicSaved;
    const lines: string[] = [
      '╔══════════════════════════════════════════════════════════════════════════════════╗',
      '║  ⚡ CODE CONTEXT NATIVE — MODEL, TOKEN & COST SAVINGS DASHBOARD                  ║',
      '╠══════════════════════════════════════════════════════════════════════════════════╣',
      `║  💰 COST SAVED (@$0.001/AIC) : +$${(savedAic * 0.001).toFixed(2)} USD (~${savedAic.toFixed(1)} AIC saved in this host)              ║`,
      `║  💎 API LIST VALUE (@$0.01)  : +$${(savedAic * 0.01).toFixed(2)} USD (GitHub AI Credits / API list ref.)       ║`,
      `║  🔥 TOKENS SAVED             : ~${formatCompactTokens(this.totalEstimatedTokensSaved)} tok (Lookup ~${formatCompactTokens(this.estimatedTokensSaved)} + ${this.turnCompressionCount}x Turn-Comp ~${formatCompactTokens(this.estimatedTurnTokensSaved)})       ║`,
      `║  ⚡ HOST NATIVE CALLS        : S:${c.success} C:${c.cache_not_ready} P:${c.policy_denied} E:${c.invalid_request + c.system_error} · Delivered ~${formatCompactTokens(this.deliveredTokens)} tok · Avg ${avgMs} ║`,
      '╚══════════════════════════════════════════════════════════════════════════════════╝',
    ];
    if (typeof daemonStatus === 'object' && daemonStatus !== null && !Array.isArray(daemonStatus)) {
      const ds = daemonStatus as Record<string, unknown>;
      const indexState = typeof ds.index_state === 'string' ? ds.index_state : 'online';
      const cache = typeof ds.cache === 'object' && ds.cache !== null ? ds.cache as Record<string, unknown> : undefined;
      const adm = typeof ds.daemon_admission === 'object' && ds.daemon_admission !== null ? ds.daemon_admission as Record<string, unknown> : undefined;
      const reg = typeof ds.registry_admission === 'object' && ds.registry_admission !== null ? ds.registry_admission as Record<string, unknown> : undefined;
      const hits = typeof cache?.hits === 'number' ? cache.hits : 0;
      const misses = typeof cache?.misses === 'number' ? cache.misses : 0;
      const hitPct = hits + misses > 0 ? `${((100 * hits) / (hits + misses)).toFixed(1)}%` : '-';
      const loaded = typeof adm?.loaded_roots === 'number' ? adm.loaded_roots : 0;
      const registered = typeof reg?.registered_roots === 'number' ? reg.registered_roots : 0;
      const mib = typeof adm?.estimated_bytes === 'number' ? (adm.estimated_bytes / (1024 * 1024)).toFixed(1) : '0.0';
      lines.push(`  • Shared Daemon: ${indexState}   • Resident Roots: ${loaded}/${registered} (${mib} MiB)   • Daemon Cache Hit: ${hitPct} (${hits}/${hits + misses})`);
    }
    if (this.sessionTelemetry && this.sessionTelemetry.models.length > 0) {
      lines.push('');
      const modelRows = this.sessionTelemetry.models.map((m) => {
        const cachePct = m.inputTokens > 0 ? `${((100 * m.cacheReadTokens) / m.inputTokens).toFixed(1)}%` : '-';
        return [
          `${m.displayName} (${m.effort})`,
          `${m.sessions}/${m.turns}`,
          formatCompactTokens(m.inputTokens),
          cachePct,
          `${formatCompactTokens(m.outputTokens)}+${formatCompactTokens(m.reasoningTokens)}`,
          `${m.usedAic.toFixed(1)} ($${(m.usedAic * 0.001).toFixed(2)})`,
          `${m.cctxCalls}`,
        ];
      });
      lines.push(renderAsciiBoxTable(
        '🤖 Active LLM Models & Session Token Usage (Recent 12h Window)',
        ['Model (Effort)', 'Sess/Turns', 'Input Tok', 'Prompt Cache', 'Out+Rsn Tok', 'Used AIC (@$0.001)', 'CCTX Calls'],
        modelRows,
        [false, true, true, true, true, true, true],
      ));
    }
    const opEntries = Object.entries(this.byOperation);
    if (opEntries.length > 0) {
      lines.push('');
      const opRows = opEntries.map(([opName, s]) => [
        opName,
        `${s.calls}`,
        `${s.successes}`,
        `${s.cacheMisses}`,
        `~${formatCompactTokens(s.deliveredTokens)}`,
        `~${formatCompactTokens(s.savedTokens)}`,
        s.timedCalls > 0 ? `${(s.totalMs / s.timedCalls).toFixed(2)}ms` : '-',
      ]);
      lines.push(renderAsciiBoxTable(
        '⚡ Extension Host Native Tool Breakdown',
        ['Operation', 'Calls', 'Success', 'Cache Miss', 'Delivered Tok', 'Saved Tok', 'Avg Latency'],
        opRows,
        [false, true, true, true, true, true, true],
      ));
    }
    return lines.join('\n');
  }

  public renderDashboardHtml(daemonStatus?: unknown): string {
    const bu = this.backendUsage;
    const ds = asObj(daemonStatus) ?? bu?.rawDaemon;
    const indexState = typeof ds?.index_state === 'string' ? ds.index_state : (bu?.daemonState ?? 'ready');
    const cache = asObj(ds?.cache);
    const adm = asObj(ds?.daemon_admission);
    const reg = asObj(ds?.registry_admission);
    const hits = asOptionalFiniteNumber(cache?.hits) ?? bu?.daemonCacheHits ?? 0;
    const misses = asOptionalFiniteNumber(cache?.misses) ?? bu?.daemonCacheMisses ?? 0;
    const dHitPct = hits + misses > 0 ? `${((100 * hits) / (hits + misses)).toFixed(1)}%` : '-';
    const loadedRoots = asOptionalFiniteNumber(adm?.loaded_roots) ?? bu?.daemonLoadedRoots ?? 0;
    const regRoots = asOptionalFiniteNumber(reg?.registered_roots) ?? bu?.daemonRegisteredRoots ?? 0;
    const ramMib = (((asOptionalFiniteNumber(adm?.estimated_bytes) ?? bu?.daemonEstimatedBytes ?? 0)) / (1024 * 1024)).toFixed(1);

    const style = `
      :root { color-scheme: light dark; }
      body {
        font-family: var(--vscode-font-family, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif);
        color: var(--vscode-foreground);
        background: var(--vscode-editor-background);
        margin: 0;
        padding: 24px 28px 40px;
        line-height: 1.45;
      }
      .header { display: flex; flex-wrap: wrap; justify-content: space-between; align-items: baseline; gap: 12px; margin-bottom: 18px; border-bottom: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.25)); padding-bottom: 12px; }
      .title { font-size: 18px; font-weight: 600; margin: 0; }
      .subtitle { font-size: 12px; color: var(--vscode-descriptionForeground); }
      .cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 12px; margin-bottom: 22px; }
      .card { background: var(--vscode-editorWidget-background, rgba(128,128,128,0.08)); border: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.22)); border-radius: 8px; padding: 14px 16px; }
      .card-label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--vscode-descriptionForeground); margin-bottom: 4px; }
      .card-value { font-size: 22px; font-weight: 700; color: var(--vscode-charts-green, #3fb950); }
      .card-value.neutral { color: var(--vscode-foreground); }
      .card-sub { font-size: 12px; color: var(--vscode-descriptionForeground); margin-top: 4px; }
      h2 { font-size: 14px; font-weight: 600; margin: 22px 0 8px; display: flex; align-items: center; gap: 8px; }
      table { width: 100%; border-collapse: collapse; font-size: 12.5px; margin-bottom: 16px; }
      th, td { padding: 8px 10px; border-bottom: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.18)); text-align: right; white-space: nowrap; }
      th:first-child, td:first-child { text-align: left; white-space: normal; }
      th { font-size: 11px; font-weight: 600; color: var(--vscode-descriptionForeground); background: var(--vscode-editorWidget-background, rgba(128,128,128,0.06)); }
      .tag { display: inline-block; font-size: 11px; padding: 1px 6px; border-radius: 4px; background: rgba(128,128,128,0.18); color: var(--vscode-descriptionForeground); margin-left: 6px; }
      .warn { color: var(--vscode-charts-yellow, #d29922); font-weight: 600; }
      .good { color: var(--vscode-charts-green, #3fb950); font-weight: 600; }
      .advisory-list { margin: 6px 0 16px; padding-left: 18px; font-size: 12.5px; }
      .advisory-list li { margin-bottom: 4px; }
      .footer { margin-top: 20px; font-size: 12px; color: var(--vscode-descriptionForeground); border-top: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.2)); padding-top: 10px; }
      code { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; }
    `;

    if (!bu) {
      const pre = escapeHtml(this.formatDashboard(daemonStatus));
      return `<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline';"><style>${style} pre { font-family: var(--vscode-editor-font-family, monospace); font-size: 12px; overflow-x: auto; }</style></head><body><div class="header"><h1 class="title">Code Context — Dashboard</h1></div><pre>${pre}</pre></body></html>`;
    }

    const { sCount, cCount, pCount, eCount, savedTok, savedAic, savedSecs } = this.effectiveBackendTotals(bu);
    const totalCalls = sCount + cCount + pCount + eCount;
    const costBase = bu.usedAic + savedAic;
    const costPct = costBase > 0 ? `-${((100 * savedAic) / costBase).toFixed(1)}%` : '0%';
    const warmHitPct = bu.warmHitPercent !== undefined
      ? `${bu.warmHitPercent.toFixed(1)}%`
      : (sCount + cCount) > 0
        ? `${((100 * sCount) / (sCount + cCount)).toFixed(1)}%`
        : '-';
    const adoptStr = bu.selectionPercent !== undefined ? `${bu.selectionPercent.toFixed(1)}%` : '-';
    const latStr = formatSmartMs(this.effectiveLatencyMs());
    const cachePct = bu.promptCacheHitPercent !== undefined
      ? `${bu.promptCacheHitPercent.toFixed(1)}%`
      : bu.inputTokens > 0
        ? `${((100 * bu.cacheReadTokens) / bu.inputTokens).toFixed(1)}%`
        : '-';
    const freshAndWrite = Math.max(0, bu.uncachedInputTokens + bu.cacheWriteTokens);

    const activeModels = bu.models.filter(
      (m) => m.inputTokens > 0 || m.outputTokens > 0 || m.usedAic > 0 || m.cctxCalls > 0,
    );
    const modelRowsHtml = activeModels.map((m) => {
      const mCache = m.inputTokens > 0 ? `${((100 * m.cacheReadTokens) / m.inputTokens).toFixed(1)}%` : '-';
      const effLabel = m.effort && m.effort !== '-' ? `<span class="tag">${escapeHtml(m.effort === 'claude-agent' ? 'agent' : m.effort)}</span>` : '';
      const lowAdopt = m.adoptionPercent !== undefined && m.adoptionPercent < 50 && m.lookupCalls >= 5;
      const adoptCell = m.adoptionPercent !== undefined
        ? `<span class="${lowAdopt ? 'warn' : ''}">${lowAdopt ? '⚠ ' : ''}${Math.round(m.adoptionPercent)}%</span> (${m.cctxCalls.toLocaleString('en-US')}/${m.lookupCalls.toLocaleString('en-US')})`
        : `${m.cctxCalls.toLocaleString('en-US')}`;
      return `<tr>
        <td><strong>${escapeHtml(m.displayName)}</strong>${effLabel}</td>
        <td>${m.sessions}/${m.turns.toLocaleString('en-US')}</td>
        <td>${escapeHtml(formatCompactTokens(m.inputTokens))} (${mCache})</td>
        <td>${escapeHtml(formatCompactTokens(m.outputTokens))} + ${escapeHtml(formatCompactTokens(m.reasoningTokens))}</td>
        <td>$${(m.usedAic * 0.001).toFixed(2)}</td>
        <td class="good">+$${(m.savedAic * 0.001).toFixed(2)}</td>
        <td>${m.savedSecs >= 1 ? `~${escapeHtml(formatCompactDuration(m.savedSecs))}` : '-'}</td>
        <td>${adoptCell}</td>
      </tr>`;
    }).join('');

    const toolRowsHtml = bu.tools.map((t) => {
      const errCnt = t.policyDenied + t.errors;
      return `<tr>
        <td><code>${escapeHtml(t.operation)}</code></td>
        <td>${t.calls.toLocaleString('en-US')}</td>
        <td>${t.successes.toLocaleString('en-US')}</td>
        <td>${t.cacheMisses > 0 ? `<span class="warn">${t.cacheMisses}</span>` : '0'}</td>
        <td>${errCnt > 0 ? `<span class="warn">${errCnt}</span>` : '0'}</td>
        <td>~${escapeHtml(formatCompactTokens(t.avgPayloadTokens))} tok</td>
        <td>${escapeHtml(formatSmartMs(t.medianMs))}</td>
        <td>${escapeHtml(formatSmartMs(t.p95Ms))}</td>
      </tr>`;
    }).join('');

    const wsRowsHtml = bu.workspaces.slice(0, 12).map((w) => {
      const low = w.adoptionPercent < 60 && (w.cctx + w.builtin) >= 5;
      return `<tr>
        <td><code>${escapeHtml(w.workspace)}</code></td>
        <td>${w.cctx.toLocaleString('en-US')}</td>
        <td>${w.builtin.toLocaleString('en-US')}</td>
        <td class="${low ? 'warn' : 'good'}">${low ? '⚠ ' : ''}${w.adoptionPercent.toFixed(1)}%</td>
      </tr>`;
    }).join('');

    const advisoriesHtml = bu.advisories.length > 0
      ? `<h2>Optimization Advisories</h2><ul class="advisory-list">${bu.advisories.slice(0, 8).map((a) => `<li><code>${escapeHtml(a.code)}</code> — ${escapeHtml(a.message)}</li>`).join('')}</ul>`
      : '';

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline';">
  <title>Code Context — ${Math.round(bu.hours)}h Dashboard</title>
  <style>${style}</style>
</head>
<body>
  <div class="header">
    <div>
      <h1 class="title">Code Context — Unified ${Math.round(bu.hours)}h Efficiency Dashboard</h1>
      <div class="subtitle">${bu.sessions} sessions · ${bu.turns.toLocaleString('en-US')} turns · Input ${escapeHtml(formatShortTokens(bu.inputTokens))} (Cache ${cachePct} · Fresh+Write ${escapeHtml(formatShortTokens(freshAndWrite))})</div>
    </div>
    <div class="subtitle">Daemon <code>${escapeHtml(indexState)}</code> · ${loadedRoots}/${regRoots} roots (${ramMib} MiB) · Cache hit ${dHitPct} · Updated ${escapeHtml(formatLocalUpdated(bu.updatedAt))}</div>
  </div>

  <div class="cards">
    <div class="card">
      <div class="card-label">Cost Saved (@$0.001/AIC)</div>
      <div class="card-value">+$${(savedAic * 0.001).toFixed(2)} <span style="font-size:14px;font-weight:600">(${costPct})</span></div>
      <div class="card-sub">With cctx $${(bu.usedAic * 0.001).toFixed(2)} · API list ref +$${(savedAic * 0.01).toFixed(2)}</div>
    </div>
    <div class="card">
      <div class="card-label">Time Saved (Est.)</div>
      <div class="card-value">~${escapeHtml(formatCompactDuration(savedSecs))}</div>
      <div class="card-sub">Turns ~${escapeHtml(formatCompactDuration(bu.turnTimeSavedSecs))} · Prefill ~${escapeHtml(formatCompactDuration(bu.prefillTimeSavedSecs))} · Index ~${escapeHtml(formatCompactDuration(bu.ioTimeSavedSecs))}</div>
    </div>
    <div class="card">
      <div class="card-label">Tokens Avoided</div>
      <div class="card-value">~${escapeHtml(formatShortTokens(savedTok))} tok</div>
      <div class="card-sub">Direct ${escapeHtml(formatShortTokens(bu.directTokensSaved))} · Cache re-read ${escapeHtml(formatShortTokens(bu.compoundedTokensSaved))} · Turns ${escapeHtml(formatShortTokens(bu.turnTokensSaved))}</div>
    </div>
    <div class="card">
      <div class="card-label">Adoption &amp; Speed</div>
      <div class="card-value neutral">${escapeHtml(adoptStr)} · ${escapeHtml(latStr)}</div>
      <div class="card-sub">Warm hit ${warmHitPct} (${sCount.toLocaleString('en-US')}/${totalCalls.toLocaleString('en-US')} calls) · Avoided PRs ~${bu.avoidedPremiumRequests.toFixed(0)}</div>
    </div>
  </div>

  ${activeModels.length > 0 ? `
  <h2>Active LLM Models (${Math.round(bu.hours)}h)</h2>
  <table>
    <thead>
      <tr><th>Model</th><th>Sess/Turns</th><th>Input (Cache%)</th><th>Out + Rsn</th><th>Spend</th><th>Saved</th><th>Time Saved</th><th>CCTX Adoption</th></tr>
    </thead>
    <tbody>${modelRowsHtml}</tbody>
  </table>` : ''}

  ${bu.tools.length > 0 ? `
  <h2>Unified CCTX Tool Breakdown (${Math.round(bu.hours)}h MCP + Native)</h2>
  <table>
    <thead>
      <tr><th>Operation</th><th>Calls</th><th>Success</th><th>Cache Miss</th><th>Err/Deny</th><th>Avg Payload</th><th>Median (p50)</th><th>P95</th></tr>
    </thead>
    <tbody>${toolRowsHtml}</tbody>
  </table>` : ''}

  ${bu.workspaces.length > 0 ? `
  <h2>Workspace Source Lookup Adoption</h2>
  <table>
    <thead>
      <tr><th>Workspace</th><th>CCTX Lookups</th><th>Built-in / Shell Fallbacks</th><th>Adoption</th></tr>
    </thead>
    <tbody>${wsRowsHtml}</tbody>
  </table>` : ''}

  ${advisoriesHtml}

  <div class="footer">
    Modeled estimates across GitHub Copilot, Claude Code, and OpenAI Codex (@$0.001/AIC internal unit; @$0.01/AIC official GitHub AI Credits reference). Avoided-turn time deduplicates parallel tool bursts within each LLM turn.
  </div>
</body>
</html>`;
  }
}
