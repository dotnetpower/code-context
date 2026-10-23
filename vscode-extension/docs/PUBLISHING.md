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

Packaging readiness is not proof that the extension is listed on Marketplace.
Before publishing:

1. Confirm ownership and publishing access for publisher **`moonchoi`** at
   <https://marketplace.visualstudio.com/manage>.
2. Keep the existing ID `moonchoi.code-context-native` for in-place upgrades.
   Changing the publisher is a new extension identity, not a harmless rename.
3. Verify the version, README links, icon, changelog, package contents, and the
   seven stable tool references.
4. Test a fresh native invocation on supported hosts. Do not infer SDK exposure
   merely from successful extension activation.
5. Authenticate using the official publisher flow. Keep tokens out of source,
   issues, logs, and chat messages.

```sh
npx vsce login moonchoi
npx vsce publish --packagePath dist/code-context-native.vsix
```

The actual listing, publisher permissions, and Marketplace review are external
steps. No Marketplace token is embedded in the extension or GitHub workflow.

See the official
[publishing guide](https://code.visualstudio.com/api/working-with-extensions/publishing-extension)
for current publisher authentication requirements.
