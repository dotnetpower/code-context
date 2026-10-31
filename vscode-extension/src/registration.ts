import { execFile } from 'node:child_process';
import { realpath } from 'node:fs/promises';
import { isAbsolute, relative, sep } from 'node:path';
import { promisify } from 'node:util';

import * as vscode from 'vscode';

import type { NativeToolBridge } from './nativeTools.js';
import { normalizeExecutablePath } from './nativeTransport.js';
import {
  allSourceGlobs,
  type AutomaticPlanRegistry,
  canUpdateAutomaticRegistration,
  inferScopes,
  mergeAutomaticPlan,
  normalizeAutomaticPlanRegistry,
  rememberAutomaticPlan,
  type RegistrationPlan,
  registrationNeedsUpdate,
  sourceSearchPatterns,
  type ValidatedPlanResult,
  validatePlanResult,
  validateRegisterResult,
} from './registrationPlan.js';
import { normalizeLocalPath, parseGitIdentity, resolveRegisteredRoot, RootResolutionError } from './rootResolver.js';
import type { JsonValue } from './types.js';
import { singleLine } from './logFormatter.js';

const execFileAsync = promisify(execFile);
const SEARCH_EXCLUDE =
  '**/{node_modules,target,dist,build,out,coverage,__pycache__,venv,.venv,.git}/**';
const AUTO_PLAN_STATE_KEY = 'automaticRegistrationPlans';
let registrationQueue: Promise<string[]> = Promise.resolve([]);
const inFlightMissingWorkspaces = new Map<string, Promise<void>>();

export async function registerCurrentWorkspace(
  bridge: NativeToolBridge,
  output: vscode.LogOutputChannel,
): Promise<boolean> {
  if (!vscode.workspace.isTrusted) {
    throw new Error('Trust this workspace before registering Code Context.');
  }
  const folder = await chooseWorkspaceFolder();
  if (!folder) {
    return false;
  }
  let plan = await inferPlan(folder);
  const executable = executablePath();
  while (true) {
    const planResult = await runCctxPlan(executable, plan);
    const confirmation = await vscode.window.showInformationMessage(
      `Register ${folder.name} with Code Context?`,
      {
        modal: true,
        detail: planSummary(plan, planResult),
      },
      'Register',
      'Customize',
    );
    if (confirmation === 'Customize') {
      const customized = await customizePlan(plan);
      if (!customized) {
        return false;
      }
      plan = customized;
      continue;
    }
    if (confirmation !== 'Register') {
      return false;
    }
    break;
  }
  const registered = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `Registering ${folder.name} with Code Context`,
      cancellable: false,
    },
    async () => runCctxRegister(executable, plan),
  );
  bridge.invalidateRegistry();
  output.info(`REGISTERED root=${singleLine(registered.root)} ${compactPlanSummary(registered)}`);
  await vscode.window.showInformationMessage(
    `Code Context registered: ${folder.uri.fsPath}`,
  );
  return true;
}

export async function autoRegisterWorkspaceFolders(
  bridge: NativeToolBridge,
  output: vscode.LogOutputChannel,
  state: vscode.Memento,
  folders: readonly vscode.WorkspaceFolder[] = vscode.workspace.workspaceFolders ?? [],
): Promise<string[]> {
  const run = (): Promise<string[]> => performAutomaticRegistration(bridge, output, state, folders);
  const operation = registrationQueue.then(run, run);
  registrationQueue = operation;
  return operation;
}

