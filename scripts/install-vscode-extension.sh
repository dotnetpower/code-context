#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
extension="$root/vscode-extension"

command -v node >/dev/null
command -v npm >/dev/null
command -v code >/dev/null

cd "$extension"
installed="$(code --list-extensions)"
if grep -Fqx 'moonchoi.code-context-native' <<< "$installed"; then
    printf 'Remove the previous publisher extension before migration: code --uninstall-extension moonchoi.code-context-native\n' >&2
    exit 1
fi
npm ci
npm run package

code --install-extension "$extension/dist/code-context-native.vsix" --force
expected="$(node -p "const p = require('./package.json'); p.publisher + '.' + p.name + '@' + p.version")"
code --list-extensions --show-versions | grep -Fx "$expected"

printf 'At a safe boundary reload the affected window, then run: Code Context: Getting Started\n'
