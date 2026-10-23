#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
extension="$root/vscode-extension"

command -v node >/dev/null
command -v npm >/dev/null
command -v code >/dev/null

cd "$extension"
npm ci
npm run package

code --install-extension "$extension/dist/code-context-native.vsix" --force
code --list-extensions --show-versions | grep '^moonchoi\.code-context-native@'

printf 'At a safe boundary reload the affected window, then run: Code Context: Getting Started\n'
