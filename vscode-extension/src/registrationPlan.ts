import { basename, extname, isAbsolute, resolve } from 'node:path';

import type { WorkspaceSpec } from './types.js';

export interface RegistrationPlan {
  root: string;
  scopes: string[];
  globs: string[];
}

export interface ValidatedPlanResult {
  root: string;
  scopes: string[];
  globs: string[];
  files: number;
  metadata_source_bytes: number;
  watch_directories: number;
  detected_profiles: string[];
}

export type AutomaticPlanRegistry = Record<string, RegistrationPlan>;
export const MAX_AUTOMATIC_PLANS = 64;

const SOURCE_EXTENSION_LIST = [
  'bash',
  'c',
  'cc',
  'cjs',
  'clj',
  'cljc',
  'cljs',
  'cpp',
  'cs',
  'cxx',
  'dart',
  'erl',
  'ex',
  'exs',
  'fs',
  'go',
  'gradle',
  'groovy',
  'h',
  'hh',
  'hpp',
  'hs',
  'hrl',
  'hxx',
  'java',
  'jl',
  'js',
  'jsx',
  'kt',
  'kts',
  'lua',
  'mjs',
  'ml',
  'mli',
  'nim',
  'php',
  'ps1',
  'py',
  'pyi',
  'r',
  'rb',
  'rs',
  'scala',
  'sh',
  'sql',
  'swift',
  'tf',
  'ts',
  'tsx',
  'vb',
  'zig',
  'zsh',
  'md',
  'mdx',
  'rst',
  'txt',
  'json',
  'jsonc',
  'yaml',
  'yml',
  'toml',
  'ini',
  'cfg',
  'xml',
  'html',
  'css',
  'scss',
  'vue',
  'svelte',
] as const;
const SOURCE_EXTENSIONS = new Set<string>(SOURCE_EXTENSION_LIST);
const SOURCE_GLOB_CASE_ALIASES = ['*.C', '*.H', '*.R'] as const;
const DEVELOPMENT_FILENAMES = ['Makefile', 'GNUmakefile', 'Dockerfile', 'Containerfile', 'CMakeLists.txt', 'Justfile'] as const;

export function inferScopes(): string[] {
  return ['.'];
}

export function inferGlobs(paths: readonly string[]): string[] {
  const extensions = new Set<string>();
  for (const path of paths) {
    const extension = extname(path).slice(1);
    if (SOURCE_EXTENSIONS.has(extension.toLowerCase())) {
      extensions.add(extension);
    }
  }
  return [...new Set([
    ...paths.filter((path) => DEVELOPMENT_FILENAMES.some((name) => name === basename(path))).map((path) => basename(path)),
    ...[...extensions]
    .sort((left, right) => left.localeCompare(right))
    .map((extension) => `*.${extension}`),
  ])].sort((left, right) => left.localeCompare(right));
}

export function allSourceGlobs(): string[] {
  return [
    ...SOURCE_EXTENSION_LIST.map((extension) => `*.${extension}`),
    ...SOURCE_GLOB_CASE_ALIASES,
    ...DEVELOPMENT_FILENAMES,
  ];
}

export function isSupportedSourcePath(path: string): boolean {
  return (
    !hasHiddenComponent(path) &&
    (SOURCE_EXTENSIONS.has(extname(path).slice(1).toLowerCase()) ||
      DEVELOPMENT_FILENAMES.some((name) => name === basename(path)))
  );
}

export function rootSourceSearchPattern(): string {
  const extensions = [
    ...SOURCE_EXTENSION_LIST,
    ...SOURCE_GLOB_CASE_ALIASES.map((glob) => glob.slice(2)),
  ];
  return `{${[...extensions.map((extension) => `*.${extension}`), ...DEVELOPMENT_FILENAMES].join(',')}}`;
}

export function sourceSearchPatterns(): string[] {
  const root = rootSourceSearchPattern();
  return [root, `**/${root}`];
}

export function mergeAutomaticPlan(
  inferred: RegistrationPlan,
  existing: WorkspaceSpec,
): RegistrationPlan {
  return {
    root: inferred.root,
    scopes: ['.'],
    globs: [...new Set([...existing.globs, ...inferred.globs])].sort((left, right) =>
      left.localeCompare(right),
    ),
  };
}

export function registrationNeedsUpdate(
  plan: RegistrationPlan,
  existing: WorkspaceSpec,
): boolean {
  return !sameStrings(plan.scopes, existing.scopes) || !sameStrings(plan.globs, existing.globs);
}

