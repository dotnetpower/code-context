import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

interface ToolContribution {
  name: string;
  toolReferenceName?: string;
  modelDescription?: string;
  canBeReferencedInPrompt?: boolean;
  inputSchema?: unknown;
  when?: string;
}

void test('release branding uses verified PNGs and complete Marketplace metadata', async () => {
  const manifest = JSON.parse(await readFile(resolve(__dirname, '../../package.json'), 'utf8')) as {
    publisher: string; name: string; displayName: string; description: string; icon: string;
    repository: { url: string; directory: string };
    galleryBanner: { color: string; theme: string };
  };
  assert.equal(manifest.publisher, 'dotnetpower');
  assert.equal(manifest.name, 'code-context-native');
  assert.equal(manifest.displayName, 'Code Context by dotnetpower');
  assert.ok(manifest.description.length <= 140);
  assert.equal(manifest.repository.url, 'https://github.com/dotnetpower/code-context.git');
  assert.equal(manifest.repository.directory, 'vscode-extension');
  assert.equal(manifest.galleryBanner.theme, 'dark');
  assert.equal(manifest.icon, 'resources/icon.png');
  for (const [name, width, height] of [['icon', 256, 256], ['banner', 1440, 480]] as const) {
    const image = await readFile(resolve(__dirname, `../../resources/${name}.png`));
    assert.equal(image.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
    assert.equal(image.readUInt32BE(16), width);
    assert.equal(image.readUInt32BE(20), height);
  }
  const chart = await readFile(resolve(__dirname, '../../resources/benchmark.png'));
  assert.equal(chart.subarray(0, 8).toString('hex'), '89504e470d0a1a0a');
  assert.ok(chart.readUInt32BE(16) >= 2000);
  assert.ok(chart.readUInt32BE(20) >= 600);
});

void test('onboarding reports backend requirement and does not mark an unpassed self-test complete', async () => {
  const manifest = JSON.parse(await readFile(resolve(__dirname, '../../package.json'), 'utf8')) as {
    version: string;
    contributes: { walkthroughs: Array<{
      id: string; steps: Array<{
        id: string; completionEvents: string[]; description: string;
        media: { markdown: string };
      }>;
    }> };
  };
  const walkthrough = manifest.contributes.walkthroughs[0];
  assert.equal(walkthrough?.id, 'getting-started');
  assert.equal(walkthrough.steps.length, 4);
  const check = walkthrough.steps.find((step) => step.id === 'check');
  assert.deepEqual(check?.completionEvents, ['onContext:codeContextNative.selfTestPassed']);
  for (const step of walkthrough.steps) {
    assert.ok(step.media.markdown.startsWith('docs/walkthrough/'), step.id);
    const media = await readFile(resolve(__dirname, '../../', step.media.markdown), 'utf8');
    assert.ok(media.length > 200, step.id);
  }
  const readme = await readFile(resolve(__dirname, '../../README.md'), 'utf8');
  assert.match(readme, /Local backend required/);
  assert.match(readme, /Marketplace publication is separate/);
  assert.match(readme, /No proposed API flags/);
  const changelog = await readFile(resolve(__dirname, '../../CHANGELOG.md'), 'utf8');
  assert.ok(changelog.includes(`## ${manifest.version}`));
});

void test('extension details expose resources and numeric benchmarks before installation', async () => {
  const readme = await readFile(resolve(__dirname, '../../README.md'), 'utf8');
  const resources = readme.indexOf('## Resources');
  const performance = readme.indexOf('## Performance');
  const install = readme.indexOf('## Install');
  assert.ok(resources >= 0 && resources < performance && performance < install);
  for (const link of ['Repository', 'Setup', 'Benchmark report', 'Issues', 'License']) {
    assert.ok(readme.includes(`[${link}](`));
  }
  assert.match(readme, /native right-hand Resources panel uses Marketplace metadata/);
  for (const text of ['4.257 ms', '0.566 ms', '4.965 ms', '0.428 ms', '5.122 ms',
    '0.499 ms', '0.070 ms', '0.799 ms', '3.809', '0.557', '6.83', 'slower']) {
    assert.ok(readme.includes(text), text);
  }
  assert.match(readme, /!\[.*\]\(resources\/benchmark\.png\)/);
});

void test('manifest contributes exactly the seven registered native tools', async () => {
  const packagePath = resolve(__dirname, '../../package.json');
  const manifest = JSON.parse(await readFile(packagePath, 'utf8')) as {
    extensionKind?: string[];
    enabledApiProposals?: string[];
    activationEvents?: string[];
    capabilities?: {
      agentsWindow?: { supported?: boolean };
      untrustedWorkspaces?: { supported?: boolean };
    };
    contributes?: {
      languageModelTools?: ToolContribution[];
      languageModelToolSets?: Array<{
        name?: string;
        tools?: string[];
      }>;
      commands?: Array<{ command?: string }>;
      configuration?: {
        properties?: Record<string, unknown>;
      };
      chatInstructions?: Array<{ path?: string }>;
    };
  };
  const tools = manifest.contributes?.languageModelTools ?? [];
  const expectedReferences = [
    'cctxNativeContext',
    'cctxNativeFiles',
    'cctxNativeGrep',
    'cctxNativeRead',
    'cctxNativeStatus',
    'cctxNativeSync',
    'cctxNativeWorkspaces',
  ];
  const expected = [
    'code-context-native_codeContext',
    'code-context-native_codeFiles',
    'code-context-native_codeGrep',
    'code-context-native_codeRead',
    'code-context-native_codeStatus',
    'code-context-native_codeSync',
    'code-context-native_codeWorkspaces',
  ];
  assert.deepEqual(
    tools.map((tool) => tool.name).sort(),
    expected,
  );
  assert.deepEqual(manifest.contributes?.chatInstructions, [
    { path: './prompts/code-context.instructions.md' },
  ]);
  const contributedInstructions = await readFile(
    resolve(__dirname, '../../prompts/code-context.instructions.md'),
    'utf8',
  );
  assert.match(contributedInstructions, /description: "Use when:/);
  assert.match(contributedInstructions, /applyTo: "\*\*"/);
  assert.match(contributedInstructions, /#cctxNativeContext/);
  assert.match(contributedInstructions, /prefer backend MCP/);
  assert.match(contributedInstructions, /cctx --backend shared --root/);
  assert.match(contributedInstructions, /Do not duplicate a still-pending lookup/);
  assert.match(contributedInstructions, /Never use another transport to bypass/);
  for (const [reference, operation] of [
    ['cctxNativeFiles', 'code_files'],
    ['cctxNativeGrep', 'code_grep'],
    ['cctxNativeContext', 'code_context'],
    ['cctxNativeRead', 'code_read'],
  ]) {
    const tool = tools.find((entry) => entry.toolReferenceName === reference);
    assert.ok(tool?.modelDescription?.includes(`backend MCP ${operation}`), reference);
    assert.ok(tool?.modelDescription?.includes('CACHE_MISS returns immediately'), reference);
    if (operation === 'code_grep' || operation === 'code_context') {
      const schema = JSON.stringify(tool?.inputSchema);
      assert.ok(schema.includes('Set regex=true'), reference);
      assert.ok(schema.includes('not semantic or keyword search'), reference);
    }
  }
  assert.match(contributedInstructions,/CCTX is an optional accelerator/);
  assert.match(contributedInstructions,/Do not enumerate or prepare sibling worktrees/);
  assert.deepEqual(manifest.extensionKind, ['workspace', 'ui']);
  assert.equal(manifest.capabilities?.agentsWindow?.supported, true);
  assert.equal(manifest.capabilities?.untrustedWorkspaces?.supported, false);
  assert.deepEqual(manifest.activationEvents, ['onStartupFinished']);
  assert.equal(manifest.enabledApiProposals, undefined);
  assert.equal(manifest.contributes?.languageModelToolSets, undefined);
  assert.deepEqual(
    tools.map((tool) => tool.toolReferenceName).sort(),
    expectedReferences,
  );
  assert.deepEqual(
    (manifest.contributes?.commands ?? [])
      .map((command) => command.command)
      .sort(),
    [
      'codeContextNative.configureConnection',
      'codeContextNative.gettingStarted',
      'codeContextNative.openSetupGuide',
      'codeContextNative.registerWorkspace',
      'codeContextNative.selfTest',
      'codeContextNative.showStatus',
    ],
  );
  assert.ok(
    manifest.contributes?.configuration?.properties?.[
      'codeContextNative.executablePath'
    ],
  );
  assert.ok(
    manifest.contributes?.configuration?.properties?.[
      'codeContextNative.autoRegisterWorkspace'
    ],
  );
  for (const tool of tools) {
    assert.equal(tool.canBeReferencedInPrompt, true, tool.name);
    assert.ok(tool.toolReferenceName, tool.name);
    assert.ok(tool.modelDescription?.includes('native'), tool.name);
    assert.ok(tool.inputSchema, tool.name);
    assert.equal(tool.when, undefined, tool.name);
  }
  for (const name of [
    'code-context-native_codeGrep',
    'code-context-native_codeContext',
  ]) {
    const tool = tools.find((candidate) => candidate.name === name);
    const schema = tool?.inputSchema as
      | { properties?: Record<string, { minLength?: number }> }
      | undefined;
    assert.equal(schema?.properties?.pattern?.minLength, 1, name);
  }
  const contextTool = tools.find(
    (tool) => tool.name === 'code-context-native_codeContext',
  );
  assert.doesNotMatch(contextTool?.modelDescription ?? '', /concept/i);

  const implementation = await readFile(
    resolve(__dirname, '../../src/nativeTools.ts'),
    'utf8',
  );
  for (const name of expected) {
    assert.match(implementation, new RegExp(`name: '${name}'`));
  }
  assert.doesNotMatch(implementation, /Retry once|use #cctxNativeStatus/);
  const registration=await readFile(resolve(__dirname,'../../src/registration.ts'),'utf8');
  assert.doesNotMatch(registration,/'worktree',\s*'list'/);
  assert.match(registration,/'--show-toplevel','--git-common-dir'/);
});
