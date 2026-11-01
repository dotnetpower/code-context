# Backend reference

For the short installation path, start with the [README](../README.md).
For host-specific connection settings, use the
[extension setup guide](../vscode-extension/docs/SETUP.md).

## Requirements and installation

The backend runs on Linux/WSL/SSH (Unix domain sockets) and native Windows
(`cctx.exe`, authenticated loopback RPC with hidden daemon auto-start).
The managed Linux installer needs stable Rust/Cargo, ripgrep on `PATH`,
systemd user services, and cgroup v2. On native Windows, `cctx.exe` uses `rg`
on `PATH` or falls back automatically to the `rg.exe` bundled with VS Code.
The VSIX contains the adapter, not a backend executable. Node.js 22+ and npm are
needed only to build the extension from source.

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
export PATH="$HOME/.local/bin:$PATH"
```

The installer builds a locked release and installs:

- `~/.local/bin/cctx`
- `~/.config/systemd/user/code-context.service`
- private state in `~/.local/state/code-context/`

It enables/restarts this user service; it does not configure an MCP client,
rewrite agent instructions, or download a VS Code extension.
`CCTX_HOME` overrides state for isolated tests: an absolute, user-owned
directory with mode `0700` is required.

## Register source coverage

The trusted Linux extension host can register on first use. For a CLI-only
project, specify the actual root, directory scopes, and positive source globs:

```sh
cctx --root /path/to/project --scope src --scope tests --glob '*.rs' plan
cctx --root /path/to/project --scope src --scope tests --glob '*.rs' register
cctx workspaces
```

`plan` reports the proposed source inventory without persisting registration.
Repeat `--scope`, `--glob`, and `--exclude` as needed. Registration is metadata;
it does not load all source. Hidden paths, ignored files, unsafe paths, symlinks,
and policy-excluded files are not eligible. Never register content excluded by
your host's organization policy: the standalone daemon cannot discover it.

Default dependency/build/cache exclusions and ecosystem profiles keep generated
content out. Use `--no-default-excludes` only for an intentional, authorized
coverage change, not to bypass a refusal.

## CLI and MCP

```sh
cctx --backend shared --root /path/to/project files '*.rs'
cctx --backend shared --root /path/to/project grep 'create_service'
cctx --backend shared --root /path/to/project context 'create_service' --radius 5
cctx --backend shared --root /path/to/project read src/lib.rs 1 40
cctx --backend shared --root /path/to/project status
```

Search patterns are case-sensitive literals unless `--regex` or
`--ignore-case` is explicit. Results use relative paths and 1-based lines.
Matching-line counts and truncation are explicit. A zero-match success differs
from a refusal, unavailable cache, or system failure.

For Copilot CLI, register the existing backend as a separate MCP server:

```sh
copilot mcp add code-context --timeout 30000 -- "$HOME/.local/bin/cctx" shared-mcp
```

Other MCP clients can launch `cctx shared-mcp` with a Linux working directory.
Use the host's supported configuration format and actual executable path.
Reload its tool catalog or start a new chat after configuration changes.
Tool discovery does not prove that a model selects the tools.

## Native tools

| Reference | Use |
|---|---|
| `#cctxNativeFiles` | Filename/path discovery with a glob |
| `#cctxNativeGrep` | Exact or regex source search |
| `#cctxNativeContext` | Search plus nearby source |
| `#cctxNativeRead` | Batched ranges with whole-file hashes (supports `path:line:col`, `#Lstart-Lend`, and `path.md#heading-slug`) |
| `#cctxNativeStatus` | Observational state, no source loading |
| `#cctxNativeSync` | Explicit full-source freshness barrier |
| `#cctxNativeWorkspaces` | Registered roots and coverage |

The extension uses stable APIs and contributes automatic agent usage
instructions (`applyTo: "**"`). Ordinary Linux/WSL/SSH workspace windows use
the workspace host; local Windows workspace windows (`C:\...`) use native
`cctx.exe native-bridge` and automatically start `cctx.exe daemon` in the
background. A Windows Agents host with `wslDistribution` configured uses framed
native IPC through WSL; it is not MCP, TCP, or a second index. See the
[connection guide](../vscode-extension/docs/SETUP.md#windows-agent-sessions).
The extension rejects untrusted activation and rechecks trust on every call.

## Optional cache and freshness

- Startup, registration, and status inspect metadata only.
- A cold/loading/syncing ordinary lookup returns `CACHE_MISS` immediately and
  schedules only the requested root for bounded background loading.
- Continue that lookup with permitted host tools. Do not poll, change transport,
  or call sync merely to wait for warmup. A later independent lookup may hit.
- Inactive source roots expire after 15 minutes; memory pressure also evicts
  least-recently-used roots. Metadata registrations remain.
- Saved-file watchers reconcile affected files/directories incrementally.
  A bounded periodic metadata scan recovers missed notifications.
- `code_sync` / `cctx sync` is an intentional, waiting full-source hash barrier.
  Use it for exhaustive absence claims or a specifically required strict check,
  not after every edit.
- Returned hashes identify read versions. Saved source is not a filesystem
  transaction and does not include unsaved editor buffers.

Read recovery refreshes requested saved ranges without widening coverage.
Scope/ignore/exclusion refusals remain refusals, not permission to try another
transport. Registration changes can require reauthorization; Git worktrees are
prepared only within operator-authorized parent coverage, not enumerated in bulk.

## Memory and I/O

The controller adapts to available memory, its own anonymous memory, and
ancestor cgroup headroom while retaining a system reserve. It uses fresh
Windows physical-memory reports when available to a WSL backend. Without one,
WSL starts conservatively rather than assuming all guest memory is free.
Budget growth does not immediately allocate that much RAM.

| Setting | Purpose |
|---|---|
| `CCTX_MEMORY_MAX_MIB` | Explicit upper ceiling; never a promise to allocate it |
| `CCTX_MEMORY_RESERVE_MIB` | Explicit reserve override |
| `CCTX_HOME` | Private registry/socket directory for isolation |

Configure service variables with `systemctl --user edit code-context.service`,
then restart only that service at a safe boundary. Check `cctx status` and
`cctx doctor` for applied limits; kernel cgroup values are authoritative.
The managed service also applies CPU/I/O weights and guarded device I/O limits.
Manual daemons do not establish machine-wide resource controls.

Metadata registration and source residency have different bounds: up to 256
registered roots; resident source roots scale with the budget, bounded at 24.
Background source preparation has a bounded queue and worker. Failed loading
reports its real cause and retry cooldown rather than an empty success.

## Privacy and diagnostics

The backend has no model, embedding service, or external source upload.
Selected results are returned to the calling client and may then enter its
agent conversation; follow that client's data policy. Local logs can contain
requested roots and tool diagnostics. Do not publish them without redaction.
The daemon is not a sandbox against other processes running as the same user.

```sh
cctx --root /path/to/project doctor
cctx --root /path/to/project coverage
systemctl --user status code-context.service
```

In VS Code, use **Output → Code Context Native** and the native self-test.
Installing a new build does not replace modules already loaded by an active
host. Reload only the affected window at a safe boundary.

## Development

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python -m unittest discover -s tests -p 'test_*.py'
cd vscode-extension
npm ci
npm run check
npm run package
```

See [architecture](../DESIGN.md), [benchmark methodology](BENCHMARKS.md), and
[packaging](../vscode-extension/docs/PUBLISHING.md). Historical one-shot
`live|ram|index` modes are experiments, not measurements of shared-daemon
startup amortization.
