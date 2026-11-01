import { existsSync } from 'node:fs';
import { basename, isAbsolute, join, relative, resolve, sep, posix } from 'node:path';

import type { WorkspaceSpec } from './types.js';

export function normalizeLocalPath(path: string): string {
  if (
    sep === '/' &&
    path.length > 1 &&
    path.startsWith('/') &&
    !path.includes('//') &&
    !path.includes('/.') &&
    !path.endsWith('/')
  ) {
    return path;
  }
  const resolved = resolve(path);
  return resolved.replace(/^[a-z]:(?=[\\/])/u, (drive) => drive.toUpperCase());
}

export class RootResolutionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'RootResolutionError';
  }
}

export function wslCandidateToLinuxPath(candidate: string): string | undefined {
  const cleaned = unwrapMarkdownLinkAndQuotes(candidate);
  const uncMatch = /^\\\\(?:wsl\.localhost|wsl\$)\\[^\\]+(\\.+)$/iu.exec(cleaned);
  if (uncMatch?.[1]) {
    const converted = uncMatch[1].replaceAll('\\', '/');
    return posix.isAbsolute(converted) ? posix.normalize(converted) : undefined;
  }
  const fileWslMatch = /^file:\/\/(?:wsl\.localhost|wsl\$)\/[^/]+(\/.+)$/iu.exec(cleaned);
  if (fileWslMatch?.[1]) {
    try {
      const decoded = decodeURIComponent(fileWslMatch[1]);
      return posix.isAbsolute(decoded) ? posix.normalize(decoded) : undefined;
    } catch {
      return posix.isAbsolute(fileWslMatch[1]) ? posix.normalize(fileWslMatch[1]) : undefined;
    }
  }
  if (posix.isAbsolute(cleaned) && !/[\\\0\r\n]/u.test(cleaned)) {
    return posix.normalize(cleaned);
  }
  return undefined;
}

function bestContainingPosixRoot(
  roots: readonly string[],
  candidate: string,
): string | undefined {
  let best: string | undefined;
  for (const root of roots) {
    const normalizedRoot = posix.normalize(root);
    if (
      (best === undefined || normalizedRoot.length > best.length) &&
      (candidate === normalizedRoot ||
        candidate.startsWith(normalizedRoot.endsWith('/') ? normalizedRoot : `${normalizedRoot}/`))
    ) {
      best = normalizedRoot;
    }
  }
  return best;
}

export function disambiguateRootByRelativePaths(
  roots: readonly string[],
  relativeTargets: readonly string[],
  usePosix = false,
): string | undefined {
  if (roots.length <= 1 || relativeTargets.length === 0) {
    return undefined;
  }
  for (const rawTarget of relativeTargets) {
    const strippedQuery = rawTarget.replace(
      /\?(?:raw|url|worker|sharedworker|inline|inline-css|no-inline|react|component|used)(?:&[a-zA-Z0-9_=-]*)?$/u,
      '',
    );
    const cleaned = strippedQuery.replaceAll('\\', '/').replace(/^(?:\.\/)+/u, '').replace(/\/+$/u, '');
    if (!cleaned || cleaned.startsWith('/') || cleaned.startsWith('..')) {
      continue;
    }
    const candidateRelatives =
      cleaned.startsWith('@/') || cleaned.startsWith('~/')
        ? [cleaned, `src/${cleaned.slice(2)}`]
        : cleaned.startsWith('#') && cleaned.length > 1
          ? [cleaned, `src/${cleaned.slice(1)}`]
          : [cleaned];
    for (const candidateRel of candidateRelatives) {
      const firstSegment = candidateRel.split('/')[0];
      if (firstSegment && candidateRel.includes('/')) {
        const rest = candidateRel.slice(firstSegment.length + 1);
        const basenameMatches = roots.filter(
          (root) => (usePosix ? posix.basename(root) : basename(root)) === firstSegment,
        );
        if (basenameMatches.length === 1 && basenameMatches[0] !== undefined) {
          const matchedRoot = basenameMatches[0];
          if (!rest || existsSync(usePosix ? posix.join(matchedRoot, rest) : join(matchedRoot, rest))) {
            return matchedRoot;
          }
        }
      }
      const existingMatches = roots.filter((root) =>
        existsSync(usePosix ? posix.join(root, candidateRel) : join(root, candidateRel)),
      );
      if (existingMatches.length === 1 && existingMatches[0] !== undefined) {
        return existingMatches[0];
      }
    }
  }
  return undefined;
}

