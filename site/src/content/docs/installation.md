---
title: "Installation & Platform Setup"
description: "Install Code Context and its native VS Code extension or MCP server on Linux, Windows, WSL, and SSH. Includes platform setup and troubleshooting."
category: setup
verified: 2026-11-14
sidebar:
  order: 1
---

> For a machine-readable index of all documentation, see [`llms.txt`](../llms.txt) and [`llms-full.txt`](../llms-full.txt).

**Code Context (`cctx`)** consists of two lightweight local components:

1. **The `cctx` Rust backend** — a single native executable (`cctx` on Linux/WSL/SSH or `cctx.exe` on Windows) that maintains the shared in-memory trigram index, incremental file watchers, adaptive memory controller, declarative probes (`pctx`), and compaction journal.
2. **Your agent client surface** — either the **[Code Context VS Code Marketplace extension](https://marketplace.visualstudio.com/items?itemName=dotnetpower.code-context-native)** (`dotnetpower.code-context-native`, VS Code 1.140+) for foreground native tools, **`cctx shared-mcp`** for Model Context Protocol (MCP) clients, or both simultaneously.

:::note[Separate backend binary]
The VS Code extension package contains the native Language Model Tools adapter and BPE-aligned agent instructions—not a precompiled backend binary. Install the `cctx` executable first so the extension can connect immediately.
:::

## Platform requirements matrix

| Platform | Transport & Daemon Lifecycle | Requirements |
|---|---|---|
| **Linux (local or SSH)** | Unix domain socket (`~/.local/state/code-context/daemon.sock`) + `systemd --user` service | Stable Rust/Cargo, `rg` (ripgrep) on `PATH`, `systemd` user session, cgroup v2 |
| **WSL2 (Linux workspace)** | Unix domain socket inside WSL distribution | Stable Rust/Cargo and `rg` inside WSL |
| **Native Windows (no WSL)** | Authenticated loopback RPC (`%LOCALAPPDATA%\code-context`), auto-started in background | Stable Rust/Cargo (MSVC) to build `cctx.exe`; uses `rg` on `PATH` or falls back to VS Code's bundled `rg.exe` |
| **Windows Agent Sessions** | Windows UI extension host bridging to WSL `cctx` | Extension installed on Windows + `codeContextNative.wslDistribution` & `wslExecutablePath` configured |

---

## 1. Install the Rust backend (`cctx`)

### Linux, WSL, and Linux SSH

Clone the repository and run the managed installer, which builds a `--locked --release` binary, installs `cctx` (and `pctx` alias) into `~/.local/bin`, and configures the user service:

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
export PATH="$HOME/.local/bin:$PATH"
```

### Native Windows (`cctx.exe`, without WSL)

Build `--locked --release` in PowerShell and copy `cctx.exe` into `%USERPROFILE%\.local\bin\cctx.exe`:

```powershell
git clone https://github.com/dotnetpower/code-context.git
Set-Location code-context
cargo build --locked --release
New-Item -ItemType Directory -Force "$HOME\.local\bin" | Out-Null
Copy-Item target\release\cctx.exe "$HOME\.local\bin\cctx.exe" -Force
& "$HOME\.local\bin\cctx.exe" --version
```

When you open a trusted Windows folder (`C:\...`) in VS Code without `codeContextNative.wslDistribution` set, the extension automatically launches `cctx.exe daemon` in the background on first tool call and stores index metadata under `%LOCALAPPDATA%\code-context`.

---

## 2. Install the VS Code Native Extension

Install **[Code Context by dotnetpower](https://marketplace.visualstudio.com/items?itemName=dotnetpower.code-context-native)** from the VS Code Extensions view, or run from your terminal:

```sh
code --install-extension dotnetpower.code-context-native
```

Alternatively, download `code-context-native.vsix` from [GitHub Releases](https://github.com/dotnetpower/code-context/releases/latest) and install via **Extensions: Install from VSIX...**. Marketplace and GitHub VSIX builds share the exact same `dotnetpower.code-context-native` identity and require **no proposed API flags**.

Open a trusted project and run **Code Context: Getting Started** from the Command Palette (`Ctrl+Shift+P` / `Cmd+Shift+P`).

---

## 3. Configure MCP Clients (Claude Code, Cursor, CLI & Subagents)

Any MCP-compatible coding agent can connect to the exact same shared daemon through `cctx shared-mcp`:

```json
{
  "mcpServers": {
    "code-context": {
      "command": "cctx",
      "args": ["shared-mcp"]
    }
  }
}
```

If `~/.local/bin` is not inherited by GUI launchers, specify the full executable path (for example `/opt/cctx/bin/cctx` or `$HOME/.local/bin/cctx` on Linux, or `%USERPROFILE%\\.local\\bin\\cctx.exe` on Windows).

---

## 4. Windows Agent Sessions (Windows UI + WSL Workspace)

A WSL-only extension installation cannot expose client tools inside a separate Windows Agents window. Install `dotnetpower.code-context-native` on the Windows host as well, then configure these workspace settings for the Agents window:

```json
{
  "remote.extensionKind": {
    "dotnetpower.code-context-native": ["ui"]
  },
  "codeContextNative.wslDistribution": "<your-wsl-distro-name>",
  "codeContextNative.wslExecutablePath": "/path/to/user-state/.local/bin/cctx"
}
```

Discover your distribution name with `wsl --list --quiet` and your Linux binary path inside WSL with `realpath ~/.local/bin/cctx`. Leave `codeContextNative.socketPath` empty in this mode.

---

## 5. Verify the installation & troubleshoot

Run the built-in health and telemetry checks:

```sh
cctx ping
cctx --root /path/to/project doctor
cctx usage --hours 24
```

### Common setup issues

| Symptom | Cause | Fix |
|---|---|---|
| `CACHE_MISS` on first lookup | Expected cold-root contract: root was queued for background indexing | Do **not** treat as zero matches or run `code_sync`; continue that single lookup with normal host tools and resume `cctx` on the next turn (~1s). |
| Legacy publisher conflict | Older `moonchoi.code-context-native` VSIX installed alongside `dotnetpower` | Uninstall the legacy extension first, then reload VS Code. |
| Connection error in Linux workspace | User daemon stopped or `~/.local/bin` missing from `PATH` | Run `~/.local/bin/cctx ping` or verify `codeContextNative.autoStartService` is `true` (default). |
| `TARGET_NOT_INDEXED` | Requested path is outside registered scopes/globs or excluded by `.gitignore` | Register explicit coverage with `cctx --root /path/to/project register` or use host tools for intentionally excluded files (`.github`, `.vscode`, `.env`). |

Next, walk through a real agent session in the [Quickstart & Agent Workflow](../quickstart/).
