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
version="$(node -p "require('./package.json').version")"
code --list-extensions --show-versions | grep -Fx "$expected"

# Clean stale WSL extension directories so only the active version remains on disk
python3 - "$version" <<'PY'
import glob, json, os, shutil, sys

version = sys.argv[1]
ext_id = "dotnetpower.code-context-native"
target_rel = f"{ext_id}-{version}"
wsl_ext_dir = os.path.expanduser("~/.vscode-server/extensions")
if os.path.isdir(wsl_ext_dir):
    for old_dir in glob.glob(os.path.join(wsl_ext_dir, f"{ext_id}-*")):
        if os.path.basename(old_dir) != target_rel and os.path.isdir(old_dir):
            shutil.rmtree(old_dir, ignore_errors=True)
    obs_path = os.path.join(wsl_ext_dir, ".obsolete")
    if os.path.isfile(obs_path):
        try:
            obs = json.load(open(obs_path, encoding="utf-8"))
            if isinstance(obs, dict):
                for k in [k for k in obs if k.startswith(f"{ext_id}-") or k.startswith("moonchoi.code-context-native-")]:
                    obs.pop(k, None)
                with open(obs_path, "w", encoding="utf-8") as f:
                    json.dump(obs, f, separators=(",", ":"))
        except Exception:
            pass
PY

if [[ -n "${WSL_DISTRO_NAME:-}" ]] && command -v powershell.exe >/dev/null && command -v wslpath >/dev/null; then
    windows_home="$(
        cd /mnt/c
        powershell.exe -NoProfile -Command '[Environment]::GetFolderPath("UserProfile")'
    )"
    windows_home="${windows_home//$'\r'/}"
    windows_home_wsl="$(wslpath -u "$windows_home")"
    windows_user_data="$windows_home_wsl/AppData/Roaming/Code/User"
    windows_ext_dir="$windows_home_wsl/.vscode/extensions"
    wsl_ext_src="$HOME/.vscode-server/extensions/dotnetpower.code-context-native-$version"
    cctx_bin="$(realpath "$HOME/.local/bin/cctx" 2>/dev/null || printf '%s' "$HOME/.local/bin/cctx")"

    windows_vsix_wsl="$windows_home_wsl/AppData/Local/Temp/code-context-native-$$.vsix"
    trap 'rm -f "${windows_vsix_wsl:-}"' EXIT
    install -m 0600 "$extension/dist/code-context-native.vsix" "$windows_vsix_wsl"
    windows_vsix="$(wslpath -w "$windows_vsix_wsl")"
    # shellcheck disable=SC2016
    (
        cd /mnt/c
        powershell.exe -NoProfile -Command \
            '& { param($Vsix) Set-Location C:\; code --uninstall-extension moonchoi.code-context-native 2>$null; code --install-extension $Vsix --force }' \
            "$windows_vsix"
    ) || true

    while IFS= read -r profile_name; do
        [[ -n "$profile_name" ]] || continue
        # shellcheck disable=SC2016
        (
            cd /mnt/c
            powershell.exe -NoProfile -Command \
                '& { param($Profile, $Vsix) Set-Location C:\; code --profile $Profile --uninstall-extension moonchoi.code-context-native 2>$null; code --profile $Profile --install-extension $Vsix --force }' \
                "$profile_name" "$windows_vsix"
        ) || true
    done < <(
        python3 "$root/scripts/configure_windows_profile.py" \
            --user-data "$windows_user_data" \
            --distribution "$WSL_DISTRO_NAME" \
            --print-profile-names 2>/dev/null || true
    )

    rm -f "$windows_vsix_wsl"
    windows_vsix_wsl=""

    if [[ -d "$windows_user_data" ]]; then
        python3 "$root/scripts/configure_windows_profile.py" \
            --user-data "$windows_user_data" \
            --extensions-dir "$windows_ext_dir" \
            --wsl-ext-src "$wsl_ext_src" \
            --version "$version" \
            --distribution "$WSL_DISTRO_NAME" \
            --executable "$cctx_bin"
    fi
fi

python3 "$root/scripts/configure_subagents.py" || true

printf 'Native tools installed as the default. At a safe boundary reload the affected window, then run: Code Context: Getting Started\n'
