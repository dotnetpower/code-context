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

void test('async session telemetry tracks MCP calls, cache misses, and missing-directory cooldown', async () => {
  const tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'cctx-usage-async-'));
  try {
    const usage = new UsageSummary();
    // Missing ~/.copilot/session-state caches the attempt for 60s without throwing
    const missing = await usage.refreshSessionTelemetryAsync(tmpHome, 1_000, false);
    assert.equal(missing, undefined);

    const sessDir = path.join(tmpHome, '.copilot', 'session-state', 'sess-mcp');
    fs.mkdirSync(sessDir, { recursive: true });
    fs.writeFileSync(
      path.join(sessDir, 'events.jsonl'),
      [
        JSON.stringify({
          type: 'session.usage_record',
          data: { usage: { model: 'gemini-4-carbon', modelDisplayName: 'Gemini 4 Carbon', inputTokens: 50_000 } },
        }),
        JSON.stringify({
          type: 'tool.execution_start',
          data: { toolCallId: 'c1', toolName: 'code-context-code_read', model: 'gemini-4-carbon' },
        }),
        JSON.stringify({
          type: 'tool.execution_complete',
          data: { toolCallId: 'c1', result: { content: 'CACHE_MISS root=/repo state=queued' } },
        }),
        JSON.stringify({
          type: 'tool.execution_start',
          data: { toolCallId: 'c2', toolName: 'mcp__code-context__code_context', model: 'gemini-4-carbon' },
        }),
      ].join('\n'),
      'utf8',
    );
    // Within 60s cooldown without force=true, returns cached undefined
    assert.equal(await usage.refreshSessionTelemetryAsync(tmpHome, 10_000, false), undefined);
    // After 60s (or with force=true), parses MCP calls + CACHE_MISS warm-hit pct (1/2 = 50.0%)
    const snap = await usage.refreshSessionTelemetryAsync(tmpHome, 70_000, false);
    assert.ok(snap);
    assert.equal(snap.models[0]?.cctxCalls, 2);
    assert.equal(snap.models[0]?.cctxWarmHitPct, 50);
  } finally {
    fs.rmSync(tmpHome, { recursive: true, force: true });
  }
});

void test('session telemetry ingests Claude 1h cache writes and Codex token_count sessions', () => {
  const tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'cctx-usage-multi-'));
  try {
    const claudeDir = path.join(tmpHome, '.claude', 'projects', 'repo');
    const codexDir = path.join(tmpHome, '.codex', 'sessions', '2026', '11', '18');
    fs.mkdirSync(claudeDir, { recursive: true });
    fs.mkdirSync(codexDir, { recursive: true });
    fs.writeFileSync(
      path.join(claudeDir, 'c1.jsonl'),
      JSON.stringify({
        type: 'assistant',
        message: {
          model: 'claude-opus-4-7',
          usage: {
            input_tokens: 1000,
            cache_creation_input_tokens: 20000,
            cache_creation: { ephemeral_1h_input_tokens: 20000 },
            cache_read_input_tokens: 79000,
            output_tokens: 500,
          },
          content: [{ type: 'tool_use', name: 'mcp__code-context__code_context' }],
        },
      }) + '\n',
      'utf8',
    );
    fs.writeFileSync(
      path.join(codexDir, 'rollout-1.jsonl'),
      [
        JSON.stringify({ type: 'turn_context', payload: { model: 'gpt-6.1-sol', effort: 'high' } }),
        JSON.stringify({
          type: 'event_msg',
          payload: {
            type: 'token_count',
            info: {
              total_token_usage: { input_tokens: 50000, cached_input_tokens: 45000, cache_write_input_tokens: 3000, output_tokens: 400, reasoning_output_tokens: 200 },
              last_token_usage: { input_tokens: 50000, cached_input_tokens: 45000, cache_write_input_tokens: 3000, output_tokens: 400, reasoning_output_tokens: 200 },
            },
          },
        }),
      ].join('\n') + '\n',
      'utf8',
    );
    const usage = new UsageSummary();
    const snap = usage.refreshSessionTelemetry(tmpHome, Date.now(), true);
    assert.ok(snap);
    assert.equal(snap.sessions, 2);
    assert.equal(snap.cacheWriteTokens, 23000);
    assert.match(usage.markdownTooltip(), /GitHub AI Credits ref/);
    assert.match(usage.markdownTooltip(), /Fresh\+Write/);
  } finally {
    fs.rmSync(tmpHome, { recursive: true, force: true });
  }
});

