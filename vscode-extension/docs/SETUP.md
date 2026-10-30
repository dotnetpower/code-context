# Setup and troubleshooting

## Requirements

- VS Code 1.140 or newer, with a chat agent that supports language model tools.
- A `cctx` backend: local Linux, native Windows (`cctx.exe`), a WSL distribution, or a Linux SSH host.
- A current Rust toolchain to build the backend, `rg` on `PATH` (or bundled with VS Code on Windows), and cgroup v2 for the managed Linux service.
- A trusted workspace and registered source coverage.

The extension package does not contain a backend binary. Installing a VSIX
alone does not install Rust, ripgrep, or a background service.

## Native Windows workspaces (without WSL)

Build and install `cctx.exe` into `%USERPROFILE%\.local\bin\cctx.exe`:

```powershell
cargo build --locked --release
New-Item -ItemType Directory -Force "$HOME\.local\bin" | Out-Null
Copy-Item target\release\cctx.exe "$HOME\.local\bin\cctx.exe" -Force
```

When opening a trusted local Windows folder (`C:\...`) or running on Windows
without `codeContextNative.wslDistribution` configured, the extension uses
`%USERPROFILE%\.local\bin\cctx.exe` (or `codeContextNative.executablePath`),
automatically starts `cctx.exe daemon` in the background on demand, and stores
state under `%LOCALAPPDATA%\code-context`.

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

### Reading the log

The Output channel uses ordinary timestamped text with real info/warning/error
levels, not JSON records. Each call has one summary with its outcome, latency
phases, root, and correlation ID:

```text
[info] code_context success total=4.200ms prepare=0.300ms queue=0.100ms ipc=3.500ms format=0.300ms daemon=2.000ms request=42 root=/project trace=example
[info] code_read cache_not_ready ... reason=CACHE_MISS ...
[warning] code_read policy_denied ... reason=READ_POLICY_DENIED ...
[error] code_context system_error ... reason=...
```

Status and self-test commands print labeled fields rather than raw JSON.
Long diagnostic trees explicitly report when the log view is truncated.
Control characters in request errors are escaped so they cannot forge log lines.
Machine-facing IPC, CLI JSON, tool trace footers, and asynchronous daemon trace
reports remain structured and compatible; only the human Output view changes.

Windows Agents with several registered roots are **callable**, not unregistered.
Pass an explicit absolute Linux root to source tools. The status command lets
you choose an existing root without loading source. Changes to unrelated
Windows session folders do not repeat the Linux readiness probe. Explicit Linux
roots go directly to the backend, which still enforces registration/access.

A cold miss is expected, not a system error. A policy warning does not authorize
widening scopes or bypassing exclusions. Large `prepare` time can reflect
reconnecting the WSL bridge after idle expiry; it is not index-search time.
There is no keepalive polling or automatic warmup to hide that cost.

After an update, reload only the affected window at a safe boundary. Installing
a new executable or VSIX does not replace code cached in a running host.

## Extension details, Resources, and VSIX updates

Version 0.13 uses the publisher **dotnetpower** and the extension ID
`dotnetpower.code-context-native`. This is a new identity, not an automatic
upgrade of a package from the previous publisher.

Uninstall the previous Code Context extension in each relevant profile/host
before installing the new one. Keep the existing `codeContextNative.*` settings
and backend registration; the service and all seven tool references are
unchanged. If `remote.extensionKind` contains a publisher-specific override,
replace its key with the new extension ID. If the previous host is still
active, reload only that window at a safe boundary. The new adapter refuses
to register duplicate tools while the old extension is still present.

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
