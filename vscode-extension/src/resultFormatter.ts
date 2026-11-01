import type { JsonValue } from './types.js';

export function formatToolResult(operation: string, value: JsonValue): string {
  switch (operation) {
    case 'code_files': {
      const result = recordValue(value, 'file result');
      const paths = arrayValue(result.paths, 'file paths');
      const total = integerValue(result.total_matches, 'total_matches');
      const lines = [
        `files=${total} truncated=${booleanValue(result.truncated, 'truncated')}`,
        ...paths.map((path) => stringValue(path, 'file path')),
      ];
      if (result.freshness !== undefined) {
        lines.splice(1, 0, formatFreshness(result.freshness, total === 0));
      }
      return lines.join('\n');
    }

    case 'code_grep':
      return formatSearch(value);
    case 'code_context':
      return formatContext(value);
    case 'code_read':
      if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
        const result=recordValue(value,'read page');
        return `page=true complete=${booleanValue(result.complete,'complete')}\n`
          + `${stringValue(result.notice,'page notice')}\n`
          + `${formatSnippets(requiredValue(result,'snippets','read page'))}\n`
          + `next_ranges=${JSON.stringify(arrayValue(result.next_ranges,'next ranges'))}`;
      }
      return formatSnippets(value);
    case 'code_status':
      return formatCompactStatusJson(value);
    case 'code_workspaces':
      return formatCompactWorkspacesJson(value);
    default:
      return JSON.stringify(value);
  }
}

function formatCompactStatusJson(value: JsonValue): string {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return JSON.stringify(value);
  }
  const clone: Record<string, JsonValue> = { ...value };
  if (typeof clone.coverage === 'object' && clone.coverage !== null && !Array.isArray(clone.coverage)) {
    const cov = { ...clone.coverage };
    if (Array.isArray(cov.globs) && cov.globs.length > 12) {
      cov.globs_count = cov.globs.length;
      cov.globs = cov.globs.slice(0, 8);
    }
    clone.coverage = cov;
  }
  if (typeof clone.performance === 'object' && clone.performance !== null && !Array.isArray(clone.performance)) {
    const perf = { ...clone.performance };
    for (const key of ['recent_requests', 'recent_clients', 'recent_background', 'evictions']) {
      const arr = perf[key];
      if (Array.isArray(arr) && arr.length > 3) {
        perf[`${key}_total`] = arr.length;
        perf[key] = arr.slice(-3);
      }
    }
    clone.performance = perf;
  }
  return JSON.stringify(clone);
}

function formatCompactWorkspacesJson(value: JsonValue): string {
  if (!Array.isArray(value) || value.length <= 2) {
    return JSON.stringify(value);
  }
  const compacted = value.map((item) => {
    if (typeof item === 'object' && item !== null && !Array.isArray(item)) {
      const copy: Record<string, JsonValue> = { ...item };
      if (Array.isArray(copy.globs) && copy.globs.length > 8) {
        copy.globs_count = copy.globs.length;
        copy.globs = copy.globs.slice(0, 6);
      }
      return copy;
    }
    return item;
  });
  return JSON.stringify(compacted);
}

export function validateDaemonStatus(value: JsonValue): void {
  const status = recordValue(value, 'daemon status');
  const capabilities = recordValue(status.capabilities, 'daemon capabilities; update the daemon and reconnect at a safe boundary');
  if (integerValue(capabilities.schema, 'capability schema') < 3 ||
      capabilities.code_files !== true || capabilities.context_radius_max !== 100 ||
      capabilities.authorized_worktrees !== true || capabilities.read_pages !== true ||
      capabilities.request_tracing !== true || capabilities.trace_clock !== 'unix_ms' ||
      capabilities.source_cache !== 'nonblocking_root_cache') {
    throw new Error('Code Context daemon capabilities are outdated; update it and reconnect tools at a safe boundary.');
  }
}

export function compressLineNumbers(nums: readonly number[]): string {
  if (nums.length <= 2) {
    return nums.join(',');
  }
  const parts: string[] = [];
  let i = 0;
  while (i < nums.length) {
    const start = nums[i]!;
    let end = start;
    let j = i + 1;
    while (j < nums.length && nums[j] === end + 1) {
      end = nums[j]!;
      j += 1;
    }
    if (j - i >= 3) {
      parts.push(`${start}-${end}`);
      i = j;
    } else {
      parts.push(String(start));
      i += 1;
    }
  }
  return parts.join(',');
}

