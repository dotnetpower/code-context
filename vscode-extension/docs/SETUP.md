# Setup and troubleshooting

## Requirements

- VS Code 1.140 or newer, with a chat agent that supports language model tools.
- A Linux `cctx` backend: local Linux, a WSL distribution, or a Linux SSH host.
- A current Rust toolchain to build the backend, `rg` on `PATH`, and cgroup v2
  for the managed service.
- A trusted workspace and registered source coverage.

The extension package does not contain a backend binary. Installing a VSIX
alone does not install Rust, ripgrep, or a Linux service.

## Linux, WSL, and SSH workspaces

Inside the Linux host:

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
```

Open the project through that host, install the VSIX there, and run
**Code Context: Getting Started**.

For explicit backend registration:

```sh
cctx --root /path/to/project --scope src --glob '*.rs' register
```

Choose your actual scopes and source globs. Automatic first-use registration
is enabled by default in trusted Linux workspace windows, but it never runs a
source scan merely because a window reloads. Set
`codeContextNative.autoRegisterWorkspace` to `false` for manual-only setup.

## Windows Agent Sessions

A WSL-only extension installation cannot register client tools in a separate
Windows Agents window. Install the VSIX in the Windows/local extension host
too. Use these settings only in the Agents workspace. Replace every angle-bracket
placeholder: use **Copy Extension ID** from the extension's gear menu, discover
the distribution with `wsl --list --quiet`, and get the executable inside that
distribution with `command -v cctx` (or `realpath ~/.local/bin/cctx`):

```json
{
  "remote.extensionKind": {
    "<extension-id>": ["ui"]
  },
  "codeContextNative.wslDistribution": "<distribution-name>",
  "codeContextNative.wslExecutablePath": "<absolute-linux-path-to-cctx>"
}
```

Keep `codeContextNative.socketPath` empty in this mode. Pass the actual absolute
Linux project/worktree root to tools. Several registered roots cannot be
implicitly mapped to a Windows Agents chat.

Authorized Git worktrees can be prepared lazily by the daemon. Register manual
roots inside WSL; the Windows registration command will not reinterpret a
Windows path as Linux source. A programmatic native self-test may pass an
explicit Linux root to `codeContextNative.selfTest`.

For all seven native references and CLI/MCP alternatives, see the
[tool reference](../../docs/REFERENCE.md#native-tools).

## Commands

| Command | Purpose |
|---|---|
| Code Context: Getting Started | Open the guided walkthrough |
| Code Context: Configure Connection | Open this extension's settings |
| Code Context: Open Setup Guide | Open this guide on GitHub |
| Code Context: Register Current Workspace | Explicitly plan/register trusted Linux coverage |
| Code Context: Run Native Tool Self-Test | Check native invocation and observational status; choose a registered root in Windows Agents |
| Code Context: Show Native Tool Status | Inspect status for the most recently requested root |

No proposed API approval is needed in version 0.12 or newer. Legacy
`enable-proposed-api` settings from older releases are not automatically edited
or removed, because other installed extensions may still rely on them.

## Connection settings

| Setting | Default | Use |
|---|---|---|
| `codeContextNative.socketPath` | empty | Linux: `CCTX_HOME` or `~/.local/state/code-context/daemon.sock` |
| `codeContextNative.executablePath` | empty | Linux manual registration: `~/.local/bin/cctx` |
| `codeContextNative.wslDistribution` | empty | Explicit WSL distribution for a Windows host |
| `codeContextNative.wslExecutablePath` | empty | Absolute Linux `cctx` path for a Windows host |
| `codeContextNative.autoStartService` | `true` | Start the existing user service when connection fails |
| `codeContextNative.autoRegisterWorkspace` | `true` | Bounded, first-use registration in trusted Linux workspaces |
| `codeContextNative.requestTimeoutMs` | `40000` | Native IPC timeout, not a source-cache warmup target |

Adaptive memory is a backend feature. It preserves system reserve, considers
parent cgroup headroom, and can use fresh Windows physical-memory reports from
native clients. On WSL without a fresh host report, it conservatively starts
with a 4GiB ceiling. Manual limits are documented in the
[backend reference](../../docs/REFERENCE.md#memory-and-io).

## When something looks wrong

| Symptom | Meaning and next step |
|---|---|
| `CACHE_MISS` | The optional root cache is not ready. Continue with permitted host tools; no retry loop or warmup sync. |
| `CACHE_LOAD_FAILED` | A real load/preparation failed. Read the included cause and cooldown. |
| `TARGET_NOT_INDEXED` | Read `reason=`: a pending saved file differs from a scope/glob/ignore/exclusion refusal. |
| `READ_POLICY_DENIED` | Do not try the same target through another CCTX tool or widen access automatically. |
| `READ_UNAVAILABLE` | The file may be absent or unstable. Discover the current filename before another read. |
| `READ_RANGE_OUT_OF_BOUNDS` | Use the reported line count or locate the current lines. |
| No native tools in a chat | Check the extension host and agent tool settings. Registration is not proof of per-chat exposure. |
| No WSL log updates | Windows Agents calls are logged in the Windows output channel. |

Open **Output → Code Context Native** for bounded native trace details.
`cctx --root /project doctor` compares installed/client/daemon builds without
stopping other sessions.

After an update, reload only the affected window at a safe boundary. Installing
a new executable or VSIX does not replace code cached in a running host.

## Extension details, Resources, and VSIX updates

The installed extension's README is versioned with the VSIX. Editing GitHub's
README does not update an already installed package, and VSIX-only distribution
does not automatically poll GitHub Releases for new versions.

1. Check the installed version in the extension's details view.
2. Download the latest VSIX from
   [Releases](https://github.com/dotnetpower/code-context/releases/latest).
3. Install it in the actual host: the Linux/WSL workspace host, and also the
   Windows/local host when using Windows Agents. Check other profiles separately.
4. Close and reopen the extension's details tab to discard its cached README.
   Only a running tool host needs a safe-boundary window reload for code changes.

In the supported editor version, the native right-hand **Resources** panel
reads repository/support/license links from Marketplace gallery metadata, not
directly from an installed VSIX's manifest. Correct `repository`, `bugs`,
`homepage`, and license assets prepare a package for publication but cannot
force that sidebar to appear before Marketplace publication. Do not patch VS
Code or forge gallery installation metadata to simulate a published listing.

The README therefore provides **Resources** links followed by the complete
benchmark table and chart before the installation section. The numeric results
remain readable even if remote images are blocked. The chart itself is also
included in the VSIX.