export function resolveWslRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[] = [],
  relativeTargets: readonly string[] = [],
): string {
  let target = explicitPath;
  const roots = workspaces.map((workspace) => workspace.root);
  if (target && !posix.isAbsolute(target) && !/[\\:\0\r\n]/u.test(target)) {
    const byName = roots.filter((root) => posix.basename(root) === target);
    if (byName.length === 1 && byName[0] !== undefined) {
      target = byName[0];
    }
  }
  if (!target && candidates.length > 0) {
    for (const candidate of candidates) {
      const linuxCandidate = wslCandidateToLinuxPath(candidate);
      if (linuxCandidate) {
        const match = bestContainingPosixRoot(roots, linuxCandidate);
        if (match) {
          target = match;
          break;
        }
      }
    }
  }
  target ??= disambiguateRootByRelativePaths(roots, relativeTargets, true);
  target ??= workspaces.length === 1 ? workspaces[0]?.root : undefined;
  if (!target) {
    const listed = workspaces.slice(0, 5).map((workspace) => workspace.root).join(', ');
    const hint = listed ? ` Registered roots: ${listed}` : '';
    throw new RootResolutionError(
      `Windows Agent Sessions require an explicit absolute Linux root when multiple roots are registered.${hint}`,
    );
  }
  if (!posix.isAbsolute(target) || /[\\\0\r\n]/u.test(target)) {
    throw new RootResolutionError('WSL Code Context root must be an absolute Linux path.');
  }
  return posix.normalize(target);
}

export function resolveRegisteredRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
  candidates: readonly string[],
  relativeTargets: readonly string[] = [],
): string {
  const roots = workspaces.map((workspace) => normalizeLocalPath(workspace.root));
  if (explicitPath !== undefined) {
    if (!isAbsolute(explicitPath) && !/[\\/:\0\r\n]/u.test(explicitPath)) {
      const byName = roots.filter((root) => basename(root) === explicitPath);
      if (byName.length === 1 && byName[0] !== undefined) {
        return byName[0];
      }
      const lowerExplicit = explicitPath.toLowerCase();
      const byCaseInsensitiveName = roots.filter(
        (root) => basename(root).toLowerCase() === lowerExplicit,
      );
      if (byCaseInsensitiveName.length === 1 && byCaseInsensitiveName[0] !== undefined) {
        return byCaseInsensitiveName[0];
      }
    }
    if (!isAbsolute(explicitPath)) {
      throw new RootResolutionError('Code Context root must be an absolute path.');
    }
    const match = bestContainingRoot(roots, normalizeLocalPath(explicitPath));
    if (!match) {
      throw new RootResolutionError(
        `No registered Code Context workspace contains ${explicitPath}. Run cctx register first.`,
      );
    }
    return match;
  }
  for (const candidate of candidates) {
    const match = bestContainingRoot(roots, normalizeLocalPath(candidate));
    if (match) {
      return match;
    }
  }
  const byRelative = disambiguateRootByRelativePaths(roots, relativeTargets, false);
  if (byRelative) {
    return byRelative;
  }
  if (roots.length === 1) {
    return roots[0] as string;
  }
  if (roots.length === 0) {
    throw new RootResolutionError(
      'No Code Context workspaces are registered. Register the current workspace first.',
    );
  }
  const listed = roots.slice(0, 5).join(', ');
  throw new RootResolutionError(
    `The current VS Code workspace is not registered with Code Context. Registered roots: ${listed}`,
  );
}

export async function resolveRegisteredRootAfterRefresh(
  workspaces: readonly WorkspaceSpec[],
  refresh: () => Promise<readonly WorkspaceSpec[]>,
  explicitPath: string | undefined,
  candidates: readonly string[],
  registerMissingRoot?: (path: string) => Promise<void>,
  relativeTargets: readonly string[] = [],
): Promise<string> {
  const preferred = candidates[0];
  if (explicitPath === undefined && preferred && isAbsolute(preferred) && registerMissingRoot &&
    !bestContainingRoot(workspaces.map((workspace) => normalizeLocalPath(workspace.root)), normalizeLocalPath(preferred))) {
    const refreshed = await refresh();
    if (!bestContainingRoot(refreshed.map((workspace) => normalizeLocalPath(workspace.root)), normalizeLocalPath(preferred))) {
      await registerMissingRoot(preferred);
      return resolveRegisteredRoot(await refresh(), preferred, [], relativeTargets);
    }
    return resolveRegisteredRoot(refreshed, preferred, [], relativeTargets);
  }
  try {
    return resolveRegisteredRoot(workspaces, explicitPath, candidates, relativeTargets);
  } catch (error: unknown) {
    if (!(error instanceof RootResolutionError)) {
      throw error;
    }
    const refreshed = await refresh();
    try {
      return resolveRegisteredRoot(refreshed, explicitPath, candidates, relativeTargets);
    } catch (refreshedError: unknown) {
      if (!(refreshedError instanceof RootResolutionError) || !registerMissingRoot) {
        throw refreshedError;
      }
      const target = explicitPath ?? candidates[0];
      if (!target || !isAbsolute(target)) {
        throw refreshedError;
      }
      await registerMissingRoot(target);
      return resolveRegisteredRoot(await refresh(), explicitPath, candidates, relativeTargets);
    }
  }
}

function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  let best: string | undefined;
  for (const root of roots) {
    if ((best === undefined || root.length > best.length) && contains(root, candidate)) {
      best = root;
    }
  }
  return best;
}

