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
  const uncMatch = /^\\\\(?:wsl\.localhost|wsl\$)\\[^\\]+(\\.+)$/iu.exec(candidate);
  if (uncMatch?.[1]) {
    const converted = uncMatch[1].replaceAll('\\', '/');
    return posix.isAbsolute(converted) ? posix.normalize(converted) : undefined;
  }
  if (posix.isAbsolute(candidate) && !/[\\\0\r\n]/u.test(candidate)) {
    return posix.normalize(candidate);
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
    const cleaned = rawTarget.replaceAll('\\', '/').replace(/^(?:\.\/)+/u, '').replace(/\/+$/u, '');
    if (!cleaned || cleaned.startsWith('/') || cleaned.startsWith('..')) {
      continue;
    }
    const firstSegment = cleaned.split('/')[0];
    if (firstSegment && cleaned.includes('/')) {
      const rest = cleaned.slice(firstSegment.length + 1);
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
      existsSync(usePosix ? posix.join(root, cleaned) : join(root, cleaned)),
    );
    if (existingMatches.length === 1 && existingMatches[0] !== undefined) {
      return existingMatches[0];
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