async function performAutomaticRegistration(
  bridge: NativeToolBridge,
  output: vscode.LogOutputChannel,
  state: vscode.Memento,
  folders: readonly vscode.WorkspaceFolder[],
): Promise<string[]> {
  if (!vscode.workspace.isTrusted) {
    return [];
  }
  if (folders.length === 0) {
    return [];
  }
  const source = new vscode.CancellationTokenSource();
  const registeredRoots: string[] = [];
  let automaticPlans = normalizeAutomaticPlanRegistry(
    state.get<AutomaticPlanRegistry>(AUTO_PLAN_STATE_KEY, {}),
  );
  try {
    let workspaces = await bridge.listWorkspaces(source.token);
    for (const folder of folders) {
      let folderRoot: string;
      try {
        folderRoot = await realpath(folder.uri.fsPath);
      } catch (error: unknown) {
        const message = error instanceof Error ? error.message : String(error);
        output.warn(`AUTO_REGISTER_SKIPPED root=${singleLine(folder.uri.fsPath)} ${singleLine(message)}`);
        continue;
      }
      let registeredRoot: string | undefined;
      try {
        registeredRoot = resolveRegisteredRoot(workspaces, folderRoot, []);
      } catch (error: unknown) {
        if (!(error instanceof RootResolutionError)) {
          throw error;
        }
      }
      if (registeredRoot !== undefined) {
        const existing = workspaces.find(
          (workspace) => normalizeLocalPath(workspace.root) === normalizeLocalPath(folderRoot),
        );
        const remembered = automaticPlans[normalizeLocalPath(folderRoot)];
        if (existing && canUpdateAutomaticRegistration(existing, remembered)) {
          try {
            const inferred = mergeAutomaticPlan({
              root: folderRoot, scopes: inferScopes(), globs: allSourceGlobs(),
            }, existing);
            if (registrationNeedsUpdate(inferred, existing)) {
              const executable = executablePath();
              const checked = await runCctxPlan(executable, inferred, true);
              await runCctxRegister(executable, inferred, true);
              output.info(
                `AUTO_UPDATED root=${inferred.root} scopes=${inferred.scopes.join(',')} globs=${inferred.globs.join(',')} ${compactPlanSummary(checked)}`,
              );
              bridge.invalidateRegistry();
              workspaces = await bridge.listWorkspaces(source.token);
            }
            automaticPlans = rememberAutomaticPlan(automaticPlans, inferred);
            await state.update(AUTO_PLAN_STATE_KEY, automaticPlans);
          } catch (error: unknown) {
            const message = error instanceof Error ? error.message : String(error);
            output.warn(
              `AUTO_UPDATE_SKIPPED root=${singleLine(folder.uri.fsPath)} ${singleLine(message)}`,
            );
          }
        } else if (remembered !== undefined) {
          delete automaticPlans[normalizeLocalPath(folderRoot)];
          await state.update(AUTO_PLAN_STATE_KEY, automaticPlans);
          output.appendLine(
            `AUTO_MANAGEMENT_RELEASED root=${folder.uri.fsPath} registration changed outside Code Context auto-registration`,
          );
        }
        continue;
      }
      try {
        const plan = await inferPlan(folder);
        const executable = executablePath();
        const checked = await runCctxPlan(executable, plan);
        await runCctxRegister(executable, plan);
        automaticPlans = rememberAutomaticPlan(automaticPlans, plan);
        await state.update(AUTO_PLAN_STATE_KEY, automaticPlans);
        registeredRoots.push(plan.root);
        output.appendLine(
          `AUTO_REGISTERED root=${plan.root} scopes=${plan.scopes.join(',')} globs=${plan.globs.join(',')} ${compactPlanSummary(checked)}`,
        );
        bridge.invalidateRegistry();
        workspaces = await bridge.listWorkspaces(source.token);
      } catch (error: unknown) {
        const message = error instanceof Error ? error.message : String(error);
        output.warn(`AUTO_REGISTER_SKIPPED root=${singleLine(folder.uri.fsPath)} ${singleLine(message)}`);
      }
    }
  } finally {
    source.dispose();
  }
  return registeredRoots;
}

export async function prepareMissingWorkspace(
  target: string,
  bridge: NativeToolBridge,
  output: vscode.LogOutputChannel,
  state: vscode.Memento,
): Promise<void> {
  if (!vscode.workspace.isTrusted || !vscode.workspace.getConfiguration('codeContextNative')
    .get<boolean>('autoRegisterWorkspace', true)) {
    throw new RootResolutionError('Automatic registration requires a trusted workspace and autoRegisterWorkspace enabled.');
  }
  const canonicalTarget = await realpath(target);
  const key = normalizeLocalPath(canonicalTarget);
  const existing = inFlightMissingWorkspaces.get(key);
  if (existing) {
    return existing;
  }
  const operation = (async (): Promise<void> => {
    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const root = await realpath(folder.uri.fsPath);
      const child = relative(root, canonicalTarget);
      if (child === '' || (child !== '..' && !child.startsWith(`..${sep}`) && !isAbsolute(child))) {
        await autoRegisterWorkspaceFolders(bridge, output, state, [folder]);
        return;
      }
    }
    await registerTrustedWorktree(canonicalTarget, bridge, output);
  })().finally(() => {
    inFlightMissingWorkspaces.delete(key);
  });
  inFlightMissingWorkspaces.set(key, operation);
  return operation;
}

