import assert from 'node:assert/strict';
import test from 'node:test';
import { formatLogDetails, formatTrace, outcomeLevel, singleLine } from '../src/logFormatter.js';
import type { ClientTrace } from '../src/daemonClient.js';

void test('trace logs show readable phases and correlation without JSON or machine memory dumps', () => {
  const trace: ClientTrace = {
    trace_id: 'trace-123', operation: 'code_read', root: '/fixture',
    total_ms: 405, prepare_ms: 400, queue_ms: 1, ipc_ms: 3, serialize_ms: 1,
    write_ms: 0, started_unix_ms: 1, completed_unix_ms: 406,
    success: true, available: true, outcome: 'success',
    daemon: { execute_ms: 2, daemon_request_id: 7 },
    host_memory: { platform: 'win32', total_bytes: 100 },
  };
  const line = formatTrace(trace);
  assert.match(line, /^code_read success total=405\.000ms prepare=400\.000ms/);
  assert.match(line, /daemon=2\.000ms request=7 root=\/fixture trace=trace-123$/);
  assert.ok(!line.includes('{') && !line.includes('host_memory'));
});

void test('expected misses, refusals, invalid input and system failures use distinct severities', () => {
  assert.equal(outcomeLevel('success'), 'info');
  assert.equal(outcomeLevel('cache_not_ready'), 'info');
  assert.equal(outcomeLevel('policy_denied'), 'warn');
  assert.equal(outcomeLevel('invalid_request'), 'warn');
  assert.equal(outcomeLevel('system_error'), 'error');
  assert.equal(singleLine('invalid\npattern\r\t\x1b'), 'invalid\\npattern\\r\\t?');
});

void test('status and self-test retain nested diagnostics in plain text with explicit bounds', () => {
  const result = formatLogDetails('Code Context status', {
    index_state: 'unloaded', stats: null,
    cache: { resident_bytes: 123, roots: ['/one', '/two'] },
  });
  assert.match(result, /index_state: unloaded/);
  assert.match(result, /stats: not available/);
  assert.match(result, /cache\.resident_bytes: 123/);
  assert.match(result, /cache\.roots\[1\]: \/two/);
  assert.ok(!result.includes('{') && !result.includes('"'));
  const bounded = formatLogDetails('Code Context status', Array.from({ length: 300 }, (_, i) => i));
  assert.equal(bounded.split('\n').length, 257);
  assert.match(bounded, /Additional fields omitted.*code_status/);
});
