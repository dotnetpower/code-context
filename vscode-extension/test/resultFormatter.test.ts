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
