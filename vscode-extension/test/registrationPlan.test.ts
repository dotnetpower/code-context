import assert from 'node:assert/strict';
import test from 'node:test';

import {
  allSourceGlobs,
  canUpdateAutomaticRegistration,
  inferGlobs,
  inferScopes,
  isSupportedSourcePath,
  mergeAutomaticPlan,
  MAX_AUTOMATIC_PLANS,
  normalizeAutomaticPlanRegistry,
  rememberAutomaticPlan,
  registrationNeedsUpdate,
  rootSourceSearchPattern,
  sourceSearchPatterns,
  validatePlanResult,
  validateRegisterResult,
} from '../src/registrationPlan.js';

void test('automatic scope covers future nested source directories', () => {
  assert.deepEqual(
    inferScopes(),
    ['.'],
  );
});

void test('automatic scope remains stable for unknown project layouts', () => {
  assert.deepEqual(inferScopes(), ['.']);
});

void test('root-level code selects the complete workspace scope', () => {
  assert.deepEqual(
    inferScopes(),
    ['.'],
  );
});

void test('observed source directories do not freeze the registration boundary', () => {
  assert.deepEqual(
    inferScopes(),
    ['.'],
  );
});

void test('source glob inference includes safe documentation and configuration', () => {
  assert.deepEqual(
    inferGlobs([
      '/workspace/src/main.ts',
      '/workspace/src/App.tsx',
      '/workspace/src/again.ts',
      '/workspace/README.md',
      '/workspace/image.png',
      '/workspace/api/main.py',
    ]),
    ['*.md', '*.py', '*.ts', '*.tsx'],
  );
});

void test('automatic globs cover future supported source languages', () => {
  const globs = allSourceGlobs();
  assert.ok(globs.includes('*.rs'));
  assert.ok(globs.includes('*.ts'));
  assert.ok(globs.includes('*.go'));
  assert.ok(globs.includes('*.java'));
  assert.ok(globs.includes('*.R'));
  assert.equal(new Set(globs).size, globs.length);
  for (const extension of ['md', 'json', 'yaml', 'yml', 'toml', 'jsonc']) {
    assert.ok(globs.includes(`*.${extension}`));
  }
  assert.ok(globs.length <= 128);
});

void test('root source search pattern includes Rust and TypeScript', () => {
  const pattern = rootSourceSearchPattern();
  assert.match(pattern, /rs/);
  assert.match(pattern, /tsx/);
  assert.match(pattern, /R/);
  assert.deepEqual(sourceSearchPatterns(), [pattern, `**/${pattern}`]);
});

void test('hidden source paths never become registration scopes', () => {
  assert.equal(isSupportedSourcePath('.fdai/hooks/check.py'), false);
  assert.equal(isSupportedSourcePath('src/.generated/file.rs'), false);
  assert.equal(isSupportedSourcePath('src/main.rs'), true);
  assert.equal(isSupportedSourcePath('Makefile'),true);
  assert.equal(isSupportedSourcePath('services/Dockerfile'),true);
  assert.equal(isSupportedSourcePath('.github/Makefile'),false);
  assert.ok(allSourceGlobs().includes('Makefile'));
  assert.deepEqual(inferGlobs(['/work/Makefile','/work/Dockerfile']),['Dockerfile','Makefile']);
});

void test('automatic plan widens legacy scopes and preserves known globs', () => {
  const plan = mergeAutomaticPlan(
    {
      root: '/workspace',
      scopes: ['.'],
      globs: ['*.rs', '*.ts'],
    },
    {
      root: '/workspace',
      scopes: ['src', 'tests'],
      globs: ['*.rs'],
      prewarm: true,
    },
  );
  assert.deepEqual(plan, {
    root: '/workspace',
    scopes: ['.'],
    globs: ['*.rs', '*.ts'],
  });
  assert.equal(
    registrationNeedsUpdate(plan, {
      root: '/workspace',
      scopes: ['src', 'tests'],
      globs: ['*.rs'],
    }),
    true,
  );
});

