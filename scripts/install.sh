#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 0755 target/release/cctx "$HOME/.local/bin/cctx.next"
mv "$HOME/.local/bin/cctx.next" "$HOME/.local/bin/cctx"
ln -sfn cctx "$HOME/.local/bin/pctx"
if [[ -n "${WSL_DISTRO_NAME:-}" && -x /mnt/c/WINDOWS/system32/wsl.exe ]]; then
    cat > "$HOME/.local/bin/wsl.exe.next" <<'SHIM'
#!/bin/sh
# Fast-path same-distro WSL re-entry when Linux processes (such as VS Code AgentHost
# copilot-runtime) receive Windows mcp.json "command": "wsl.exe" configurations.
try_fast_path() {
    [ -n "${WSL_DISTRO_NAME:-}" ] || return 1
    [ "$#" -gt 0 ] || return 1
    target_distro="$WSL_DISTRO_NAME"
    target_cd=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -d|--distribution)
                [ "$#" -ge 2 ] || return 1
                target_distro="$2"
                shift 2
                ;;
            --distribution=*)
                target_distro="${1#--distribution=}"
                shift
                ;;
            --cd)
                [ "$#" -ge 2 ] || return 1
                target_cd="$2"
                shift 2
                ;;
            --|--exec|-e)
                shift
                break
                ;;
            -*)
                return 1
                ;;
            *)
                break
                ;;
        esac
    done
    [ "$target_distro" = "$WSL_DISTRO_NAME" ] || return 1
    [ "$#" -ge 1 ] || return 1
    resolved_cmd=""
    case "$1" in
        *wsl.exe) return 1 ;;
        /*)
            [ -x "$1" ] || return 1
            resolved_cmd="$1"
            ;;
        */*)
            return 1
            ;;
        *)
            resolved_cmd="$(command -v "$1" 2>/dev/null || true)"
            case "$resolved_cmd" in
                /*) [ -x "$resolved_cmd" ] || return 1 ;;
                *) return 1 ;;
            esac
            case "$resolved_cmd" in
                *wsl.exe|/mnt/*) return 1 ;;
            esac
            ;;
    esac
    if [ -n "$target_cd" ]; then
        case "$target_cd" in
            /*) cd "$target_cd" 2>/dev/null || return 1 ;;
            *) return 1 ;;
        esac
    fi
    shift
    exec "$resolved_cmd" "$@"
}
try_fast_path "$@"
exec /mnt/c/WINDOWS/system32/wsl.exe "$@"
SHIM
    chmod 0755 "$HOME/.local/bin/wsl.exe.next"
    mv "$HOME/.local/bin/wsl.exe.next" "$HOME/.local/bin/wsl.exe"
fi
install -m 0644 deploy/code-context.service "$HOME/.config/systemd/user/code-context.service"
systemctl --user daemon-reload
systemctl --user enable code-context.service
systemctl --user restart code-context.service
for attempt in $(seq 1 500); do
    if "$HOME/.local/bin/cctx" ping; then
        python3 scripts/configure_subagents.py || true
        exit 0
    fi
    sleep 0.01
done
printf 'code-context daemon did not become ready\n' >&2
exit 1
