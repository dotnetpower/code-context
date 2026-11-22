#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 0755 target/release/cctx "$HOME/.local/bin/cctx.next"
mv "$HOME/.local/bin/cctx.next" "$HOME/.local/bin/cctx"
ln -sfn cctx "$HOME/.local/bin/pctx"
if [[ -n "${WSL_DISTRO_NAME:-}" ]]; then
    for win_bin_dir in /mnt/c/Users/*/.local/bin; do
        if [[ -f "$win_bin_dir/cctx.exe" ]]; then
            win_built=""
            if command -v x86_64-pc-windows-gnu-gcc >/dev/null 2>&1 \
                && cargo build --locked --release --target x86_64-pc-windows-gnu; then
                win_built="target/x86_64-pc-windows-gnu/release/cctx.exe"
            elif command -v clang >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
                rust_sysroot="$(rustc --print sysroot)"
                rust_lld_dir="$rust_sysroot/lib/rustlib/x86_64-unknown-linux-gnu/bin"
                win_extra_flags=""
                cached_mingw="$HOME/.cache/code-context-win-sysroot/root"
                if [[ -d "$cached_mingw/usr/x86_64-w64-mingw32/lib" ]]; then
                    win_extra_flags="-L native=$cached_mingw/usr/x86_64-w64-mingw32/lib"
                    gcc_eh_file="$(find "$cached_mingw/usr/lib/gcc/x86_64-w64-mingw32" -name "libgcc_eh.a" 2>/dev/null | head -n 1 || true)"
                    if [[ -n "$gcc_eh_file" ]]; then
                        win_extra_flags="$win_extra_flags -L native=$(dirname "$gcc_eh_file")"
                    fi
                fi
                if [[ -x "$rust_lld_dir/gcc-ld/ld.lld" ]] \
                    && PATH="$rust_lld_dir/gcc-ld:$rust_lld_dir:$PATH" \
                       CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER=clang \
                       CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_RUSTFLAGS="$win_extra_flags -C link-arg=--target=x86_64-pc-windows-gnu -C link-arg=-fuse-ld=lld" \
                       cargo build --locked --release --target x86_64-pc-windows-gnullvm; then
                    win_built="target/x86_64-pc-windows-gnullvm/release/cctx.exe"
                fi
            fi
            if [[ -n "$win_built" && -f "$win_built" ]]; then
                cp "$win_built" "$win_bin_dir/cctx.exe.next"
                mv -f "$win_bin_dir/cctx.exe.next" "$win_bin_dir/cctx.exe"
                cp "$win_built" "$win_bin_dir/pctx.exe.next"
                mv -f "$win_bin_dir/pctx.exe.next" "$win_bin_dir/pctx.exe"
            fi
            break
        fi
    done
fi
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
        "$HOME/.local/bin/cctx" --root "$(pwd)" prune-worktrees --apply >/dev/null 2>&1 || true
        python3 scripts/configure_subagents.py || true
        exit 0
    fi
    sleep 0.01
done
printf 'code-context daemon did not become ready\n' >&2
exit 1
