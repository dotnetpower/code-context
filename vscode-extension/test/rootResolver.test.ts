import assert from 'node:assert/strict';
import test from 'node:test';
import { execFileSync } from 'node:child_process';
import { mkdtemp, realpath, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  buildContextArguments,
  buildFilesArguments,
  buildReadArguments,
  buildSearchArguments,
  extractLineRangeFromPath,
  normalizeGlobPattern,
  normalizeRelativeTargetPath,
  resolveRegisteredRoot,
  resolveRegisteredRootAfterRefresh,
  listedWorktreeRoot,
  parseGitIdentity,
  wslCandidateToLinuxPath,
} from '../src/rootResolver.js';
import type { WorkspaceSpec } from '../src/types.js';

const workspaces: WorkspaceSpec[] = [
  { root: '/work', scopes: ['src'], globs: ['*.ts'] },
  { root: '/work/nested', scopes: ['src'], globs: ['*.rs'] },
  { root: '/other', scopes: ['api'], globs: ['*.py'] },
];

void test('requested Git identity preserves spaces and rejects malformed or relative paths',()=>{
  assert.deepEqual(parseGitIdentity('/repo.worktrees/linked workspace\n/repo/.git\n'),{
    root:'/repo.worktrees/linked workspace',commonDirectory:'/repo/.git',
  });
  for (const output of ['', '/repo\n', '/repo\nrelative\n',
    '/repo\n/repo/.git\n/unrequested\n', '"/quoted"\n/repo/.git\n',
    '/repo\r\n/repo/.git\n']) {
    assert.throws(()=>parseGitIdentity(output),/exactly two/);
  }
});

void test('explicit subdirectory resolves to the longest registered ancestor', () => {
  assert.equal(
    resolveRegisteredRoot(workspaces, '/work/nested/src/lib.rs', []),
    '/work/nested',
  );
});

void test('active editor candidate wins before other workspace folders', () => {
  assert.equal(
    resolveRegisteredRoot(workspaces, undefined, [
      '/other/api/main.py',
      '/work/src/main.ts',
    ]),
    '/other',
  );
});

void test('a single registry entry resolves without editor context', () => {
  assert.equal(
    resolveRegisteredRoot([workspaces[0] as WorkspaceSpec], undefined, []),
    '/work',
  );
});

void test('relative explicit roots and unregistered paths fail explicitly', () => {
  assert.throws(
    () => resolveRegisteredRoot(workspaces, 'relative/path', []),
    /absolute path/,
  );
  assert.throws(
    () => resolveRegisteredRoot(workspaces, '/missing/path', []),
    /No registered/,
  );
});

void test('ambiguous registry without matching editor requires an explicit root', () => {
  assert.throws(
    () => resolveRegisteredRoot(workspaces, undefined, ['/unregistered']),
    /current VS Code workspace is not registered/,
  );
  assert.equal(resolveRegisteredRoot(workspaces, 'other', []), '/other');
});

void test('path-prefix collisions are not treated as descendants', () => {
  assert.throws(
    () =>
      resolveRegisteredRoot(
        [{ root: '/work/app', scopes: ['src'], globs: ['*.ts'] }],
        '/work/application/source.ts',
        [],
      ),
    /No registered/,
  );
});

void test('registry refresh can resolve a newly registered workspace', async () => {
  assert.equal(
    await resolveRegisteredRootAfterRefresh(
      [],
      () => Promise.resolve(workspaces),
      '/work/src/main.ts',
      [],
    ),
    '/work',
  );
});

void test('registry refresh reports the current error rather than the stale error', async () => {
  await assert.rejects(
    resolveRegisteredRootAfterRefresh(
      [],
      () => Promise.resolve(workspaces),
      '/still-missing/source.ts',
      [],
    ),
    /No registered Code Context workspace contains/,
  );
});

void test('missing root registers a verified worktree then refreshes once', async () => {
  let registered = false;
  assert.equal(await resolveRegisteredRootAfterRefresh(
    [], () => Promise.resolve(registered ? workspaces : []),
    '/work/src', [], (path) => {
      assert.equal(path, '/work/src');
      registered = true;
      return Promise.resolve();
    },
  ), '/work');
});

void test('unregistered active worktree is not silently redirected to the main repository', async () => {
  let registered = false;
  const main = [{ root: '/repo', scopes: ['.'], globs: ['*.ts'] }];
  const linked = { root: '/repo.worktrees/one', scopes: ['.'], globs: ['*.ts'] };
  const root = await resolveRegisteredRootAfterRefresh(
    main, () => Promise.resolve(registered ? [...main, linked] : main),
    undefined, ['/repo.worktrees/one/src/main.ts', '/repo'], (path) => {
      assert.equal(path, '/repo.worktrees/one/src/main.ts');
      registered = true;
      return Promise.resolve();
    },
  );
  assert.equal(root, linked.root);
});

void test('worktree discovery uses NUL-delimited exact repository membership', () => {
  const listed = 'worktree /repo\0HEAD abc\0branch refs/heads/main\0\0worktree /repo.worktrees/one\0HEAD def\0\0';
  assert.equal(listedWorktreeRoot(listed, '/repo.worktrees/one/src'), '/repo.worktrees/one');
  assert.throws(() => listedWorktreeRoot(listed, '/repo.worktrees/other'), /not a Git worktree/);
  assert.throws(() => listedWorktreeRoot(listed, '/repo.worktrees/one-evil'), /not a Git worktree/);
  assert.throws(() => listedWorktreeRoot(listed, 'relative'), /absolute/);
});

void test('rejected registration propagates without silently searching another root', async () => {
  await assert.rejects(resolveRegisteredRootAfterRefresh(
    workspaces, () => Promise.resolve(workspaces), '/untrusted', [],
    () => Promise.reject(new Error('not trusted')),
  ), /not trusted/);
});

