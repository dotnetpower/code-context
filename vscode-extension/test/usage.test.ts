import assert from 'node:assert/strict';
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
  usage.record({ operation: 'code_workspaces', root: null }, 4000);
  assert.equal(usage.lastRoot, '/project');
});
