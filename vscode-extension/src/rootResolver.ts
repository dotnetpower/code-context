import { isAbsolute, relative, resolve, sep, posix } from 'node:path';

import type { WorkspaceSpec } from './types.js';

export class RootResolutionError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = 'RootResolutionError';
  }
}

export function resolveWslRoot(
  workspaces: readonly WorkspaceSpec[],
  explicitPath: string | undefined,
): string {
  const target = explicitPath ?? (workspaces.length === 1 ? workspaces[0]?.root : undefined);
  if (!target) {
    throw new RootResolutionError(
      'Windows Agent Sessions require an explicit absolute Linux root when multiple roots are registered.',
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
): string {
  const roots = workspaces.map((workspace) => resolve(workspace.root));
  if (explicitPath !== undefined) {
    if (!isAbsolute(explicitPath)) {
      throw new RootResolutionError('Code Context root must be an absolute path.');
    }
    const match = bestContainingRoot(roots, resolve(explicitPath));
    if (!match) {
      throw new RootResolutionError(
        `No registered Code Context workspace contains ${explicitPath}. Run cctx register first.`,
      );
    }
    return match;
  }
  for (const candidate of candidates) {
    const match = bestContainingRoot(roots, resolve(candidate));
    if (match) {
      return match;
    }
  }
  if (roots.length === 1) {
    return roots[0] as string;
  }
  if (roots.length === 0) {
    throw new RootResolutionError(
      'No Code Context workspaces are registered. Register the current workspace first.',
    );
  }
  throw new RootResolutionError(
    'The current VS Code workspace is not registered with Code Context.',
  );
}

export async function resolveRegisteredRootAfterRefresh(
  workspaces: readonly WorkspaceSpec[],
  refresh: () => Promise<readonly WorkspaceSpec[]>,
  explicitPath: string | undefined,
  candidates: readonly string[],
  registerMissingRoot?: (path: string) => Promise<void>,
): Promise<string> {
  const preferred = candidates[0];
  if (explicitPath === undefined && preferred && isAbsolute(preferred) && registerMissingRoot &&
    !bestContainingRoot(workspaces.map((workspace) => resolve(workspace.root)), preferred)) {
    const refreshed = await refresh();
    if (!bestContainingRoot(refreshed.map((workspace) => resolve(workspace.root)), preferred)) {
      await registerMissingRoot(preferred);
      return resolveRegisteredRoot(await refresh(), preferred, []);
    }
    return resolveRegisteredRoot(refreshed, preferred, []);
  }
  try {
    return resolveRegisteredRoot(workspaces, explicitPath, candidates);
  } catch (error: unknown) {
    if (!(error instanceof RootResolutionError)) {
      throw error;
    }
    const refreshed = await refresh();
    try {
      return resolveRegisteredRoot(refreshed, explicitPath, candidates);
    } catch (refreshedError: unknown) {
      if (!(refreshedError instanceof RootResolutionError) || !registerMissingRoot) {
        throw refreshedError;
      }
      const target = explicitPath ?? candidates[0];
      if (!target || !isAbsolute(target)) {
        throw refreshedError;
      }
      await registerMissingRoot(target);
      return resolveRegisteredRoot(await refresh(), explicitPath, candidates);
    }
  }
}

function bestContainingRoot(roots: readonly string[], candidate: string): string | undefined {
  return roots
    .filter((root) => contains(root, candidate))
    .sort((left, right) => right.length - left.length)[0];
}

function contains(root: string, candidate: string): boolean {
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