void test('backend code_usage snapshot unifies 24h MCP and Native telemetry across WSL and Windows', async () => {
  const usage = new UsageSummary();
  // Simulate a cold Extension Host startup probe (`code_workspaces`, 125.67ms, 0 tokens saved)
  usage.record({ operation: 'code_workspaces', root: null, totalMs: 125.67 }, 1_700_000_000_000);

  let backendCalls = 0;
  usage.setBackendQuery((hours, force) => {
    backendCalls += 1;
    assert.equal(hours, 24);
    assert.equal(force, true);
    return Promise.resolve({
      hours: 24,
      sessions: 23,
      daemon: {
        ready: true,
        index_state: 'ready',
        cache: { state: 'ready', hits: 2637, misses: 4 },
        daemon_admission: { loaded_roots: 3, estimated_bytes: 42 * 1024 * 1024 },
        registry_admission: { registered_roots: 11 },
        recent_source_avg_ms: 0.85,
      },
      token_summary: {
        total_turns: 3504,
        input_tokens: 820_000_000,
        uncached_input_tokens: 4_300_000,
        cache_write_tokens: 22_000_000,
        cache_read_tokens: 793_700_000,
        prompt_cache_hit_percent: 96.8,
        output_tokens: 3_100_000,
        reasoning_tokens: 900_000,
        used_aic: 16888.2,
        saved_aic: 13290.4,
        estimated_total_tokens_saved: 645_100_000,
        direct_tokens_saved: 42_000_000,
        compounded_history_tokens_saved: 560_000_000,
        estimated_turn_tokens_saved: 38_100_000,
        turn_compression_calls: 612,
        bpe_formula_tokens_saved: 5_000_000,
        cctx_selection_percent: 95.3,
        cctx_warm_cache_hit_percent: 99.8,
        cost_savings_percent: 44.0,
        estimated_time_saved_secs: 6180.0,
        avoided_turn_time_saved_secs: 4380.0,
        lookup_io_time_saved_secs: 150.0,
        prefill_time_saved_secs: 1650.0,
        effective_avoided_turns: 612.0,
        avoided_premium_requests_est: 912.5,
      },
      model_usage: [
        {
          model: 'claude-opus-4.7',
          display_name: 'Claude Opus 4.7',
          effort: 'xhigh',
          sessions: 15,
          turns: 2100,
          input_tokens: 610_000_000,
          cache_read_tokens: 592_000_000,
          cache_write_tokens: 15_000_000,
          output_tokens: 2_200_000,
          reasoning_tokens: 700_000,
          used_aic: 14120.5,
          saved_aic: 11450.2,
          saved_secs: 4920.0,
          estimated_total_tokens_saved: 520_000_000,
          cctx_calls: 1920,
          lookup_calls: 2010,
          cctx_adoption_percent: 95.5,
        },
        {
          model: 'gpt-5.4',
          display_name: 'GPT-5.4',
          effort: 'high',
          sessions: 8,
          turns: 1404,
          input_tokens: 210_000_000,
          cache_read_tokens: 201_700_000,
          cache_write_tokens: 7_000_000,
          output_tokens: 900_000,
          reasoning_tokens: 200_000,
          used_aic: 2767.7,
          saved_aic: 1840.2,
          saved_secs: 1260.0,
          estimated_total_tokens_saved: 125_100_000,
          cctx_calls: 73,
          lookup_calls: 610,
          cctx_adoption_percent: 12.0,
        },
        {
          model: 'bori-pure-rust',
          display_name: 'Bori Pure-Rust (0 Token · <5ms)',
          effort: '-',
          sessions: 1,
          turns: 4,
          input_tokens: 0,
          cache_read_tokens: 0,
          cache_write_tokens: 0,
          output_tokens: 0,
          reasoning_tokens: 0,
          used_aic: 0,
          saved_aic: 0,
          saved_secs: 0,
          estimated_total_tokens_saved: 0,
          cctx_calls: 0,
          lookup_calls: 0,
        },
      ],
      insights: {
        tool_reliability: [
          {
            tool: 'code-context-code_read',
            calls: 1300,
            failures: 6,
            failure_causes: { cache_miss: 2, invalid_input: 4 },
            median_ms: 0.68,
            p95_ms: 2.1,
            avg_payload_tokens: 920,
          },
          {
            tool: 'cctxNativeRead',
            calls: 50,
            failures: 0,
            failure_causes: {},
            median_ms: 1.12,
            p95_ms: 3.4,
            avg_payload_tokens: 800,
          },
          {
            tool: 'code-context-code_context',
            calls: 980,
            failures: 2,
            failure_causes: { cache_miss: 2 },
            median_ms: 0.76,
            p95_ms: 2.9,
            avg_payload_tokens: 1350,
          },
          {
            tool: 'code-context-code_grep',
            calls: 323,
            failures: 4,
            failure_causes: { policy_denied: 1, invalid_input: 3 },
            median_ms: 0.71,
            p95_ms: 2.4,
            avg_payload_tokens: 610,
          },
          {
            tool: 'code-context-code_status',
            calls: 3,
            failures: 0,
            failure_causes: {},
            median_ms: 188.0,
            p95_ms: 213.0,
            avg_payload_tokens: 2100,
          },
        ],
      },
    });
  });

  await usage.refreshSessionTelemetryAsync(undefined, 1_700_000_005_000, true);
  assert.equal(backendCalls, 1);
  assert.equal(usage.hasRenderableData, true);

  // Status bar shows concise Cost + Time + Tokens + Adoption instead of cryptic S/C/P/E codes
  const bar = usage.text();
  assert.equal(bar, '$(database) CCTX +$13.29 · ~1h 43m · 645M tok · 95%');

  // Hover tooltip shows 4-pillar Cost/Time/Tokens/Adopt+Speed, filters 0-token noise rows, flags <50% adoption, and folds admin ops
  const md = usage.markdownTooltip();
  assert.match(md, /Time Saved/);
  assert.match(md, /\*\*~1h 43m\*\*/);
  assert.match(md, /\*\*\+\$13\.29\*\* \(`-44\.0%`\)/);
  assert.match(md, /\*\*95%\*\* · \*\*0\.7ms\*\*/);
  assert.doesNotMatch(md, /125\.7ms|125\.67/);
  assert.match(md, /Claude Opus 4\.7.*~1h 22m.*96% \(1,920\)/);
  assert.match(md, /GPT-5\.4.*~21m.*\$\(warning\) \*\*12%\*\* \(73\)/);
  assert.doesNotMatch(md, /Bori Pure-Rust/, '0-token non-LLM rows must be hidden from hover tooltip');
  // `code-context-code_read` (1300) + `cctxNativeRead` (50) merged into `code_read` (1,350) with readable miss/err badges
  assert.match(md, /`code_read` \| 1,350 \(`miss 2, err 4`\)/);
  assert.match(md, /_Admin ops: code_status 3_/);
  assert.doesNotMatch(md, /ExtHost bridge:/, 'bridge counters are hidden when bridge errors are zero');

  // Full ASCII dashboard retains complete detail including TIME SAVED and all models/tools
  const dash = usage.formatDashboard({
    index_state: 'ready',
    cache: { hits: 2637, misses: 4 },
    daemon_admission: { loaded_roots: 3, estimated_bytes: 42 * 1024 * 1024 },
    registry_admission: { registered_roots: 11 },
  });
  assert.match(dash, /UNIFIED 24H MODEL, TIME, TOKEN & COST SAVINGS DASHBOARD/);
  assert.match(dash, /TIME SAVED \(EST\.\)\s*:\s*~1h 43m/);
  assert.match(dash, /\+\$13\.29 USD/);
  assert.match(dash, /Claude Opus 4\.7 \(xhigh\)/);
  assert.match(dash, /Unified CCTX Tool Breakdown \(24h MCP \+ Native\)/);
});