async function registerTrustedWorktree(
  target: string,
  bridge: NativeToolBridge,
  output: vscode.LogOutputChannel,
): Promise<void> {
  if (!vscode.workspace.isTrusted || !vscode.workspace.getConfiguration('codeContextNative')
    .get<boolean>('autoRegisterWorkspace', true)) {
    throw new RootResolutionError('Automatic registration requires a trusted workspace and autoRegisterWorkspace enabled.');
  }
  const canonicalTarget = await realpath(target);
  const identity = await requestedGitIdentity(canonicalTarget);
  const canonicalRoot = await realpath(identity.root);
  if (normalizeLocalPath(canonicalRoot) !== normalizeLocalPath(identity.root)) {
    throw new RootResolutionError('Refusing a symlinked worktree root; open its canonical path explicitly.');
  }
  const targetCommon = await realpath(identity.commonDirectory);
  const failures: string[] = [];
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    try {
      const parent=await requestedGitIdentity(folder.uri.fsPath);
      if (await realpath(parent.commonDirectory)!==targetCommon) {
        failures.push('Requested root does not share this open repository Git directory.');
        continue;
      }
    } catch (error: unknown) {
      failures.push(error instanceof Error ? error.message : String(error));
      continue;
    }
    const { stdout } = await execFileAsync(executablePath(), [
      '--root', canonicalRoot, 'prepare',
    ], { timeout: 120_000, maxBuffer: 1024 * 1024, windowsHide: true });
    const prepared: unknown = JSON.parse(stdout);
    if (typeof prepared !== 'object' || prepared === null ||
        !('root' in prepared) || prepared.root !== canonicalRoot) {
      throw new RootResolutionError('Shared worktree preparation returned an unexpected root.');
    }
    bridge.invalidateRegistry();
    output.appendLine(`WORKTREE_REGISTRATION_CHECKED root=${canonicalRoot}`);
    return;
  }
  const message = `No open trusted repository authorizes worktree ${target}. ${failures.join('; ')}`;
  output.warn(`WORKTREE_REGISTRATION_REJECTED ${singleLine(message)}`);
  throw new RootResolutionError(message);
}

async function requestedGitIdentity(directory:string):Promise<{root:string;commonDirectory:string}> {
  const env={...process.env};
  for (const key of ['GIT_DIR','GIT_WORK_TREE','GIT_COMMON_DIR','GIT_INDEX_FILE','GIT_NAMESPACE']) {
    delete env[key];
  }
  const {stdout}=await execFileAsync('git',[
    '-c','core.quotePath=false','-C',directory,'rev-parse','--path-format=absolute',
    '--show-toplevel','--git-common-dir',
  ],{timeout:10_000,maxBuffer:1024*1024,env,windowsHide:true});
  return parseGitIdentity(stdout);
}

async function chooseWorkspaceFolder(): Promise<vscode.WorkspaceFolder | undefined> {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    throw new Error('Open a folder before registering Code Context.');
  }
  const active = vscode.window.activeTextEditor?.document.uri;
  const activeFolder = active ? vscode.workspace.getWorkspaceFolder(active) : undefined;
  if (activeFolder) {
    return activeFolder;
  }
  if (folders.length === 1) {
    return folders[0];
  }
  const selected = await vscode.window.showQuickPick(
    folders.map((folder) => ({
      label: folder.name,
      description: folder.uri.fsPath,
      folder,
    })),
    { title: 'Select a workspace folder to register with Code Context' },
  );
  return selected?.folder;
}

