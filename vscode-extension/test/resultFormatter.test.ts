import assert from 'node:assert/strict';
import test from 'node:test';

import { formatToolResult, validateDaemonStatus } from '../src/resultFormatter.js';
import type { JsonValue } from '../src/types.js';

const freshness = {
  basis: 'watcher_snapshot',
  generation: 1,
  returned_file_check: 'metadata',
  full_disk_verified: false,
  notice: 'Verify recent edits with code_read or a host policy-aware live disk read; use code_sync before claiming absence.',
};

void test('paged reads display incomplete status and explicit continuation ranges',()=>{
  const output=formatToolResult('code_read',{
    page:true,complete:false,snippets:[],next_ranges:[{path:'a.rs',start:501,end:700}],
    notice:'Explicit read page; compare file hashes before continuing.',
  });
  assert.match(output,/page=true complete=false/);
  assert.match(output,/next_ranges=.*501/);
  assert.match(output,/compare file hashes/);
});

void test('zero-result searches retain explicit disk verification guidance', () => {
  const search = { matches: [], total_matches: 0, truncated: false };
  const cases: Array<[string, JsonValue]> = [
    ['code_grep', { ...search, freshness }],
    ['code_context', { search, snippets: [], freshness }],
    ['code_files', { paths: [], total_matches: 0, truncated: false, freshness }],
  ];
  for (const [operation, result] of cases) {
    const text = formatToolResult(operation, result);
    assert.match(text, /NOTICE freshness=watcher_snapshot/);
    assert.match(text, /full_disk_verified=false/);
    assert.match(text, /live disk read/);
    assert.match(text, /code_sync/);
  }
});

void test('context reports hash verification without claiming a full disk scan', () => {
  const text = formatToolResult('code_context', {
    search: { matches: [], total_matches: 0, truncated: false },
    snippets: [], freshness: { ...freshness, returned_file_check: 'sha256' },
  });
  assert.match(text, /returned_file_check=sha256 full_disk_verified=false/);
  assert.throws(() => formatToolResult('code_grep', {
    matches: [], total_matches: 0, truncated: false,
    freshness: { ...freshness, full_disk_verified: 'false' },
  }), /full_disk_verified/);
});

void test('search results become direct path-line text', () => {
  assert.equal(
    formatToolResult('code_grep', {
      total_matches: 1,
      truncated: false,
      matches: [{ path: 'src/main.rs', line: 7, text: 'fn main() {}' }],
    }),
    'matches=1 truncated=false\nsrc/main.rs:7 fn main() {}',
  );
  assert.equal(
    formatToolResult('code_grep', {
      total_matches: 2,
      truncated: false,
      matches: [
        { path: 'src/main.rs', line: 7, text: 'fn main() {}' },
        { path: 'src/main.rs', line: 12, text: 'fn helper() {}' },
      ],
    }),
    'matches=2 truncated=false\nsrc/main.rs:7 fn main() {}\n:12 fn helper() {}',
  );
});

void test('self-test cannot report old daemon capabilities as ready', () => {
  assert.throws(() => validateDaemonStatus({}), /capabilities/);
  assert.throws(() => validateDaemonStatus({ capabilities: {
    schema: 1, code_files: false, context_radius_max: 20,
  } }), /outdated/);
  assert.throws(() => validateDaemonStatus({ capabilities: {
    schema: 3, code_files: true, context_radius_max: 100, authorized_worktrees: true, read_pages: true,
  } }), /outdated/);
  assert.throws(() => validateDaemonStatus({ capabilities: {
    schema: 3, code_files: true, context_radius_max: 100, authorized_worktrees: true,
    read_pages: true, request_tracing: true,
  } }), /outdated/);
  assert.doesNotThrow(() => validateDaemonStatus({ capabilities: {
    schema: 3, code_files: true, context_radius_max: 100, authorized_worktrees: true, read_pages:true, request_tracing:true, trace_clock:'unix_ms', source_cache:'nonblocking_root_cache',
  } }));
  const bloatedStatus = {
    capabilities: {
      schema: 3, code_files: true, context_radius_max: 100, authorized_worktrees: true,
      read_pages: true, request_tracing: true, trace_clock: 'unix_ms', source_cache: 'nonblocking_root_cache',
    },
    coverage: { root: '/repo', globs: Array.from({ length: 100 }, (_, i) => `*.ext${i}`) },
    performance: {
      recent_requests: Array.from({ length: 60 }, (_, i) => ({ id: i })),
      recent_clients: Array.from({ length: 60 }, (_, i) => ({ id: i })),
    },
  };
  const compactStatusText = formatToolResult('code_status', bloatedStatus);
  const parsedStatus = JSON.parse(compactStatusText) as {
    coverage: { globs: string[]; globs_count: number };
    performance: { recent_requests: JsonValue[]; recent_requests_total: number };
  };
  assert.doesNotThrow(() => validateDaemonStatus(parsedStatus));
  assert.equal(parsedStatus.coverage.globs.length, 8);
  assert.equal(parsedStatus.coverage.globs_count, 100);
  assert.equal(parsedStatus.performance.recent_requests.length, 3);
  assert.equal(parsedStatus.performance.recent_requests_total, 60);
});

