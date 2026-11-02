import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import test from 'node:test';
import { classifyOutcome, UsageSummary } from '../src/usage.js';

void test('usage distinguishes unavailable caches from refusals and actual failures', () => {
  assert.equal(classifyOutcome(), 'success');
  assert.equal(classifyOutcome('CACHE_MISS root=/project'), 'cache_not_ready');
  assert.equal(classifyOutcome('TARGET_NOT_INDEXED reason=not_yet_indexed'), 'cache_not_ready');
  assert.equal(classifyOutcome('TARGET_NOT_INDEXED reason=excluded_by_rule'), 'policy_denied');
  assert.equal(classifyOutcome('READ_POLICY_DENIED reason=unsupported_glob'), 'policy_denied');
  assert.equal(classifyOutcome('READ_RANGE_OUT_OF_BOUNDS'), 'invalid_request');
  assert.equal(classifyOutcome('READ_RANGE_LIMIT: provide 1..64'), 'invalid_request');
  assert.equal(classifyOutcome('RESPONSE_BUDGET_EXCEEDED: response exceeds 256 KiB'), 'invalid_request');
  assert.equal(classifyOutcome('CACHE_LOAD_FAILED: admission'), 'system_error');
  assert.equal(classifyOutcome('Windows Agent Sessions require an explicit absolute Linux root'), 'invalid_request');
  assert.equal(classifyOutcome('WSL Code Context root must be an absolute Linux path.'), 'invalid_request');
});

void test('status is event driven, bounded and identifies native-host rather than global usage', () => {
  const usage = new UsageSummary();
  assert.equal(usage.last, undefined);
  usage.record({ operation: 'code_context', root: '/project' }, 1000);
  usage.record({ operation: 'code_read', root: '/project', error: 'CACHE_MISS root=/project' }, 2000);
  usage.record({ operation: 'code_read', root: '/project', error: 'READ_POLICY_DENIED' }, 3000);
  assert.equal(usage.counts.success, 1);
  assert.equal(usage.counts.cache_not_ready, 1);
  assert.equal(usage.counts.policy_denied, 1);
  assert.match(usage.text(), /CCTX N S:1 C:1 P:1 E:0/);
  assert.match(usage.tooltip(), /not all sessions/);
  assert.match(usage.tooltip(), /1970-01-01T00:00:03.000Z/);
  assert.equal(usage.lastRoot, '/project');
  usage.record({ operation: 'code_workspaces', root: null, totalMs: 1.5 }, 4000);
  assert.equal(usage.lastRoot, '/project');
  assert.equal(usage.avgLatencyMs, 1.5);
});

void test('usage aggregates session model telemetry, markdown tooltip, and status dashboard', () => {
  const tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'cctx-usage-test-'));
  try {
    const sessDir = path.join(tmpHome, '.copilot', 'session-state', 'sess-1');
    fs.mkdirSync(sessDir, { recursive: true });
    const nowMs = 1_800_000_000_000;
    fs.writeFileSync(
      path.join(sessDir, 'events.jsonl'),
      [
        JSON.stringify({
          type: 'session.usage_record',
          data: {
            usage: {
              model: 'gemini-4-argon',
              modelDisplayName: 'Gemini 4 Argon',
              reasoningEffort: 'high',
              inputTokens: 120_000,
              cacheReadTokens: 114_000,
              outputTokens: 1_200,
              reasoningTokens: 600,
            },
          },
        }),
        JSON.stringify({
          type: 'tool.execution_start',
          data: { toolName: 'cctxNativeContext', model: 'gemini-4-argon' },
        }),
      ].join('\n'),
      'utf8',
    );
    const usage = new UsageSummary();
    usage.record({
      operation: 'code_context',
      root: '/project',
      totalMs: 0.42,
      deliveredChars: 1600,
      turnCompressed: true,
    }, nowMs);
    const snap = usage.refreshSessionTelemetry(tmpHome, Date.now(), true);
    assert.ok(snap);
    assert.equal(snap.models.length, 1);
    assert.equal(snap.models[0]?.displayName, 'Gemini 4 Argon');
    assert.equal(snap.models[0]?.cctxCalls, 1);
    assert.equal(usage.deliveredTokens, 400);
    assert.equal(usage.turnCompressionCount, 1);
    assert.equal(usage.totalEstimatedTokensSaved, 1650);
    assert.match(usage.text(), /CCTX N S:1 C:0 P:0 E:0 · Gemini 4 Argon · ~1\.[67]K tok/);
    const md = usage.markdownTooltip();
    assert.match(md, /Gemini 4 Argon/);
    assert.match(md, /95\.0%/);
    assert.match(md, /code_context/);
    const dash = usage.formatDashboard({
      index_state: 'loaded',
      cache: { hits: 98, misses: 2 },
      daemon_admission: { loaded_roots: 2, estimated_bytes: 10 * 1024 * 1024 },
      registry_admission: { registered_roots: 5 },
    });
    assert.match(dash, /Active LLM Models & Session Token Usage/);
    assert.match(dash, /Gemini 4 Argon \(high\)/);
    assert.match(dash, /98\.0% \(98\/100\)/);
  } finally {
    fs.rmSync(tmpHome, { recursive: true, force: true });
  }
});