export function canUpdateAutomaticRegistration(
  existing: WorkspaceSpec,
  remembered: RegistrationPlan | undefined,
): boolean {
  return (
    remembered !== undefined &&
    sameStrings(existing.scopes, remembered.scopes) &&
    sameStrings(existing.globs, remembered.globs)
  );
}

export function normalizeAutomaticPlanRegistry(value: unknown): AutomaticPlanRegistry {
  if (!isRecord(value)) {
    return {};
  }
  const normalized: AutomaticPlanRegistry = {};
  for (const [root, plan] of Object.entries(value).slice(-MAX_AUTOMATIC_PLANS)) {
    if (!isRecord(plan) || !isAbsolute(root) || plan.root !== root) {
      continue;
    }
    const scopes = stringArray(plan.scopes);
    const globs = stringArray(plan.globs);
    if (!scopes || !globs) {
      continue;
    }
    normalized[resolve(root)] = { root: resolve(root), scopes, globs };
  }
  return normalized;
}

export function rememberAutomaticPlan(
  registry: AutomaticPlanRegistry,
  plan: RegistrationPlan,
): AutomaticPlanRegistry {
  const root = resolve(plan.root);
  const next = { ...registry };
  delete next[root];
  next[root] = { root, scopes: [...plan.scopes], globs: [...plan.globs] };
  while (Object.keys(next).length > MAX_AUTOMATIC_PLANS) {
    const oldest = Object.keys(next)[0];
    if (oldest === undefined) {
      break;
    }
    delete next[oldest];
  }
  return next;
}

export function validatePlanResult(
  expected: RegistrationPlan,
  value: unknown,
): ValidatedPlanResult {
  if (!isRecord(value)) {
    throw new Error('Code Context plan returned a non-object result.');
  }
  const root = value.root;
  const scopes = stringArray(value.scopes);
  const globs = stringArray(value.globs);
  const profiles = optionalStringArray(value.detected_profiles);
  if (
    typeof root !== 'string' ||
    !isAbsolute(root) ||
    resolve(root) !== resolve(expected.root)
  ) {
    throw new Error('Code Context plan returned an invalid root.');
  }
  if (!scopes || !sameStrings(scopes, expected.scopes)) {
    throw new Error('Code Context plan returned unexpected scopes.');
  }
  if (!globs || !sameStrings(globs, expected.globs)) {
    throw new Error('Code Context plan returned unexpected globs.');
  }
  const files = nonNegativeInteger(value.files, 'files');
  if (files === 0) {
    throw new Error('Code Context plan contains no admitted source files.');
  }
  return {
    root,
    scopes,
    globs,
    files,
    metadata_source_bytes: nonNegativeInteger(
      value.metadata_source_bytes,
      'metadata_source_bytes',
    ),
    watch_directories: nonNegativeInteger(
      value.watch_directories,
      'watch_directories',
    ),
    detected_profiles: profiles,
  };
}

export function validateRegisterResult(
  expected: RegistrationPlan,
  value: unknown,
): ValidatedPlanResult {
  if (!isRecord(value) || value.registered !== true) {
    throw new Error('Code Context register did not confirm registration.');
  }
  return validatePlanResult(expected, value.plan);
}

function sameStrings(left: readonly string[], right: readonly string[]): boolean {
  const sortedRight = [...right].sort();
  return (
    left.length === right.length &&
    [...left].sort().every((value, index) => value === sortedRight[index])
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value) || value.length === 0 || value.length > 128) {
    return undefined;
  }
  const strings: string[] = [];
  for (const item of value) {
    if (typeof item !== 'string' || item.length === 0) {
      return undefined;
    }
    strings.push(item);
  }
  return strings;
}

function optionalStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) {
    throw new Error('Code Context plan returned invalid detected_profiles.');
  }
  const strings: string[] = [];
  for (const item of value) {
    if (typeof item !== 'string' || item.length === 0) {
      throw new Error('Code Context plan returned invalid detected_profiles.');
    }
    strings.push(item);
  }
  return strings;
}

function nonNegativeInteger(value: unknown, label: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`Code Context plan returned invalid ${label}.`);
  }
  return value;
}

function hasHiddenComponent(path: string): boolean {
  return path
    .split(/[\\/]/u)
    .filter(Boolean)
    .some((component) => component.startsWith('.'));
}