void test('real Git worktree output authorizes siblings but not unrelated repositories', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'cctx-worktrees-'));
  try {
    const root = await realpath(directory);
    const repo = join(root, 'repo');
    const worktree = join(root, 'linked workspace');
    execFileSync('git', ['init', '-q', repo]);
    execFileSync('git', ['-C', repo, '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
      'commit', '-q', '--allow-empty', '-m', 'fixture']);
    execFileSync('git', ['-C', repo, 'worktree', 'add', '-q', '-b', 'test-linked', worktree]);
    const output = execFileSync('git', ['-C', repo, 'worktree', 'list', '--porcelain'], { encoding: 'utf8' });
    assert.equal(listedWorktreeRoot(output, join(worktree, 'src')), worktree);
    assert.throws(() => listedWorktreeRoot(output, join(root, 'unrelated')), /not a Git worktree/);
    const identity=(directory:string):{root:string;commonDirectory:string}=>parseGitIdentity(
      execFileSync('git',['-C',directory,'rev-parse','--path-format=absolute',
        '--show-toplevel','--git-common-dir'],{encoding:'utf8'}));
    const parent=identity(repo);
    const requested=identity(worktree);
    assert.equal(requested.root,worktree);
    assert.equal(requested.commonDirectory,parent.commonDirectory);
    const unrelated=join(root,'unrelated');
    execFileSync('git',['init','-q',unrelated]);
    assert.notEqual(identity(unrelated).commonDirectory,parent.commonDirectory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

void test('frontend Vite query suffixes, tsc coordinates, @/ aliases, and compound globs normalize cleanly', () => {
  assert.deepEqual(extractLineRangeFromPath('src/assets/icon.svg?react'), {
    path: 'src/assets/icon.svg',
  });
  assert.deepEqual(extractLineRangeFromPath('src/App.tsx:42:15'), {
    path: 'src/App.tsx',
    start: 42,
    end: 82,
  });
  assert.deepEqual(extractLineRangeFromPath('src/App.tsx(18,5)'), {
    path: 'src/App.tsx',
    start: 18,
    end: 58,
  });
  assert.equal(normalizeGlobPattern('./**/*.{ts,tsx}'), '*.{ts,tsx}');
  assert.equal(normalizeGlobPattern('**/*.module.css'), '*.module.css');
  assert.equal(normalizeGlobPattern('**/*.test.tsx'), '*.test.tsx');
  assert.equal(normalizeRelativeTargetPath('@/components/Button.tsx?react'), 'src/components/Button.tsx');
  assert.equal(normalizeRelativeTargetPath('~/hooks/useAuth.ts'), 'src/hooks/useAuth.ts');
  assert.equal(normalizeRelativeTargetPath('b/src/main.rs'), 'src/main.rs');
  assert.deepEqual(extractLineRangeFromPath('[App.tsx](/repo/src/App.tsx#L10C5-L20C2)'), {
    path: '/repo/src/App.tsx',
    start: 10,
    end: 20,
  });
  assert.deepEqual(extractLineRangeFromPath('[Setup](docs/SETUP.md#windows-agent-sessions)'), {
    path: 'docs/SETUP.md',
    anchor: 'windows-agent-sessions',
  });
  const mdAnchorRead = buildReadArguments({
    path: '[Setup](docs/SETUP.md#windows-agent-sessions)',
  });
  assert.deepEqual(mdAnchorRead.ranges, [
    { path: 'docs/SETUP.md#windows-agent-sessions', start: 1, end: 120 },
  ]);
  assert.equal(normalizeGlobPattern('*.ts, *.tsx'), '*.{ts,tsx}');
  assert.equal(normalizeGlobPattern('*.{ts, tsx, js}'), '*.{ts,tsx,js}');
  assert.equal(wslCandidateToLinuxPath('file://wsl.localhost/Ubuntu/home/user/repo'), '/home/user/repo');

  assert.deepEqual(
    buildFilesArguments({
      includePattern: null,
      paths: ['src/**/*.tsx'],
      maxResults: 50,
    }),
    {
      glob: 'src/**/*.tsx',
      paths: ['src'],
      limit: 50,
    },
  );
  assert.deepEqual(
    buildSearchArguments({
      symbol: 'useAuth',
      isRegexp: true,
      isCaseSensitive: false,
      maxResults: 25,
      maxMatchesPerFile: 3,
      glob: '',
    }),
    {
      pattern: 'useAuth',
      regex: true,
      ignore_case: true,
      limit: 25,
      max_matches_per_file: 3,
    },
  );
  assert.deepEqual(
    buildContextArguments({
      needle: 'Button',
      contextLines: 8,
    }),
    {
      pattern: 'Button',
      radius: 8,
    },
  );
  const readBuilt = buildReadArguments({
    paths: ['[a.ts](src/a.ts)', '`src/b.ts`'],
    view_range: [10, -1],
  });
  assert.deepEqual(readBuilt.ranges, [
    { path: 'src/a.ts', start: 10, end: 89 },
    { path: 'src/b.ts', start: 10, end: 89 },
  ]);
  const splitBuilt = buildReadArguments({
    path: 'src/large.ts',
    start: 1,
    end: 1200,
  });
  assert.equal(splitBuilt.ranges.length, 3);
  assert.deepEqual(splitBuilt.ranges[0], { path: 'src/large.ts', start: 1, end: 500 });
  assert.deepEqual(splitBuilt.ranges[1], { path: 'src/large.ts', start: 501, end: 1000 });
  assert.deepEqual(splitBuilt.ranges[2], { path: 'src/large.ts', start: 1001, end: 1200 });
});