void test('automatic plan does not rewrite an equivalent registration', () => {
  assert.equal(
    registrationNeedsUpdate(
      { root: '/workspace', scopes: ['.'], globs: ['*.rs', '*.ts'] },
      {
        root: '/workspace',
        scopes: ['.'],
        globs: ['*.ts', '*.rs'],
      },
    ),
    false,
  );
});

void test('automatic updates require an unchanged remembered plan', () => {
  const existing = {
    root: '/workspace',
    scopes: ['.'],
    globs: ['*.rs', '*.ts'],
  };
  assert.equal(
    canUpdateAutomaticRegistration(existing, {
      root: '/workspace',
      scopes: ['.'],
      globs: ['*.ts', '*.rs'],
    }),
    true,
  );
  assert.equal(canUpdateAutomaticRegistration(existing, undefined), false);
  assert.equal(
    canUpdateAutomaticRegistration(
      { ...existing, scopes: ['src'] },
      {
        root: '/workspace',
        scopes: ['.'],
        globs: ['*.rs', '*.ts'],
      },
    ),
    false,
  );
});

void test('automatic registration state rejects malformed entries', () => {
  assert.deepEqual(normalizeAutomaticPlanRegistry(null), {});
  assert.deepEqual(
    normalizeAutomaticPlanRegistry({
      relative: { root: 'relative', scopes: ['.'], globs: ['*.ts'] },
      '/wrong': { root: '/different', scopes: ['.'], globs: ['*.ts'] },
      '/missing-globs': { root: '/missing-globs', scopes: ['.'] },
      '/valid': { root: '/valid', scopes: ['.'], globs: ['*.ts'] },
    }),
    {
      '/valid': { root: '/valid', scopes: ['.'], globs: ['*.ts'] },
    },
  );
});

void test('automatic registration state remains bounded and refreshes recency', () => {
  let registry = {};
  for (let index = 0; index <= MAX_AUTOMATIC_PLANS; index += 1) {
    registry = rememberAutomaticPlan(registry, {
      root: `/workspace-${index}`,
      scopes: ['.'],
      globs: ['*.ts'],
    });
  }
  assert.equal(Object.keys(registry).length, MAX_AUTOMATIC_PLANS);
  assert.equal('/workspace-0' in registry, false);
  registry = rememberAutomaticPlan(registry, {
    root: '/workspace-1',
    scopes: ['.'],
    globs: ['*.ts'],
  });
  assert.equal(Object.keys(registry).at(-1), '/workspace-1');
});

void test('plan and register responses require complete matching contracts', () => {
  const plan = { root: '/workspace', scopes: ['.'], globs: ['*.rs', '*.ts'] };
  const result = {
    root: '/workspace',
    scopes: ['.'],
    globs: ['*.ts', '*.rs'],
    files: 2,
    metadata_source_bytes: 100,
    watch_directories: 1,
    detected_profiles: ['rust'],
  };
  assert.deepEqual(validatePlanResult(plan, result), result);
  assert.deepEqual(
    validateRegisterResult(plan, { registered: true, plan: result }),
    result,
  );
  assert.throws(
    () => validatePlanResult(plan, { ...result, files: 0 }),
    /no admitted source files/,
  );
  assert.throws(
    () => validatePlanResult(plan, { ...result, globs: ['*.rs'] }),
    /unexpected globs/,
  );
  assert.throws(
    () => validatePlanResult(plan, { ...result, root: '/different' }),
    /invalid root/,
  );
  assert.throws(
    () => validatePlanResult(plan, { ...result, watch_directories: -1 }),
    /invalid watch_directories/,
  );
  assert.throws(
    () => validatePlanResult(plan, { ...result, detected_profiles: [1] }),
    /invalid detected_profiles/,
  );
  assert.throws(
    () => validateRegisterResult(plan, { registered: false, plan: result }),
    /did not confirm/,
  );
});
