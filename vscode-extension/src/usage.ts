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
  outputTokens: number;
  reasoningTokens: number;
  usedAic: number;
  models: SessionModelSummary[];
}

export function formatCompactTokens(value: number): string {
  if (!Number.isFinite(value) || value <= 0) { return '0'; }
  if (value >= 1_000_000_000) { return `${(value / 1_000_000_000).toFixed(2)}B`; }
  if (value >= 1_000_000) { return `${(value / 1_000_000).toFixed(2)}M`; }
  if (value >= 1_000) { return `${(value / 1_000).toFixed(1)}K`; }
  return `${Math.round(value)}`;
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
    /READ_RANGE_OUT_OF_BOUNDS|READ_RANGE_LIMIT|READ_PLAN_TOO_LARGE|READ_PAGE_LIMIT|RESPONSE_BUDGET_EXCEEDED|reason=missing_on_disk|regex parse error|limit 1\.\.(64|1000)|provide 1\.\.64 ranges|include\/exclude globs|invalid range|unknown field|Windows Agent Sessions require|WSL Code Context root|No registered Code Context workspace|root must be an absolute path/u.test(error)
  ) {
    return 'invalid_request';
  }
  return 'system_error';
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

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
    const op = this.byOperation[event.operation] ??= {
      calls: 0, successes: 0, cacheMisses: 0, deliveredTokens: 0, savedTokens: 0, totalMs: 0, timedCalls: 0,
    };
    op.calls += 1;
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
      if (typeof event.deliveredChars === 'number' && event.deliveredChars > 0) {
        this.recordPayload(event.operation, event.deliveredChars, false);
      }
      if (event.turnCompressed === true) {
        this.turnCompressionCount += 1;
        this.estimatedTurnTokensSaved += 1200;
      }
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
    if (!force && this.sessionTelemetry && nowMs - this.sessionTelemetry.updatedAt < 10_000) {
      return this.sessionTelemetry;
    }
    try {
      const baseHome = customHome ?? process.env.XDG_STATE_HOME ?? os.homedir();
      const sessionDir = path.join(baseHome, '.copilot', 'session-state');
      if (!fs.existsSync(sessionDir)) { return this.sessionTelemetry; }
      const cutoffMs = nowMs - 12 * 3600 * 1000;
      const candidates: Array<{ file: string; mtimeMs: number; sessionId: string }> = [];
      for (const entry of fs.readdirSync(sessionDir, { withFileTypes: true })) {
        if (!entry.isDirectory()) { continue; }
        const eventsFile = path.join(sessionDir, entry.name, 'events.jsonl');
        try {
          const stat = fs.statSync(eventsFile);
          if (stat.mtimeMs >= cutoffMs) {
            candidates.push({ file: eventsFile, mtimeMs: stat.mtimeMs, sessionId: entry.name });
          }
        } catch {
          // Ignore rotated or inaccessible session files.
        }
      }
      candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);
      const selected = candidates.slice(0, 6);
      const byModel = new Map<string, {
        model: string; displayName: string; effort: string; sessions: Set<string>;
        turns: number; inputTokens: number; cacheReadTokens: number; outputTokens: number;
        reasoningTokens: number; nanoAiu: number; cctxCalls: number; cctxMisses: number;
      }>();
      let totalTurns = 0;
      let totalIn = 0;
      let totalCache = 0;
      let totalOut = 0;
      let totalRsn = 0;
      let totalNanoAiu = 0;
      const activeSessions = new Set<string>();
      for (const item of selected) {
        let raw = '';
        try {
          const stat = fs.statSync(item.file);
          const maxBytes = 512 * 1024;
          if (stat.size <= maxBytes) {
            raw = fs.readFileSync(item.file, 'utf8');
          } else {
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
        } catch {
          continue;
        }
        let lastModel = 'unknown';
        let lineStart = 0;
        while (lineStart < raw.length) {
          const nlIdx = raw.indexOf('\n', lineStart);
          const lineEnd = nlIdx >= 0 ? nlIdx : raw.length;
          const line = raw.slice(lineStart, lineEnd);
          lineStart = lineEnd + 1;
          if (!line.includes('"session.usage_record"') && !line.includes('"tool.execution_')) {
            continue;
          }
          try {
            const ev = JSON.parse(line) as Record<string, unknown>;
            const type = ev.type;
            const data = typeof ev.data === 'object' && ev.data !== null ? ev.data as Record<string, unknown> : undefined;
            if (type === 'session.usage_record' && data) {
              const u = typeof data.usage === 'object' && data.usage !== null ? data.usage as Record<string, unknown> : undefined;
              if (!u) { continue; }
              const model = typeof u.model === 'string' && u.model ? u.model : 'unknown';
              const displayName = typeof u.modelDisplayName === 'string' && u.modelDisplayName ? u.modelDisplayName : model;
              const effort = typeof u.reasoningEffort === 'string' && u.reasoningEffort ? u.reasoningEffort : '-';
              lastModel = model;
              activeSessions.add(item.sessionId);
              const inTok = typeof u.inputTokens === 'number' ? u.inputTokens : 0;
              const crTok = typeof u.cacheReadTokens === 'number' ? u.cacheReadTokens : 0;
              const outTok = typeof u.outputTokens === 'number' ? u.outputTokens : 0;
              const rsnTok = typeof u.reasoningTokens === 'number' ? u.reasoningTokens : 0;
              const cu = typeof u.copilotUsage === 'object' && u.copilotUsage !== null ? u.copilotUsage as Record<string, unknown> : undefined;
              const nanoAiu = typeof cu?.totalNanoAiu === 'number' ? cu.totalNanoAiu : 0;
              totalTurns += 1;
              totalIn += inTok;
              totalCache += crTok;
              totalOut += outTok;
              totalRsn += rsnTok;
              totalNanoAiu += nanoAiu;
              let entry = byModel.get(model);
              if (!entry) {
                entry = {
                  model, displayName, effort, sessions: new Set(),
                  turns: 0, inputTokens: 0, cacheReadTokens: 0, outputTokens: 0, reasoningTokens: 0,
                  nanoAiu: 0, cctxCalls: 0, cctxMisses: 0,
                };
                byModel.set(model, entry);
              }
              entry.sessions.add(item.sessionId);
              entry.turns += 1;
              entry.inputTokens += inTok;
              entry.cacheReadTokens += crTok;
              entry.outputTokens += outTok;
              entry.reasoningTokens += rsnTok;
              entry.nanoAiu += nanoAiu;
            } else if (type === 'tool.execution_start' && data) {
              const toolName = typeof data.toolName === 'string' ? data.toolName : '';
              if (toolName.startsWith('cctxNative') || toolName.startsWith('code-context')) {
                const m = typeof data.model === 'string' && data.model ? data.model : lastModel;
                const entry = byModel.get(m);
                if (entry) { entry.cctxCalls += 1; }
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
          outputTokens: m.outputTokens,
          reasoningTokens: m.reasoningTokens,
          usedAic: Math.round((m.nanoAiu / 1e9) * 100) / 100,
          cctxCalls: m.cctxCalls,
          cctxWarmHitPct: m.cctxCalls > 0 ? Math.round((1000 * (m.cctxCalls - m.cctxMisses)) / m.cctxCalls) / 10 : undefined,
        }));
      this.sessionTelemetry = {
        updatedAt: nowMs,
        sessions: activeSessions.size,
        turns: totalTurns,
        inputTokens: totalIn,
        cacheReadTokens: totalCache,
        outputTokens: totalOut,
        reasoningTokens: totalRsn,
        usedAic: Math.round((totalNanoAiu / 1e9) * 100) / 100,
        models,
      };
    } catch {
      // Ignore file-system telemetry errors in restricted environments.
    }
    return this.sessionTelemetry;
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

  public text(): string {
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
      lines.push(
        `**$(hubot) Active LLM Models** (${st.sessions} sessions · ${st.turns} turns · Spend \`$${(st.usedAic * 0.001).toFixed(2)}\` / \`${st.usedAic.toFixed(1)} AIC\` · Cache \`${cachePct}\`)`,
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
      '[$(pulse) Show Full Dashboard](command:codeContextNative.showStatus) · [$(beaker) Run Self-Test](command:codeContextNative.selfTest)',
    );
    return lines.join('\n');
  }

  public formatDashboard(daemonStatus?: unknown): string {
    this.refreshSessionTelemetry();
    const c = this.counts;
    const avgMs = this.avgLatencyMs !== undefined ? `${this.avgLatencyMs.toFixed(2)}ms` : '-';
    const savedAic = this.estimatedAicSaved;
    const lines: string[] = [
      '╔══════════════════════════════════════════════════════════════════════════════════╗',
      '║  ⚡ CODE CONTEXT NATIVE — MODEL, TOKEN & COST SAVINGS DASHBOARD                  ║',
      '╠══════════════════════════════════════════════════════════════════════════════════╣',
      `║  💰 COST SAVED (@$0.001/AIC) : +$${(savedAic * 0.001).toFixed(2)} USD (~${savedAic.toFixed(1)} AIC saved in this host)              ║`,
      `║  💎 API LIST VALUE (@$0.01)  : +$${(savedAic * 0.01).toFixed(2)} USD (Frontier API list price equivalent)       ║`,
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
}
