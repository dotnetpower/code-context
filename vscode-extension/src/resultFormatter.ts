import type { JsonValue } from './types.js';

export function formatToolResult(operation: string, value: JsonValue): string {
  switch (operation) {
    case 'code_files': {
      const result = recordValue(value, 'file result');
      const paths = arrayValue(result.paths, 'file paths');
      const lines = [
        `files=${integerValue(result.total_matches, 'total_matches')} truncated=${booleanValue(result.truncated, 'truncated')}`,
        ...paths.map((path) => stringValue(path, 'file path')),
      ];
      if (result.freshness !== undefined) {
        lines.splice(1, 0, formatFreshness(result.freshness));
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
    default:
      return JSON.stringify(value);
  }
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

function formatSearch(value: JsonValue): string {
  const search = recordValue(value, 'search result');
  const matches = arrayValue(search.matches, 'search matches');
  const lines = [
    `matches=${integerValue(search.total_matches, 'total_matches')} truncated=${booleanValue(search.truncated, 'truncated')}`,
  ];
  for (const [index, match] of matches.entries()) {
    const item = recordValue(match, `search match ${index}`);
    const path = stringValue(item.path, `search match ${index} path`);
    const line = integerValue(item.line, `search match ${index} line`, 1);
    const text =
      item.text === undefined ? '' : ` ${stringValue(item.text, `search match ${index} text`)}`;
    lines.push(`${path}:${line}${text}`);
  }
  if (search.freshness !== undefined) {
    lines.splice(1, 0, formatFreshness(search.freshness));
  }
  return lines.join('\n');
}

function formatContext(value: JsonValue): string {
  const context = recordValue(value, 'context result');
  const search = formatSearch(requiredValue(context, 'search', 'context result'));
  const snippets = formatSnippets(requiredValue(context, 'snippets', 'context result'));
  const notice = context.freshness === undefined ? '' : `\n${formatFreshness(context.freshness)}`;
  return `${search}${notice}\n\n${snippets}`;
}

function formatFreshness(value: JsonValue): string {
  const freshness = recordValue(value, 'search freshness');
  const check = stringValue(freshness.returned_file_check, 'returned_file_check');
  const verified = booleanValue(freshness.full_disk_verified, 'full_disk_verified');
  const notice = stringValue(freshness.notice, 'freshness notice');
  return `NOTICE freshness=${stringValue(freshness.basis, 'freshness basis')} returned_file_check=${check} full_disk_verified=${verified}: ${notice}`;
}

function formatSnippets(value: JsonValue): string {
  const snippets = arrayValue(value, 'snippets');
  const output: string[] = [];
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
    output.push(`--- ${path}:${start}-${end} sha256=${sha}`);
    lines.forEach((line, index) => {
      output.push(`${start + index}: ${stringValue(line, `${label} line ${index}`)}`);
    });
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