function contains(root: string, candidate: string): boolean {
  if (candidate === root) {
    return true;
  }
  if (candidate.startsWith(root)) {
    const boundary = root.endsWith(sep) ? candidate[root.length - 1] : candidate[root.length];
    if (boundary === sep) {
      const rest = candidate.slice(root.length);
      if (!rest.includes(`..${sep}`) && !rest.endsWith('..')) {
        return true;
      }
    }
  }
  const child = relative(root, candidate);
  return child === '' || (!child.startsWith(`..${sep}`) && child !== '..' && !isAbsolute(child));
}

export function parseGitIdentity(stdout:string): {root:string;commonDirectory:string} {
  const paths=stdout.split('\n');
  if (paths.at(-1)==='') {paths.pop();}
  const [root,commonDirectory]=paths;
  if (paths.length!==2 || root===undefined || commonDirectory===undefined ||
      paths.some((path)=>!isAbsolute(path)||/[\r\n"]/.test(path))) {
    throw new RootResolutionError('Git identity must contain exactly two unquoted absolute paths.');
  }
  return {root,commonDirectory};
}

export function listedWorktreeRoot(porcelain: string, target: string): string {
  if (!isAbsolute(target)) {
    throw new RootResolutionError('Worktree target must be an absolute path.');
  }
  const roots = porcelain
    .split(/[\0\r\n]/u)
    .filter((field) => field.startsWith('worktree '))
    .map((field) => field.slice('worktree '.length))
    .filter(isAbsolute);
  const root = bestContainingRoot(roots, resolve(target));
  if (!root) {
    throw new RootResolutionError(
      'Target is not a Git worktree of an open trusted repository; open that folder or register it explicitly.',
    );
  }
  return root;
}

const VITE_QUERY_SUFFIX_RE =
  /\?(?:raw|url|worker|sharedworker|inline|inline-css|no-inline|react|component|used)(?:&[a-zA-Z0-9_=-]*)?$/u;

export function unwrapMarkdownLinkAndQuotes(raw: string): string {
  let trimmed = raw.trim();
  if (
    trimmed.length >= 2 &&
    ((trimmed.startsWith('`') && trimmed.endsWith('`')) ||
      (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
      (trimmed.startsWith("'") && trimmed.endsWith("'")))
  ) {
    trimmed = trimmed.slice(1, -1).trim();
  }
  const mdMatch = /^\[[^\]]*\]\((.+)\)$/u.exec(trimmed);
  if (mdMatch?.[1]) {
    let target = mdMatch[1].trim();
    if (target.startsWith('<') && target.endsWith('>') && target.length >= 2) {
      target = target.slice(1, -1).trim();
    }
    return target;
  }
  return trimmed;
}

export function stripViteQuerySuffix(rawPath: string): string {
  return rawPath.replace(VITE_QUERY_SUFFIX_RE, '');
}

export function normalizeGlobPattern(glob: string): string {
  let cleaned = unwrapMarkdownLinkAndQuotes(glob);
  while (cleaned.startsWith('./') && cleaned.length > 2) {
    cleaned = cleaned.slice(2);
  }
  if (!cleaned.includes('{') && cleaned.includes(',')) {
    const parts = cleaned
      .split(',')
      .map((part) => part.trim())
      .filter((part) => part.length > 0);
    if (
      parts.length >= 2 &&
      parts.every((part) => /^(?:\*\*\/)?\*\.([a-zA-Z0-9_.+-]+)$/u.test(part))
    ) {
      const exts = parts.map((part) => part.replace(/^(?:\*\*\/)?\*\./u, ''));
      return `*.{${exts.join(',')}}`;
    }
  }
  if (cleaned.includes('{') && cleaned.includes('}')) {
    cleaned = cleaned.replace(/\{([^}]+)\}/gu, (_match, inner: string) => {
      const compact = inner
        .split(',')
        .map((s) => s.trim())
        .filter((s) => s.length > 0)
        .join(',');
      return `{${compact}}`;
    });
  }
  if (/^\*\*\/\*\.([a-zA-Z0-9_.+-]+|\{[a-zA-Z0-9_.,+-]+\})$/u.test(cleaned)) {
    return cleaned.slice(3);
  }
  return cleaned;
}

export function normalizeUriOrPath(raw: string): string {
  const trimmed = unwrapMarkdownLinkAndQuotes(raw);
  const wslFileMatch = /^file:\/\/(?:wsl\.localhost|wsl\$)\/[^/]+(\/.*)$/iu.exec(trimmed);
  if (wslFileMatch?.[1]) {
    try {
      return decodeURIComponent(wslFileMatch[1]);
    } catch {
      return wslFileMatch[1];
    }
  }
  if (trimmed.startsWith('file://')) {
    try {
      const parsed = new URL(trimmed);
      let decoded = decodeURIComponent(parsed.pathname);
      if (/^\/[a-zA-Z]:\//u.test(decoded)) {
        decoded = decoded.slice(1);
      }
      return decoded + (parsed.hash || '');
    } catch {
      return trimmed;
    }
  }
  const remoteMatch =
    /^(?:vscode-remote|vscode-file|vscode-userdata|vscode-insiders|cursor|windsurf|vscodium|git):\/\/[^/]+(\/.*)$/iu.exec(
      trimmed,
    );
  if (remoteMatch?.[1]) {
    try {
      return decodeURIComponent(remoteMatch[1]);
    } catch {
      return remoteMatch[1];
    }
  }
  const msysMatch = /^\/([a-zA-Z])\/(Users|Program Files|Windows|projects|work|dev|repos)\/(.*)$/u.exec(
    trimmed,
  );
  if (msysMatch?.[1] && msysMatch[2] && msysMatch[3] !== undefined) {
    return `${msysMatch[1].toUpperCase()}:/${msysMatch[2]}/${msysMatch[3]}`;
  }
  return trimmed;
}

export function extractLineRangeFromPath(rawPath: string): {
  path: string;
  start?: number;
  end?: number;
  anchor?: string;
} {
  let cleanedInput = unwrapMarkdownLinkAndQuotes(rawPath);
  if (cleanedInput.startsWith('-->')) {
    cleanedInput = cleanedInput.slice(3).trim();
  }
  const pyTraceMatch = /^File\s+"([^"]+)"(?:,\s*line\s+(\d+))?/u.exec(cleanedInput);
  if (pyTraceMatch?.[1]) {
    const cleanFile = stripViteQuerySuffix(normalizeUriOrPath(pyTraceMatch[1]));
    if (pyTraceMatch[2]) {
      const start = Number.parseInt(pyTraceMatch[2], 10);
      return { path: cleanFile, start, end: start + 40 };
    }
    return { path: cleanFile };
  }
  const normalized = stripViteQuerySuffix(normalizeUriOrPath(cleanedInput));
  const hashMatch =
    /^(.+?\.[^#/:]+)#L?(\d+)(?:C\d+)?(?:-L?(\d+)(?:C\d+)?)?$/u.exec(normalized);
  if (hashMatch?.[1] && hashMatch[2]) {
    const start = Number.parseInt(hashMatch[2], 10);
    const end = hashMatch[3] ? Number.parseInt(hashMatch[3], 10) : start + 40;
    return { path: stripViteQuerySuffix(hashMatch[1]), start, end };
  }
  const slugMatch = /^(.+?\.[^#/:]+)#([a-zA-Z0-9_.\-\p{L}]+)$/u.exec(normalized);
  if (slugMatch?.[1] && slugMatch[2]) {
    return { path: stripViteQuerySuffix(slugMatch[1]), anchor: slugMatch[2] };
  }
  const colonMatch = /^(.+?\.[^/:]+):(\d+)(?::\d+)?(?:-(\d+)(?::\d+)?)?$/u.exec(normalized);
  if (colonMatch?.[1] && colonMatch[2]) {
    const start = Number.parseInt(colonMatch[2], 10);
    const end = colonMatch[3] ? Number.parseInt(colonMatch[3], 10) : start + 40;
    return { path: stripViteQuerySuffix(colonMatch[1]), start, end };
  }
  const tscMatch = /^(.+?\.[^/:(]+)\((\d+),\s*\d+\)$/u.exec(normalized);
  if (tscMatch?.[1] && tscMatch[2]) {
    const start = Number.parseInt(tscMatch[2], 10);
    return { path: stripViteQuerySuffix(tscMatch[1]), start, end: start + 40 };
  }
  return { path: normalized };
}

export function normalizeRelativeTargetPath(rawPath: string, root?: string | null): string {
  let normalized = extractLineRangeFromPath(rawPath).path;
  if (!normalized.startsWith('\\\\') && !/^[a-zA-Z]:\\/u.test(normalized)) {
    normalized = normalized.replaceAll('\\', '/');
  }
  if (root) {
    const cleanRoot = root.replaceAll('\\', '/').replace(/\/+$/u, '');
    if (normalized === cleanRoot) {
      return '.';
    }
    if (normalized.startsWith(`${cleanRoot}/`)) {
      normalized = normalized.slice(cleanRoot.length + 1);
    } else {
      const rootName = posix.basename(cleanRoot) || basename(cleanRoot);
      if (rootName && normalized.startsWith(`${rootName}/`)) {
        const stripped = normalized.slice(rootName.length + 1);
        if (stripped && !existsSync(join(root, normalized)) && existsSync(join(root, stripped))) {
          normalized = stripped;
        }
      }
    }
  }
  while (normalized.startsWith('./') && normalized.length > 2) {
    normalized = normalized.slice(2);
  }
  if ((normalized.startsWith('a/') || normalized.startsWith('b/')) && normalized.length > 2) {
    const gitStripped = normalized.slice(2);
    if (
      root
        ? !existsSync(join(root, normalized)) && existsSync(join(root, gitStripped))
        : gitStripped.startsWith('src/') ||
          gitStripped.startsWith('tests/') ||
          gitStripped.startsWith('vscode-extension/')
    ) {
      normalized = gitStripped;
    }
  }
  if ((normalized.startsWith('@/') || normalized.startsWith('~/')) && normalized.length > 2) {
    const aliasedSubpath = `src/${normalized.slice(2)}`;
    if (!root || (!existsSync(join(root, normalized)) && existsSync(join(root, 'src')))) {
      normalized = aliasedSubpath;
    }
  } else if (normalized.startsWith('#') && normalized.length > 1 && normalized.includes('/')) {
    const aliasedSubpath = `src/${normalized.slice(1)}`;
    if (!root || (!existsSync(join(root, normalized)) && existsSync(join(root, 'src')))) {
      normalized = aliasedSubpath;
    }
  }
  if (normalized.length > 1 && normalized.endsWith('/')) {
    normalized = normalized.replace(/\/+$/u, '');
  }
  return normalized;
}

export function deduplicateAndMergeRanges(
  ranges: Array<{ path: string; start: number; end: number }>,
  splitRanges = true,
): Array<{ path: string; start: number; end: number }> {
  const expanded: Array<{ path: string; start: number; end: number }> = [];
  for (const r of ranges) {
    if (splitRanges && r.end - r.start >= 500) {
      let cursor = r.start;
      while (cursor <= r.end && expanded.length < 64) {
        const chunkEnd = Math.min(r.end, cursor + 499);
        expanded.push({ path: r.path, start: cursor, end: chunkEnd });
        cursor = chunkEnd + 1;
      }
    } else {
      expanded.push(r);
    }
  }
  if (expanded.length <= 1) {
    return expanded;
  }
  const merged: Array<{ path: string; start: number; end: number }> = [];
  for (const current of expanded) {
    const previous = merged.at(-1);
    if (
      previous &&
      previous.path === current.path &&
      current.start >= previous.start &&
      current.start <= previous.end + 1 &&
      Math.max(previous.end, current.end) - previous.start < 500
    ) {
      previous.end = Math.max(previous.end, current.end);
    } else if (
      !merged.some(
        (item) =>
          item.path === current.path && item.start <= current.start && item.end >= current.end,
      )
    ) {
      merged.push({ ...current });
    }
  }
  return merged.slice(0, 64);
}

export interface CopilotRootInput {
  root?: string | null;
  workspaceRoot?: string | null;
  workspace_root?: string | null;
  cwd?: string | null;
  projectRoot?: string | null;
  project_root?: string | null;
  repoRoot?: string | null;
  repo_root?: string | null;
}

export interface CopilotFilesInput extends CopilotRootInput {
  glob?: string | null;
  pattern?: string | null;
  includePattern?: string | null;
  include_pattern?: string | null;
  filePattern?: string | null;
  file_pattern?: string | null;
  glob_pattern?: string | null;
  globPattern?: string | null;
  file_glob?: string | null;
  fileGlob?: string | null;
  include_glob?: string | null;
  includeGlob?: string | null;
  include?: string | null;
  filter?: string | null;
  paths?: string[] | string | null;
  path?: string | null;
  file_path?: string | null;
  filePath?: string | null;
  filepath?: string | null;
  relative_path?: string | null;
  relativePath?: string | null;
  target_file?: string | null;
  targetFile?: string | null;
  dir?: string | null;
  directory?: string | null;
  folder?: string | null;
  limit?: number | null;
  maxResults?: number | null;
  max_results?: number | null;
  head_limit?: number | null;
}

export interface CopilotSearchInput extends CopilotRootInput {
  pattern?: string | null;
  query?: string | null;
  symbol?: string | null;
  symbol_name?: string | null;
  symbolName?: string | null;
  search?: string | null;
  search_query?: string | null;
  searchQuery?: string | null;
  regex_pattern?: string | null;
  regexPattern?: string | null;
  text?: string | null;
  needle?: string | null;
  regex?: boolean | null;
  isRegexp?: boolean | null;
  is_regex?: boolean | null;
  useRegex?: boolean | null;
  ignore_case?: boolean | null;
  caseInsensitive?: boolean | null;
  case_insensitive?: boolean | null;
  isCaseSensitive?: boolean | null;
  limit?: number | null;
  maxResults?: number | null;
  max_results?: number | null;
  head_limit?: number | null;
  max_matches_per_file?: number | null;
  maxMatchesPerFile?: number | null;
  paths?: string[] | string | null;
  path?: string | null;
  file_path?: string | null;
  filePath?: string | null;
  filepath?: string | null;
  relative_path?: string | null;
  relativePath?: string | null;
  target_file?: string | null;
  targetFile?: string | null;
  dir?: string | null;
  directory?: string | null;
  folder?: string | null;
  glob?: string | null;
  includePattern?: string | null;
  include_pattern?: string | null;
  filePattern?: string | null;
  file_pattern?: string | null;
  glob_pattern?: string | null;
  globPattern?: string | null;
  file_glob?: string | null;
  fileGlob?: string | null;
  include_glob?: string | null;
  includeGlob?: string | null;
  include?: string | null;
  filter?: string | null;
}

export interface CopilotContextInput extends CopilotSearchInput {
  radius?: number | null;
  contextLines?: number | null;
  context_lines?: number | null;
  context?: number | null;
}

export interface CopilotReadRangeInput {
  path?: string | null;
  filePath?: string | null;
  file_path?: string | null;
  filepath?: string | null;
  relative_path?: string | null;
  relativePath?: string | null;
  target_file?: string | null;
  targetFile?: string | null;
  uri?: string | null;
  file?: string | null;
  target?: string | null;
  start?: number | null;
  end?: number | null;
  start_line?: number | null;
  end_line?: number | null;
  startLine?: number | null;
  endLine?: number | null;
  start_line_number?: number | null;
  end_line_number?: number | null;
  startLineNumber?: number | null;
  endLineNumber?: number | null;
  from_line?: number | null;
  to_line?: number | null;
  fromLine?: number | null;
  toLine?: number | null;
  line?: number | null;
  line_number?: number | null;
  lineNumber?: number | null;
  around_line?: number | null;
  aroundLine?: number | null;
  center_line?: number | null;
  centerLine?: number | null;
  radius?: number | null;
  context_lines?: number | null;
  contextLines?: number | null;
  context?: number | null;
  offset?: number | null;
  limit?: number | null;
  numLines?: number | null;
  lineCount?: number | null;
  view_range?: readonly [number, number] | null;
}

export interface CopilotReadInput extends CopilotRootInput, CopilotReadRangeInput {
  split_ranges?: boolean | null;
  splitRanges?: boolean | null;
  page?: boolean | null;
  ranges?: CopilotReadRangeInput[] | null;
  paths?: string[] | string | null;
}

export function extractRawRootInput(input: CopilotRootInput | undefined): string | undefined {
  if (!input) {
    return undefined;
  }
  for (const candidate of [
    input.root,
    input.workspaceRoot,
    input.workspace_root,
    input.cwd,
    input.projectRoot,
    input.project_root,
    input.repoRoot,
    input.repo_root,
  ]) {
    if (typeof candidate === 'string' && candidate.trim().length > 0) {
      return candidate.trim();
    }
  }
  return undefined;
}

function splitPathWithEmbeddedGlob(
  rawPaths: string[] | string | null | undefined,
  root?: string | null,
): { paths: string[] | undefined; extractedGlob: string | undefined } {
  if (rawPaths === undefined || rawPaths === null) {
    return { paths: undefined, extractedGlob: undefined };
  }
  const items = Array.isArray(rawPaths)
    ? rawPaths
    : typeof rawPaths === 'string' && rawPaths.trim().length > 0
      ? [rawPaths]
      : [];
  const normalized: string[] = [];
  let extractedGlob: string | undefined;
  for (const item of items) {
    if (typeof item !== 'string' || item.trim().length === 0) {
      continue;
    }
    const cleaned = normalizeRelativeTargetPath(item, root);
    if (!cleaned) {
      continue;
    }
    if (/[*?[\]{}]/u.test(cleaned)) {
      const slashIdx = cleaned.search(/[*?[\]{}]/u);
      const beforeWildcard = cleaned.slice(0, slashIdx);
      const lastSlash = beforeWildcard.lastIndexOf('/');
      if (lastSlash > 0) {
        const prefix = beforeWildcard.slice(0, lastSlash);
        if (prefix && prefix !== '.') {
          normalized.push(prefix);
        }
      }
      extractedGlob ??= normalizeGlobPattern(cleaned);
    } else {
      normalized.push(cleaned);
    }
  }
  const unique = [...new Set(normalized)];
  if (unique.length > 1 && unique.includes('.')) {
    const filtered = unique.filter((p) => p !== '.');
    return { paths: filtered.length > 0 ? filtered : ['.'], extractedGlob };
  }
  return { paths: unique.length > 0 ? unique : undefined, extractedGlob };
}

export function buildFilesArguments(
  input: CopilotFilesInput,
  root?: string | null,
): Record<string, string | number | boolean | string[]> {
  const rawPathInput =
    input.paths ??
    input.path ??
    input.file_path ??
    input.filePath ??
    input.filepath ??
    input.relative_path ??
    input.relativePath ??
    input.target_file ??
    input.targetFile ??
    input.dir ??
    input.directory ??
    input.folder;
  const { paths: normalizedPaths, extractedGlob } = splitPathWithEmbeddedGlob(rawPathInput, root);
  const rawGlob =
    nonEmptyString(input.glob) ??
    nonEmptyString(input.includePattern) ??
    nonEmptyString(input.include_pattern) ??
    nonEmptyString(input.filePattern) ??
    nonEmptyString(input.file_pattern) ??
    nonEmptyString(input.glob_pattern) ??
    nonEmptyString(input.globPattern) ??
    nonEmptyString(input.file_glob) ??
    nonEmptyString(input.fileGlob) ??
    nonEmptyString(input.include_glob) ??
    nonEmptyString(input.includeGlob) ??
    nonEmptyString(input.include) ??
    nonEmptyString(input.filter) ??
    nonEmptyString(input.pattern) ??
    extractedGlob;
  const glob = normalizeGlobPattern(rawGlob ?? '*');
  const args: Record<string, string | number | boolean | string[]> = { glob };
  if (normalizedPaths !== undefined) {
    args.paths = normalizedPaths;
  }
  const rawLimit = input.limit ?? input.maxResults ?? input.max_results ?? input.head_limit;
  if (typeof rawLimit === 'number' && Number.isFinite(rawLimit)) {
    args.limit = Math.min(1000, Math.max(1, Math.trunc(rawLimit)));
  }
  return args;
}

export function buildSearchArguments(
  input: CopilotSearchInput,
  root?: string | null,
  maxLimit = 1000,
): Record<string, string | number | boolean | string[]> {
  const pattern =
    nonEmptyString(input.pattern) ??
    nonEmptyString(input.query) ??
    nonEmptyString(input.symbol) ??
    nonEmptyString(input.symbol_name) ??
    nonEmptyString(input.symbolName) ??
    nonEmptyString(input.search) ??
    nonEmptyString(input.search_query) ??
    nonEmptyString(input.searchQuery) ??
    nonEmptyString(input.regex_pattern) ??
    nonEmptyString(input.regexPattern) ??
    nonEmptyString(input.text) ??
    nonEmptyString(input.needle) ??
    '';
  const args: Record<string, string | number | boolean | string[]> = { pattern };
  const regexVal = input.regex ?? input.isRegexp ?? input.is_regex ?? input.useRegex;
  if (typeof regexVal === 'boolean') {
    args.regex = regexVal;
  }
  const ignoreCaseVal =
    input.ignore_case ??
    input.caseInsensitive ??
    input.case_insensitive ??
    (typeof input.isCaseSensitive === 'boolean' ? !input.isCaseSensitive : undefined);
  if (typeof ignoreCaseVal === 'boolean') {
    args.ignore_case = ignoreCaseVal;
  }
  const rawLimit = input.limit ?? input.maxResults ?? input.max_results ?? input.head_limit;
  if (typeof rawLimit === 'number' && Number.isFinite(rawLimit)) {
    args.limit = Math.min(maxLimit, Math.max(1, Math.trunc(rawLimit)));
  }
  const rawMaxPerFile = input.max_matches_per_file ?? input.maxMatchesPerFile;
  if (typeof rawMaxPerFile === 'number' && Number.isFinite(rawMaxPerFile)) {
    args.max_matches_per_file = Math.min(64, Math.max(1, Math.trunc(rawMaxPerFile)));
  }
  const rawPathInput =
    input.paths ??
    input.path ??
    input.file_path ??
    input.filePath ??
    input.filepath ??
    input.relative_path ??
    input.relativePath ??
    input.target_file ??
    input.targetFile ??
    input.dir ??
    input.directory ??
    input.folder;
  const { paths: normalizedPaths, extractedGlob } = splitPathWithEmbeddedGlob(rawPathInput, root);
  if (normalizedPaths !== undefined) {
    args.paths = normalizedPaths;
  }
  const rawGlob =
    nonEmptyString(input.glob) ??
    nonEmptyString(input.includePattern) ??
    nonEmptyString(input.include_pattern) ??
    nonEmptyString(input.filePattern) ??
    nonEmptyString(input.file_pattern) ??
    nonEmptyString(input.glob_pattern) ??
    nonEmptyString(input.globPattern) ??
    nonEmptyString(input.file_glob) ??
    nonEmptyString(input.fileGlob) ??
    nonEmptyString(input.include_glob) ??
    nonEmptyString(input.includeGlob) ??
    nonEmptyString(input.include) ??
    nonEmptyString(input.filter) ??
    extractedGlob;
  if (rawGlob !== undefined) {
    args.glob = normalizeGlobPattern(rawGlob);
  }
  return args;
}

export function buildContextArguments(
  input: CopilotContextInput,
  root?: string | null,
): Record<string, string | number | boolean | string[]> {
  const args = buildSearchArguments(input, root, 64);
  const rawRadius = input.radius ?? input.contextLines ?? input.context_lines ?? input.context;
  if (typeof rawRadius === 'number' && Number.isFinite(rawRadius)) {
    args.radius = Math.min(100, Math.max(0, Math.trunc(rawRadius)));
  }
  return args;
}

export function buildReadArguments(
  input: CopilotReadInput,
  root?: string | null,
): {
  split_ranges: boolean;
  page: boolean;
  ranges: Array<{ path: string; start: number; end: number }>;
} {
  let rawRanges: CopilotReadRangeInput[];
  if (Array.isArray(input.ranges) && input.ranges.length > 0) {
    rawRanges = input.ranges;
  } else if (Array.isArray(input.paths) && input.paths.length > 0) {
    rawRanges = input.paths
      .filter((p): p is string => typeof p === 'string' && p.trim().length > 0)
      .map((path) => ({
        ...input,
        path,
      }));
  } else if (
    typeof (
      input.path ??
      input.filePath ??
      input.file_path ??
      input.filepath ??
      input.relative_path ??
      input.relativePath ??
      input.target_file ??
      input.targetFile ??
      input.uri ??
      input.file ??
      input.target
    ) === 'string'
  ) {
    rawRanges = [input];
  } else {
    rawRanges = [];
  }
  const defaultSpan = rawRanges.length > 1 ? 79 : 119;
  const splitRanges = input.split_ranges ?? input.splitRanges ?? true;
  const normalized = rawRanges.map((range) => {
    const rawPath =
      nonEmptyString(range.path) ??
      nonEmptyString(range.filePath) ??
      nonEmptyString(range.file_path) ??
      nonEmptyString(range.filepath) ??
      nonEmptyString(range.relative_path) ??
      nonEmptyString(range.relativePath) ??
      nonEmptyString(range.target_file) ??
      nonEmptyString(range.targetFile) ??
      nonEmptyString(range.uri) ??
      nonEmptyString(range.file) ??
      nonEmptyString(range.target) ??
      '';
    const extracted = extractLineRangeFromPath(rawPath);
    const tupleStart = Array.isArray(range.view_range) ? range.view_range[0] : undefined;
    const tupleEnd = Array.isArray(range.view_range) ? range.view_range[1] : undefined;
    const explicitStart =
      positiveNumber(range.start) ??
      positiveNumber(range.start_line) ??
      positiveNumber(range.startLine) ??
      positiveNumber(range.start_line_number) ??
      positiveNumber(range.startLineNumber) ??
      positiveNumber(range.from_line) ??
      positiveNumber(range.fromLine) ??
      positiveNumber(range.offset) ??
      positiveNumber(tupleStart);
    const explicitEnd =
      positiveNumber(range.end) ??
      positiveNumber(range.end_line) ??
      positiveNumber(range.endLine) ??
      positiveNumber(range.end_line_number) ??
      positiveNumber(range.endLineNumber) ??
      positiveNumber(range.to_line) ??
      positiveNumber(range.toLine) ??
      positiveNumber(tupleEnd);
    const lineCandidate =
      positiveNumber(range.line) ??
      positiveNumber(range.line_number) ??
      positiveNumber(range.lineNumber);
    const centerCandidate =
      positiveNumber(range.around_line) ??
      positiveNumber(range.aroundLine) ??
      positiveNumber(range.center_line) ??
      positiveNumber(range.centerLine);
    const radiusCandidate =
      positiveNumber(range.radius) ??
      positiveNumber(range.context_lines) ??
      positiveNumber(range.contextLines) ??
      positiveNumber(range.context);
    const baseTargetPath = normalizeRelativeTargetPath(extracted.path, root);
    if (
      explicitStart === undefined &&
      explicitEnd === undefined &&
      (centerCandidate !== undefined || (lineCandidate !== undefined && radiusCandidate !== undefined))
    ) {
      const center = Math.trunc(centerCandidate ?? lineCandidate ?? 1);
      const rad = Math.trunc(radiusCandidate ?? 20);
      const s = Math.max(1, center - rad);
      const e = Math.max(s, center + rad);
      return { path: baseTargetPath, start: s, end: e };
    }
    const rawStart = explicitStart ?? lineCandidate ?? extracted.start ?? 1;
    const clampedStart = Math.max(1, Math.trunc(rawStart));
    const countSpan =
      positiveNumber(range.numLines) ??
      positiveNumber(range.lineCount) ??
      positiveNumber(range.limit);
    const rawEndCandidate =
      explicitEnd ??
      (countSpan !== undefined ? clampedStart + Math.trunc(countSpan) - 1 : undefined) ??
      extracted.end ??
      Math.max(clampedStart, clampedStart + defaultSpan);
    const clampedEnd = Math.max(1, Math.trunc(rawEndCandidate));
    const hasExplicitStart =
      explicitStart !== undefined || lineCandidate !== undefined || extracted.start !== undefined;
    const finalPath =
      extracted.anchor && !hasExplicitStart
        ? `${baseTargetPath}#${extracted.anchor}`
        : baseTargetPath;
    return {
      path: finalPath,
      start: Math.min(clampedStart, clampedEnd),
      end: Math.max(clampedStart, clampedEnd),
    };
  });
  return {
    split_ranges: Boolean(splitRanges),
    page: Boolean(input.page ?? false),
    ranges: deduplicateAndMergeRanges(normalized, Boolean(splitRanges)),
  };
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : undefined;
}

function positiveNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : undefined;
}
