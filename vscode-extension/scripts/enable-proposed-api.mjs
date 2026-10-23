import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { dirname } from 'node:path';

import { applyEdits, modify, parse } from 'jsonc-parser';

const path = process.argv[2];
if (!path) {
  throw new Error('usage: node enable-proposed-api.mjs PATH_TO_ARGV_JSON');
}

let source = '{}\n';
try {
  source = await readFile(path, 'utf8');
} catch (error) {
  if (error?.code !== 'ENOENT') {
    throw error;
  }
}

const errors = [];
const document = parse(source, errors, {
  allowTrailingComma: true,
  disallowComments: false,
});
if (errors.length > 0 || typeof document !== 'object' || document === null) {
  throw new Error(`Cannot safely update invalid VS Code argv.json: ${path}`);
}
const current = Array.isArray(document['enable-proposed-api'])
  ? document['enable-proposed-api'].filter((value) => typeof value === 'string')
  : [];
const manifest = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
const extensionId = `${manifest.publisher}.${manifest.name}`;
if (!current.includes(extensionId)) {
  current.push(extensionId);
}
const edits = modify(source, ['enable-proposed-api'], current, {
  formattingOptions: {
    insertSpaces: false,
    tabSize: 4,
    eol: '\n',
  },
});
const updated = applyEdits(source, edits);
await mkdir(dirname(path), { recursive: true });
const temporary = `${path}.cctx-next`;
await writeFile(temporary, updated, { encoding: 'utf8', mode: 0o600 });
await rename(temporary, path);
console.log(`Enabled proposed API for ${extensionId} in ${path}`);