void test('backend query failure clears stale backend snapshot and falls back hermetically to local telemetry', async () => {
  const tmpHome = fs.mkdtempSync(path.join(os.tmpdir(), 'cctx-usage-fallback-'));
  try {
    const sessDir = path.join(tmpHome, '.copilot', 'session-state', 's-local');
    fs.mkdirSync(sessDir, { recursive: true });
    const nowIso = new Date().toISOString();
    fs.writeFileSync(
      path.join(sessDir, 'events.jsonl'),
      JSON.stringify({
        type: 'assistant.usage',
        timestamp: nowIso,
        data: { model: 'claude-sonnet-4.6', inputTokens: 40_000, cacheReadTokens: 35_000, outputTokens: 500 },
      }) + '\n',
      'utf8',
    );
    const usage = new UsageSummary();
    // Seed a prior backend snapshot first, then verify a failing backend refresh clears it and surfaces local `s-local`.
    usage.ingestBackendPayload({
      hours: 24,
      sessions: 99,
      token_summary: { total_turns: 999, saved_aic: 50, estimated_total_tokens_saved: 100_000 },
      model_usage: [],
    });
    assert.ok(usage.backendUsage);

    usage.setBackendQuery(() => Promise.reject(new Error('unknown operation: code_usage')));
    const snap = await usage.refreshSessionTelemetryAsync(tmpHome, Date.now(), true);
    assert.ok(snap);
    assert.equal(snap.sessions, 1, 'must hermetically read only tmpHome without leaking os.homedir()');
    assert.equal(snap.turns, 1, 'assistant.usage must be parsed in local fallback');
    assert.equal(snap.models[0]?.model, 'claude-sonnet-4.6');
    assert.equal(usage.backendUsage, undefined, 'stale backend snapshot must be cleared on fallback');
    assert.match(usage.markdownTooltip(), /claude-sonnet-4\.6/);

    // Also verify cached snapshot watermark (`computed_at_unix_ms`) never drops local calls recorded after T0,
    // and invalid backend payloads clear both backendUsage and backend-derived sessionTelemetry on an empty home.
    const emptyHome = fs.mkdtempSync(path.join(os.tmpdir(), 'cctx-usage-empty-'));
    try {
      const cachedPayload = {
        computed_at_unix_ms: 1_700_000_010_000,
        hours: 24,
        sessions: 2,
        token_summary: { total_turns: 10, saved_aic: 20, estimated_total_tokens_saved: 5_000 },
        model_usage: [{ model: 'gpt-5.4', display_name: 'GPT-5.4', effort: 'high', sessions: 2, turns: 10 }],
        insights: {
          cctx_operations: [{ operation: 'code_read', calls: 10, failures: 0, failure_causes: {}, avg_payload_tokens: 200, median_ms: 0.5 }],
        },
      };
      usage.ingestBackendPayload(cachedPayload, 1_700_000_010_000);
      assert.match(usage.tooltip(), /OK 10,/);
      usage.record({ operation: 'code_read', root: '/repo', totalMs: 0.6 }, 1_700_000_012_000);
      assert.match(usage.tooltip(), /OK 11,/);
      // Re-ingesting the same 15s-cached payload (computed_at_unix_ms = 1_700_000_010_000 < 1_700_000_012_000) keeps OK 11!
      usage.ingestBackendPayload(cachedPayload, 1_700_000_015_000);
      assert.match(usage.tooltip(), /OK 11,/);

      // Invalid payload on an empty local home clears both backendUsage and stale backend sessionTelemetry.
      usage.setBackendQuery(() => Promise.resolve({ malformed: true }));
      const cleared = await usage.refreshSessionTelemetryAsync(emptyHome, 1_700_000_080_000, true);
      assert.equal(cleared, undefined);
      assert.equal(usage.backendUsage, undefined);
    } finally {
      fs.rmSync(emptyHome, { recursive: true, force: true });
    }
  } finally {
    fs.rmSync(tmpHome, { recursive: true, force: true });
  }
});
