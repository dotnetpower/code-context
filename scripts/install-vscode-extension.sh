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

python3 - "$expected" <<'PY'
import glob, json, os, shutil, sys

expected = sys.argv[1]
ext_id, version = expected.split("@", 1)
target_rel = f"{ext_id}-{version}"
wsl_src = os.path.expanduser(f"~/.vscode-server/extensions/{target_rel}")
if os.path.isdir(wsl_src):
    for win_ext_dir in glob.glob("/mnt/c/Users/*/.vscode/extensions"):
        win_json = os.path.join(win_ext_dir, "extensions.json")
        if not os.path.isfile(win_json):
            continue
        dst_dir = os.path.join(win_ext_dir, target_rel)
        if os.path.abspath(wsl_src) != os.path.abspath(dst_dir):
            if os.path.exists(dst_dir):
                shutil.rmtree(dst_dir)
            shutil.copytree(wsl_src, dst_dir)
        for old_dir in glob.glob(os.path.join(win_ext_dir, f"{ext_id}-*")):
            if os.path.basename(old_dir) != target_rel and os.path.isdir(old_dir):
                old_out = os.path.join(old_dir, "out")
                new_out = os.path.join(wsl_src, "out")
                if os.path.isdir(new_out):
                    if os.path.exists(old_out):
                        shutil.rmtree(old_out)
                    shutil.copytree(new_out, old_out)
        try:
            data = json.load(open(win_json, encoding="utf-8"))
            drive_path = win_ext_dir.replace("/mnt/c/", "/c:/") + "/" + target_rel
            updated = False
            for item in data:
                if isinstance(item, dict) and item.get("identifier", {}).get("id") == ext_id:
                    item["version"] = version
                    item["relativeLocation"] = target_rel
                    if isinstance(item.get("location"), dict):
                        item["location"]["path"] = drive_path
                    updated = True
            if updated:
                with open(win_json, "w", encoding="utf-8") as f:
                    json.dump(data, f, separators=(",", ":"))
        except Exception:
            pass
PY

python3 "$root/scripts/configure_subagents.py" || true

printf 'At a safe boundary reload the affected window, then run: Code Context: Getting Started\n'
