import type { JsonValue } from './types.js';

const MAX_INLINE_CHARS = 7_400;

export function formatToolResult(operation: string, value: JsonValue): string {
  switch (operation) {
    case 'code_files': {
      const result = recordValue(value, 'file result');
      const paths = arrayValue(result.paths, 'file paths').map((path) =>
        stringValue(path, 'file path'),
      );
      const total = integerValue(result.total_matches, 'total_matches');
      let truncated = booleanValue(result.truncated, 'truncated');
      const freshnessLine =
        result.freshness !== undefined
          ? formatFreshness(result.freshness, total === 0)
          : undefined;
      const kept: string[] = [];
      let used = 64 + (freshnessLine ? freshnessLine.length + 1 : 0);
      for (const p of paths) {
        if (kept.length > 0 && used + p.length + 1 > MAX_INLINE_CHARS - 120) {
          truncated = true;
          break;
        }
        kept.push(p);
        used += p.length + 1;
      }
      const lines = [`files=${total} truncated=${truncated}`];
      if (freshnessLine !== undefined) {
        lines.push(freshnessLine);
      }
      lines.push(...kept);
      if (kept.length < paths.length) {
        lines.push(
          `... [${paths.length - kept.length} more path(s) omitted to stay inline (<8KiB); narrow glob/paths or set limit]`,
        );
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
        const header = `page=true complete=${booleanValue(result.complete,'complete')}\n`
          + `${stringValue(result.notice,'page notice')}\n`;
        const footer = `\nnext_ranges=${JSON.stringify(arrayValue(result.next_ranges,'next ranges'))}`;
        const budget = Math.max(1_500, MAX_INLINE_CHARS - header.length - footer.length);
        return `${header}${formatSnippets(requiredValue(result,'snippets','read page'), budget)}${footer}`;
      }
      return formatSnippets(value, MAX_INLINE_CHARS);
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
    const isLoaded = clone.index_state === 'loaded';
    for (const key of ['recent_requests', 'recent_clients', 'recent_background', 'evictions']) {
      const keepRecent = isLoaded && key !== 'recent_requests' ? 2 : 3;
      const arr = perf[key];
      if (Array.isArray(arr)) {
        const sliced = arr.length > keepRecent ? arr.slice(-keepRecent) : arr;
        if (arr.length > keepRecent) {
          perf[`${key}_total`] = arr.length;
        }
        if (key === 'recent_clients') {
          perf[key] = sliced.map((entry) => {
            if (typeof entry === 'object' && entry !== null && !Array.isArray(entry)) {
              const clientCopy = { ...entry };
              delete clientCopy.host_memory;
              return clientCopy;
            }
            return entry;
          });
        } else {
          perf[key] = sliced;
        }
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
  const manyWorkspaces = value.length > 8;
  const compacted = value.map((item) => {
    if (typeof item === 'object' && item !== null && !Array.isArray(item)) {
      const copy: Record<string, JsonValue> = { ...item };
      delete copy.policy;
      if (Array.isArray(copy.globs) && copy.globs.length > 8) {
        copy.globs_count = copy.globs.length;
        if (manyWorkspaces && typeof copy.managed_parent === 'string') {
          delete copy.globs;
          delete copy.default_excludes;
          delete copy.prewarm;
          delete copy.allow_worktrees;
          if (Array.isArray(copy.excludes) && copy.excludes.length === 0) {
            delete copy.excludes;
          }
        } else {
          copy.globs = copy.globs.slice(0, 6);
        }
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

function formatSearch(value: JsonValue, includeFreshness = true, maxChars = MAX_INLINE_CHARS): string {
  const search = recordValue(value, 'search result');
  const matches = arrayValue(search.matches, 'search matches');
  const total = integerValue(search.total_matches, 'total_matches');
  let truncated = booleanValue(search.truncated, 'truncated');
  const bodyLines: string[] = [];
  let previousPath: string | undefined;
  let pendingLines: number[] = [];
  const flushPending = (): void => {
    if (previousPath !== undefined && pendingLines.length > 0) {
      bodyLines.push(`${previousPath}:${compressLineNumbers(pendingLines)}`);
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
        bodyLines.push(`:${line}${text}`);
      } else {
        bodyLines.push(`${path}:${line}${text}`);
        previousPath = path;
      }
    }
  }
  flushPending();
  const freshnessLine =
    includeFreshness && search.freshness !== undefined
      ? formatFreshness(search.freshness, total === 0)
      : undefined;
  const kept: string[] = [];
  let used = 64 + (freshnessLine ? freshnessLine.length + 1 : 0);
  for (const line of bodyLines) {
    if (kept.length > 0 && used + line.length + 1 > maxChars - 120) {
      truncated = true;
      break;
    }
    kept.push(line);
    used += line.length + 1;
  }
  const lines = [`matches=${total} truncated=${truncated}`];
  if (freshnessLine !== undefined) {
    lines.push(freshnessLine);
  }
  lines.push(...kept);
  if (kept.length < bodyLines.length) {
    lines.push(
      `... [${bodyLines.length - kept.length} more match line(s) omitted to stay inline (<8KiB); set max_matches_per_file or narrow paths]`,
    );
  }
  return lines.join('\n');
}

function formatContext(value: JsonValue): string {
  const context = recordValue(value, 'context result');
  const searchVal = requiredValue(context, 'search', 'context result');
  const searchRec = recordValue(searchVal, 'search result');
  const total = integerValue(searchRec.total_matches, 'total_matches');
  const search = formatSearch(searchVal, context.freshness === undefined, 2_400);
  const notice = context.freshness === undefined ? '' : `\n${formatFreshness(context.freshness, total === 0)}`;
  const snippetBudget = Math.max(1_500, MAX_INLINE_CHARS - search.length - notice.length - 2);
  const snippets = formatSnippets(requiredValue(context, 'snippets', 'context result'), snippetBudget);
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

function formatSnippets(value: JsonValue, maxChars = MAX_INLINE_CHARS): string {
  const snippets = arrayValue(value, 'snippets');
  const validated: Array<{
    path: string;
    start: number;
    end: number;
    sha: string;
    lines: string[];
  }> = [];
  for (const [index, snippet] of snippets.entries()) {
    const label = `snippet ${index}`;
    const item = recordValue(snippet, label);
    const path = stringValue(item.path, `${label} path`);
    const start = integerValue(item.start, `${label} start`, 1);
    const end = integerValue(item.end, `${label} end`, start);
    const sha = stringValue(item.sha256, `${label} sha256`);
    const rawLines = arrayValue(item.lines, `${label} lines`);
    if (rawLines.length !== end - start + 1) {
      throw new Error(
        `Invalid Code Context ${label}: expected ${end - start + 1} lines, received ${rawLines.length}.`,
      );
    }
    const formattedLines: string[] = [];
    for (let lineIndex = 0; lineIndex < rawLines.length; lineIndex += 1) {
      const rawLine = stringValue(rawLines[lineIndex], `${label} line ${lineIndex}`);
      const boundedLine =
        rawLine.length > 2048 ? `${rawLine.slice(0, 2048)}... [truncated]` : rawLine;
      formattedLines.push(`${start + lineIndex}: ${boundedLine}`);
    }
    validated.push({ path, start, end, sha, lines: formattedLines });
  }
  if (validated.length === 0) {
    return 'snippets=0';
  }
  const output: string[] = [];
  const nextRanges: Array<{ path: string; start: number; end: number }> = [];
  let previousPath: string | undefined;
  let previousSha: string | undefined;
  let used = 0;
  const charLimit = Math.max(512, maxChars - 180);
  for (let i = 0; i < validated.length; i += 1) {
    const item = validated[i]!;
    const sameFileHeader = previousPath === item.path && previousSha === item.sha;
    const baseHeaderLen = item.path.length + (sameFileHeader ? 16 : 88);
    if (output.length > 0 && used + baseHeaderLen + (item.lines[0]?.length ?? 0) > charLimit) {
      for (let rest = i; rest < validated.length; rest += 1) {
        const rem = validated[rest]!;
        nextRanges.push({ path: rem.path, start: rem.start, end: rem.end });
      }
      break;
    }
    let keptCount = 0;
    let snippetChars = baseHeaderLen;
    for (const line of item.lines) {
      if (
        (output.length > 0 || keptCount > 0) &&
        used + snippetChars + line.length + 1 > charLimit
      ) {
        break;
      }
      snippetChars += line.length + 1;
      keptCount += 1;
    }
    if (keptCount === 0) {
      keptCount = 1;
    }
    const actualEnd = item.start + keptCount - 1;
    const header = sameFileHeader
      ? `--- ${item.path}:${item.start}-${actualEnd}`
      : `--- ${item.path}:${item.start}-${actualEnd} sha256=${item.sha}`;
    previousPath = item.path;
    previousSha = item.sha;
    output.push(header);
    used += header.length + 1;
    for (let k = 0; k < keptCount; k += 1) {
      const line = item.lines[k]!;
      output.push(line);
      used += line.length + 1;
    }
    if (actualEnd < item.end) {
      nextRanges.push({ path: item.path, start: actualEnd + 1, end: item.end });
      for (let rest = i + 1; rest < validated.length; rest += 1) {
        const rem = validated[rest]!;
        nextRanges.push({ path: rem.path, start: rem.start, end: rem.end });
      }
      break;
    }
  }
  if (nextRanges.length > 0) {
    output.push(
      `... [inline budget reached (<8KiB); next_ranges=${JSON.stringify(nextRanges.slice(0, 8))}]`,
    );
  }
  return output.join('\n');
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
