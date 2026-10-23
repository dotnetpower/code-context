#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --release
mkdir -p "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 0755 target/release/cctx "$HOME/.local/bin/cctx.next"
mv "$HOME/.local/bin/cctx.next" "$HOME/.local/bin/cctx"
install -m 0644 deploy/code-context.service "$HOME/.config/systemd/user/code-context.service"
systemctl --user daemon-reload
systemctl --user enable code-context.service
systemctl --user restart code-context.service
for attempt in $(seq 1 50); do
    if "$HOME/.local/bin/cctx" ping; then
        exit 0
    fi
    sleep 0.1
done
printf 'code-context daemon did not become ready\n' >&2
exit 1