function formatSearch(value: JsonValue, includeFreshness = true): string {
  const search = recordValue(value, 'search result');
  const matches = arrayValue(search.matches, 'search matches');
  const total = integerValue(search.total_matches, 'total_matches');
  const lines = [
    `matches=${total} truncated=${booleanValue(search.truncated, 'truncated')}`,
  ];
  let previousPath: string | undefined;
  let pendingLines: number[] = [];
  const flushPending = (): void => {
    if (previousPath !== undefined && pendingLines.length > 0) {
      lines.push(`${previousPath}:${compressLineNumbers(pendingLines)}`);
      pendingLines = [];
    }
  };
  for (const [index, match] of matches.entries()) {
    const item = recordValue(match, `search match ${index}`);
    const path = stringValue(item.path, `search match ${index} path`);
    const line = integerValue(item.line, `search match ${index} line`, 1);
    if (item.text === undefined) {
      if (previousPath === path) {
        pendingLines.push(line);
      } else {
        flushPending();
        previousPath = path;
        pendingLines = [line];
      }
    } else {
      flushPending();
      const rawText = stringValue(item.text, `search match ${index} text`);
      const preview = rawText.length > 512 ? `${rawText.slice(0, 512)}...` : rawText;
      const text = ` ${preview}`;
      if (previousPath === path) {
        lines.push(`:${line}${text}`);
      } else {
        lines.push(`${path}:${line}${text}`);
        previousPath = path;
      }
    }
  }
  flushPending();
  if (includeFreshness && search.freshness !== undefined) {
    lines.splice(1, 0, formatFreshness(search.freshness, total === 0));
  }
  return lines.join('\n');
}

function formatContext(value: JsonValue): string {
  const context = recordValue(value, 'context result');
  const searchVal = requiredValue(context, 'search', 'context result');
  const searchRec = recordValue(searchVal, 'search result');
  const total = integerValue(searchRec.total_matches, 'total_matches');
  const search = formatSearch(searchVal, context.freshness === undefined);
  const snippets = formatSnippets(requiredValue(context, 'snippets', 'context result'));
  const notice = context.freshness === undefined ? '' : `\n${formatFreshness(context.freshness, total === 0)}`;
  return `${search}${notice}\n\n${snippets}`;
}

function formatFreshness(value: JsonValue, includeNotice = true): string {
  const freshness = recordValue(value, 'search freshness');
  const basis = stringValue(freshness.basis, 'freshness basis');
  const check = stringValue(freshness.returned_file_check, 'returned_file_check');
  const verified = booleanValue(freshness.full_disk_verified, 'full_disk_verified');
  const notice = stringValue(freshness.notice, 'freshness notice');
  return includeNotice
    ? `NOTICE freshness=${basis} returned_file_check=${check} full_disk_verified=${verified}: ${notice}`
    : `NOTICE freshness=${basis} returned_file_check=${check} full_disk_verified=${verified}`;
}

function formatSnippets(value: JsonValue): string {
  const snippets = arrayValue(value, 'snippets');
  const output: string[] = [];
  let previousPath: string | undefined;
  let previousSha: string | undefined;
  for (const [index, snippet] of snippets.entries()) {
    const label = `snippet ${index}`;
    const item = recordValue(snippet, label);
    const path = stringValue(item.path, `${label} path`);
    const start = integerValue(item.start, `${label} start`, 1);
    const end = integerValue(item.end, `${label} end`, start);
    const sha = stringValue(item.sha256, `${label} sha256`);
    const lines = arrayValue(item.lines, `${label} lines`);
    if (lines.length !== end - start + 1) {
      throw new Error(
        `Invalid Code Context ${label}: expected ${end - start + 1} lines, received ${lines.length}.`,
      );
    }
    if (previousPath === path && previousSha === sha) {
      output.push(`--- ${path}:${start}-${end}`);
    } else {
      output.push(`--- ${path}:${start}-${end} sha256=${sha}`);
      previousPath = path;
      previousSha = sha;
    }
    for (let lineIndex = 0; lineIndex < lines.length; lineIndex += 1) {
      const rawLine = stringValue(lines[lineIndex], `${label} line ${lineIndex}`);
      const boundedLine =
        rawLine.length > 2048 ? `${rawLine.slice(0, 2048)}... [truncated]` : rawLine;
      output.push(`${start + lineIndex}: ${boundedLine}`);
    }
  }
  return output.length > 0 ? output.join('\n') : 'snippets=0';
}

function recordValue(
  value: JsonValue | undefined,
  label: string,
): { [key: string]: JsonValue } {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error(`Invalid Code Context ${label}: expected an object.`);
  }
  return value;
}

function arrayValue(value: JsonValue | undefined, label: string): JsonValue[] {
  if (!Array.isArray(value)) {
    throw new Error(`Invalid Code Context ${label}: expected an array.`);
  }
  return value;
}

function requiredValue(
  value: { [key: string]: JsonValue },
  key: string,
  label: string,
): JsonValue {
  if (!(key in value)) {
    throw new Error(`Invalid Code Context ${label}: missing ${key}.`);
  }
  return value[key] as JsonValue;
}

function stringValue(value: JsonValue | undefined, label: string): string {
  if (typeof value !== 'string') {
    throw new Error(`Invalid Code Context ${label}: expected a string.`);
  }
  return value;
}

function integerValue(value: JsonValue | undefined, label: string, minimum = 0): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < minimum) {
    throw new Error(
      `Invalid Code Context ${label}: expected an integer greater than or equal to ${minimum}.`,
    );
  }
  return value;
}

function booleanValue(value: JsonValue | undefined, label: string): boolean {
  if (typeof value !== 'boolean') {
    throw new Error(`Invalid Code Context ${label}: expected a boolean.`);
  }
  return value;
}