void test('context results include numbered source without JSON processing', () => {
  const output = formatToolResult('code_context', {
    search: {
      total_matches: 1,
      truncated: false,
      matches: [{ path: 'src/main.rs', line: 7 }],
    },
    snippets: [
      {
        path: 'src/main.rs',
        start: 6,
        end: 8,
        sha256: 'abc',
        lines: ['use std::io;', 'fn main() {}', ''],
      },
    ],
  });
  assert.match(output, /src\/main\.rs:7/);
  assert.match(output, /--- src\/main\.rs:6-8 sha256=abc/);
  assert.match(output, /7: fn main\(\) \{\}/);
  assert.doesNotMatch(output, /^\{/);

  const multiHit = formatToolResult('code_context', {
    search: {
      total_matches: 5,
      truncated: false,
      matches: [
        { path: 'src/main.rs', line: 7 },
        { path: 'src/main.rs', line: 12 },
        { path: 'src/main.rs', line: 20 },
        { path: 'src/main.rs', line: 21 },
        { path: 'src/main.rs', line: 22 },
        { path: 'src/lib.rs', line: 4 },
      ],
    },
    snippets: [],
  });
  assert.match(multiHit, /src\/main\.rs:7,12,20-22\nsrc\/lib\.rs:4/);
});

void test('read ranges retain path, hash, and line numbers', () => {
  assert.equal(
    formatToolResult('code_read', [
      {
        path: 'src/lib.rs',
        start: 10,
        end: 11,
        sha256: 'def',
        lines: ['pub fn ready() {', '}'],
      },
    ]),
    '--- src/lib.rs:10-11 sha256=def\n10: pub fn ready() {\n11: }',
  );
  assert.equal(
    formatToolResult('code_read', [
      {
        path: 'src/lib.rs',
        start: 10,
        end: 11,
        sha256: 'def',
        lines: ['pub fn ready() {', '}'],
      },
      {
        path: 'src/lib.rs',
        start: 20,
        end: 20,
        sha256: 'def',
        lines: ['pub fn done() {}'],
      },
    ]),
    '--- src/lib.rs:10-11 sha256=def\n10: pub fn ready() {\n11: }\n--- src/lib.rs:20-20\n20: pub fn done() {}',
  );
});

void test('malformed search payloads fail instead of emitting success-shaped defaults', () => {
  assert.throws(
    () =>
      formatToolResult('code_grep', {
        total_matches: '1',
        truncated: false,
        matches: [],
      }),
    /total_matches/,
  );
  assert.throws(
    () =>
      formatToolResult('code_grep', {
        total_matches: 1,
        truncated: false,
        matches: [{ path: 'src/main.rs' }],
      }),
    /line/,
  );
});

void test('malformed snippet ranges fail instead of hiding missing source', () => {
  assert.throws(
    () =>
      formatToolResult('code_read', [
        {
          path: 'src/lib.rs',
          start: 10,
          end: 11,
          sha256: 'def',
          lines: ['pub fn ready() {}'],
        },
      ]),
    /expected 2 lines/,
  );
});
void test('file discovery returns filenames without fabricated source lines', () => {
  assert.equal(formatToolResult('code_files', {
    paths: ['README.md', 'src/a.rs'], total_matches: 2, truncated: false,
  }), 'files=2 truncated=false\nREADME.md\nsrc/a.rs');
});

void test('oversized tool outputs are bounded to stay under 8KiB inline host limit', () => {
  const manyPaths = Array.from({ length: 200 }, (_, i) => `services/operator-service/src/module_${i}/routes_${i}.py`);
  const filesOut = formatToolResult('code_files', {
    paths: manyPaths, total_matches: 200, truncated: false, freshness,
  });
  assert.ok(filesOut.length <= 7600, `files length=${filesOut.length}`);
  assert.match(filesOut, /^files=200 truncated=true\n/);
  assert.match(filesOut, /omitted to stay inline/);

  const manyMatches = Array.from({ length: 150 }, (_, i) => ({
    path: `src/module_${i}.rs`,
    line: i + 1,
    text: `pub fn handler_${i}() { println!("processing item ${i} with some extra context text"); }`,
  }));
  const grepOut = formatToolResult('code_grep', {
    total_matches: 150, truncated: false, matches: manyMatches, freshness,
  });
  assert.ok(grepOut.length <= 7600, `grep length=${grepOut.length}`);
  assert.match(grepOut, /^matches=150 truncated=true\n/);

  const bigLines = Array.from({ length: 250 }, (_, i) => `  const value_${i} = computeSomething(${i}, "payload_string_for_testing_inline_budget");`);
  const readOut = formatToolResult('code_read', [
    {
      path: 'src/large.ts',
      start: 1,
      end: 250,
      sha256: 'abc12345',
      lines: bigLines,
    },
  ]);
  assert.ok(readOut.length <= 7600, `read length=${readOut.length}`);
  assert.match(readOut, /inline budget reached \(<8KiB\); next_ranges=/);

  const manyWorkspaces = Array.from({ length: 50 }, (_, i) => ({
    root: `/worktrees/project-${i}`,
    scopes: ['.'],
    globs: Array.from({ length: 90 }, (__, g) => `*.ext${g}`),
    excludes: [],
    default_excludes: true,
    prewarm: false,
    allow_worktrees: false,
    managed_parent: i === 0 ? null : '/worktrees/project-0',
    policy: { max_files: 100000, max_source_bytes: 268435456 },
  }));
  const wsOut = formatToolResult('code_workspaces', manyWorkspaces);
  assert.ok(wsOut.length <= 7600, `workspaces length=${wsOut.length}`);
});
