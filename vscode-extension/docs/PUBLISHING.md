# Packaging and publishing

## Build an installable VSIX

From `vscode-extension/`, with Node.js 22 or newer:

```sh
npm ci
npm run check
npm run package
```

The output is `dist/code-context-native.vsix`. VSCE runs lint, tests, and
compilation. Package content is allowlisted: runtime JavaScript, native
instructions, documentation, license, manifest, and rendered PNG artwork.
It excludes source maps, tests, source files, dependencies, caches, and logs.

The extension uses stable language model tool contributions. Do not reintroduce
`enabledApiProposals` or the proposed `languageModelToolSets` contribution in
the distributable manifest. Stable tool IDs and `toolReferenceName` values are
covered by tests; VS Code creates the extension's automatic tool grouping.

## Original artwork

PNG assets are committed, so ordinary packaging needs no image tools. To change
the artwork, edit `resources/icon.svg` and `resources/banner.svg`, then render:

```sh
python -m pip install cairosvg
python scripts/render-assets.py
```

Use PNGs in the Marketplace manifest and README. The icon is 256x256, the banner
1440x480. The SVGs are editable source, not packaged runtime dependencies.

## GitHub distribution

The release workflow validates an explicit version tag, builds the VSIX, and
attaches the package and SHA-256 checksum to a GitHub release. It does not publish
to the Marketplace. Tag releases only after reviewing the tested commit.

Users can download the VSIX and install it from the Extensions view. They still
need the Linux/WSL backend described in the setup guide.

## Marketplace checklist

The first Marketplace identity is `dotnetpower.code-context-native`.
Its display name is **Code Context by dotnetpower**: the unqualified name
was rejected by Marketplace as already taken. Changing a display name does
not change the publisher/name identity or require reinstalling a new ID.
Local packaging cannot reserve or guarantee Marketplace name availability;
only the server's accepted publication verifies it.

Log in to a Microsoft account with publishing access to the **dotnetpower**
publisher; a GitHub username alone does not grant that access.

Packaging readiness is not proof that the extension is listed on Marketplace.
Before publishing:

1. Read the publisher from `package.json` and confirm its publishing access at
   <https://marketplace.visualstudio.com/manage>.
2. Keep the existing publisher/name pair in `package.json` for in-place upgrades.
   Changing the publisher is a new extension identity, not a harmless rename.
3. Verify the version, README links, icon, changelog, package contents, and the
   seven stable tool references.
4. Test a fresh native invocation on supported hosts. Do not infer SDK exposure
   merely from successful extension activation.
5. Authenticate using the official publisher flow. Keep tokens out of source,
   issues, logs, and chat messages.

Prefer an existing authorized Microsoft Entra identity without saving a PAT:

```sh
npx vsce verify-pat --azure-credential "$(node -p "require('./package.json').publisher")"
npx vsce publish --azure-credential --packagePath dist/code-context-native.vsix
```

For a publisher owned by a personal Microsoft account (MSA), acquire the Azure
DevOps token (`499b84ac-1321-427f-aa17-267ca6975798/.default`) from the
Microsoft Services tenant (`f8cdef31-a31e-4b4a-93e4-5f571e91255a`) in memory
rather than a subscription directory tenant, and pass it directly to `vsce`
without persisting a plaintext token.

If access is denied, use the management portal to create/select the publisher
and configure its members, or upload the verified VSIX there after signing in.
Do not bypass the denial or use another publisher identity without approval.
Do not use `vsce login` when its credential-store warning says that credentials
would fall back to plaintext storage.

The actual listing, publisher permissions, and Marketplace review are external
steps. No Marketplace token is embedded in the extension or GitHub workflow.

See the official
[publishing guide](https://code.visualstudio.com/api/working-with-extensions/publishing-extension)
for current publisher authentication requirements.
