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
  assert.deepEqual(manifest.enabledApiProposals, [
    'contribLanguageModelToolSets',
  ]);
  const toolSet = manifest.contributes?.languageModelToolSets?.[0];
  assert.equal(toolSet?.name, 'code-context-native');
  assert.deepEqual([...(toolSet?.tools ?? [])].sort(), expectedReferences);
  assert.deepEqual(
    tools.map((tool) => tool.toolReferenceName).sort(),
    expectedReferences,
  );
  assert.deepEqual(
    (manifest.contributes?.commands ?? [])
      .map((command) => command.command)
      .sort(),
    [
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