async function inferPlan(folder: vscode.WorkspaceFolder): Promise<RegistrationPlan> {
  const root = await realpath(folder.uri.fsPath);
  const source = (
    await Promise.all(
      sourceSearchPatterns().map((pattern) =>
        vscode.workspace.findFiles(
          new vscode.RelativePattern(folder, pattern),
          SEARCH_EXCLUDE,
          1,
        ),
      ),
    )
  ).flat();
  if (source.length === 0) {
    throw new Error(
      'No supported source files were detected. Add at least one source file or register with cctx in a terminal.',
    );
  }
  return {
    root,
    scopes: inferScopes(),
    globs: allSourceGlobs(),
  };
}

async function customizePlan(
  plan: RegistrationPlan,
): Promise<RegistrationPlan | undefined> {
  const scopesInput = await vscode.window.showInputBox({
    title: 'Code Context: Source directories',
    prompt: 'Comma-separated directories relative to the workspace root.',
    value: plan.scopes.join(', '),
    validateInput: validateList,
  });
  if (scopesInput === undefined) {
    return undefined;
  }
  const globsInput = await vscode.window.showInputBox({
    title: 'Code Context: Source file globs',
    prompt: 'Comma-separated positive globs, for example *.ts, *.tsx.',
    value: plan.globs.join(', '),
    validateInput: validateList,
  });
  if (globsInput === undefined) {
    return undefined;
  }
  return {
    root: plan.root,
    scopes: parseList(scopesInput),
    globs: parseList(globsInput),
  };
}

async function runCctx(
  executable: string,
  plan: RegistrationPlan,
  action: 'plan' | 'register',
  preserveRegistrationSettings = false,
): Promise<JsonValue> {
  const args = ['--root', plan.root];
  for (const scope of plan.scopes) {
    args.push('--scope', scope);
  }
  for (const glob of plan.globs) {
    args.push('--glob', glob);
  }
  if (preserveRegistrationSettings) {
    args.push('--preserve-registration-settings', '--allow-root-scope');
  }
  args.push(action);
  const { stdout } = await execFileAsync(executable, args, {
    timeout: 120_000,
    maxBuffer: 2 * 1024 * 1024,
    windowsHide: true,
  });
  return JSON.parse(stdout) as JsonValue;
}

async function runCctxPlan(
  executable: string,
  plan: RegistrationPlan,
  preserveRegistrationSettings = false,
): Promise<ValidatedPlanResult> {
  return validatePlanResult(
    plan,
    await runCctx(executable, plan, 'plan', preserveRegistrationSettings),
  );
}

async function runCctxRegister(
  executable: string,
  plan: RegistrationPlan,
  preserveRegistrationSettings = false,
): Promise<ValidatedPlanResult> {
  return validateRegisterResult(
    plan,
    await runCctx(executable, plan, 'register', preserveRegistrationSettings),
  );
}

function executablePath(): string {
  const configured = vscode.workspace
    .getConfiguration('codeContextNative')
    .get<string>('executablePath', '');
  return normalizeExecutablePath(configured);
}

function parseList(value: string): string[] {
  return [...new Set(value.split(',').map((item) => item.trim()).filter(Boolean))];
}

function validateList(value: string): string | undefined {
  return parseList(value).length === 0 ? 'Enter at least one value.' : undefined;
}

function planSummary(plan: RegistrationPlan, result: ValidatedPlanResult): string {
  const sourceMiB = (result.metadata_source_bytes / 1024 / 1024).toFixed(2);
  return [
    `Root: ${plan.root}`,
    `Scopes: ${plan.scopes.join(', ')}`,
    `Globs: ${plan.globs.join(', ')}`,
    `Files: ${result.files}`,
    `Source: ${sourceMiB} MiB`,
    `Watch directories: ${result.watch_directories}`,
    `Profiles: ${result.detected_profiles.join(', ') || 'none'}`,
  ].join('\n');
}

function compactPlanSummary(result: ValidatedPlanResult): string {
  return `files=${result.files} source_bytes=${result.metadata_source_bytes} watches=${result.watch_directories} profiles=${result.detected_profiles.join(',') || 'none'}`;
}
