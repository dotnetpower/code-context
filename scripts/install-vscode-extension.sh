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

if command -v powershell.exe >/dev/null && command -v wslpath >/dev/null; then
    profile_windows="$(powershell.exe -NoProfile -Command '[Environment]::GetFolderPath("UserProfile")' | tr -d '\r')"
    argv_path="$(wslpath -u "$profile_windows")/.vscode/argv.json"
else
    argv_path="$HOME/.vscode/argv.json"
fi
node "$extension/scripts/enable-proposed-api.mjs" "$argv_path"

code --install-extension "$extension/dist/code-context-native.vsix" --force
code --list-extensions --show-versions | grep '^moonchoi\.code-context-native@'

printf 'Fully quit all VS Code windows, reopen VS Code, then run: Code Context: Run Native Tool Self-Test\n'
