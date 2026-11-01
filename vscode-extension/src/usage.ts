export type Outcome = 'success' | 'cache_not_ready' | 'policy_denied' | 'invalid_request' | 'system_error';

export interface UsageEvent {
  operation: string;
  root: string | null;
  error?: string;
  totalMs?: number;
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
    /READ_RANGE_OUT_OF_BOUNDS|READ_RANGE_LIMIT|READ_PLAN_TOO_LARGE|READ_PAGE_LIMIT|RESPONSE_BUDGET_EXCEEDED|reason=missing_on_disk|regex parse error|limit 1\.\.64|invalid range|unknown field|Windows Agent Sessions require|WSL Code Context root|No registered Code Context workspace|root must be an absolute path/u.test(error)
  ) {
    return 'invalid_request';
  }
  return 'system_error';
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

  public record(event: UsageEvent, at = Date.now()): void {
    const outcome = classifyOutcome(event.error);
    this.counts[outcome] += 1;
    if (typeof event.totalMs === 'number' && Number.isFinite(event.totalMs) && event.totalMs >= 0) {
      this.totalMsSum += event.totalMs;
      this.timedCalls += 1;
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

  public get avgLatencyMs(): number | undefined {
    return this.timedCalls > 0 ? this.totalMsSum / this.timedCalls : undefined;
  }

  public text(): string {
    const counts = this.counts;
    return `$(database) CCTX N S:${counts.success} C:${counts.cache_not_ready} P:${counts.policy_denied} E:${counts.invalid_request + counts.system_error}`;
  }

  public tooltip(): string {
    const c = this.counts;
    const multiRoots =
      this.recentRoots.length > 1
        ? `Recent roots (${this.recentRoots.length}): ${this.recentRoots.join(', ')}`
        : this.lastRoot
          ? `Last requested root: ${this.lastRoot}`
          : 'Pass an explicit Linux root in Windows Agent Sessions.';
    return [
      'Native calls in this extension host (including metadata), not all sessions.',
      `Success ${c.success}; cache not ready ${c.cache_not_ready}; policy denied ${c.policy_denied}; invalid input ${c.invalid_request}; system errors ${c.system_error}.`,
      this.last ? `Last: ${new Date(this.last.at).toISOString()} ${this.last.operation} ${this.last.outcome}` : 'No calls observed yet.',
      multiRoots,
      'Click for daemon status, adaptive memory and aggregate native/MCP outcomes.',
    ].join('\n');
  }
}
