# Code Context Native Tools

## Windows Agent Sessions with WSL

Install this extension in the **Windows/local extension host** as well as WSL
when using the Windows Agent Sessions window. A WSL-only installation registers
tools in WSL workspace windows, not in an independent Windows agent window.
The manifest explicitly declares `capabilities.agentsWindow.supported` so the
Agents renderer does not disable the extension before activation. Configure
these Windows user settings:

```json
{
  "codeContextNative.wslDistribution": "Ubuntu-22.04",
  "codeContextNative.wslExecutablePath": "/home/you/.local/bin/cctx"
}
```

In an Agent Sessions workspace that otherwise assigns this extension to a
workspace host, set these values in that workspace instead and add the supported
host override there:

```json
{
  "remote.extensionKind": {
    "moonchoi.code-context-native": ["ui"]
  }
}
```

Keep this override scoped to the Agent Sessions workspace. Normal WSL/SSH
project windows should retain the preferred workspace host and direct Unix IPC.

Use your actual distribution and absolute Linux executable path. Keep
`socketPath` empty in this mode; the bridge uses the WSL user's daemon runtime.
The backend must support `cctx native-bridge` (extension 0.10.0 or newer).

This mode registers the same seven native VS Code tools. A single persistent
`wsl.exe --exec cctx native-bridge` process relays bounded native framed IPC to
the existing shared daemon. It is **not MCP**, adds no TCP listener, and does
not create another indexer. Linux workspace extension hosts keep direct Unix
IPC; the manifest prefers the workspace host and permits a UI-host fallback.
Workspace trust, backend registration, content exclusions and cold-cache
behavior are unchanged.
Tool visibility does not depend on the Workspace Trust UI context key, which
can differ from the authoritative API in Agents windows. The extension declares
untrusted workspaces unsupported, and every invocation still checks
`workspace.isTrusted`, including rootless metadata tools. An actually untrusted
workspace cannot activate or invoke these tools.

Enable **Code Context Native** in the chat's tool picker. For Windows Agent
Sessions, pass `root` as the actual absolute Linux project/worktree path;
there is no safe implicit choice when several roots are registered. The daemon
remains the authority for access and authorized lazy worktree preparation.
Register new manual roots inside WSL; the Windows registration command does not
scan Windows paths as if they were Linux source. Programmatic self-tests may
pass an explicit Linux root to `codeContextNative.selfTest`. Tools

Native VS Code Language Model Tool integration for the shared Rust
`code-context` daemon.

Version 0.9.0 makes CCTX an optional **nonblocking root cache**. Startup,
registry inspection and legacy prewarm preferences do not load/pin source.
Cold/loading/syncing Files/Grep/Context/Read requests return `CACHE_MISS`
immediately; only the requested root is queued for background loading. Continue
that lookup with a permitted host search/read, not readiness polling, repeated
CCTX calls, another CCTX transport, or sync. A miss is never empty/absence proof.
Later independent queries may hit the cache. Explicit sync remains a waiting
full-source barrier. Root idle expiry is15minutes, with earlier LRU/memory
eviction when needed. Granularity is a registered project/worktree root, not
separate subdirectory segments.
First-use trusted registration is asynchronous too, with at most8pending roots,
coalescing and visible failure cooldown. Existing catalogs/extension hosts need
a safe refresh for updated descriptions; server miss text also carries fallback
guidance for older clients.

Version0.9.1 removes repeated read-path matcher compilation and idle40Hz polling
in the backend, skips no-change metadata reconstruction, and stops abandoned
runtime endpoints. Source policy/ignore/profile checks and live hashes remain
enforced. The user service gains low scheduling weights and physical-I/O caps;
these backend/resource changes apply without restarting unrelated VS Code work.

The extension runs in the workspace extension host (including WSL/SSH), keeps a
persistent bounded Unix socket connection, and exposes seven tools. File discovery, search,
context, read, status, and workspace listing are read-only; sync mutates only
the local in-memory index:

- `#cctxNativeFiles` (filename/path glob, not content)
- `#cctxNativeGrep`
- `#cctxNativeContext`
- `#cctxNativeRead`
- `#cctxNativeStatus`
- `#cctxNativeSync`
- `#cctxNativeWorkspaces`

The explicit `Native` suffix prevents VS Code from resolving the reference to
the coexisting MCP tool with a similar generated name.

All seven tools are contributed in the `code-context-native` language model tool
set. VS Code Agent Host forwards client tools by enabled tool set, so this set
must remain enabled in the Agent tools picker.

The existing CLI and MCP server remain available. The native extension does not
bundle or duplicate the Rust daemon.

Version 0.8.1 negotiates request tracing with the shared daemon. Native results
keep their source content and add a `CCTX_TRACE` diagnostic part. Its ID also
appears in the output channel and daemon request/client timing histories, so
host completion events can be correlated with queue, IPC and execution times.
Client handler start/end Unix-millisecond timestamps also identify time spent
waiting before the native handler is entered; elapsed durations stay monotonic.
Workspace preparation and client queue time are measured separately. Timing
reports are bounded and do not make a source result wait behind later requests.
Unavailable tracing on an older daemon is explicit; the self-test requires the
new capability. Existing MCP processes must be refreshed at a safe boundary:
installing an executable or VSIX does not hot-patch their running code.

Version 0.8.2 accompanies minimum-LRU cold-build admission and idle-only prewarm
in the backend. Native invocation time starts when the extension handler is
entered; it cannot measure or eliminate a host's earlier external-tool dispatch
queue. In a background chat/window whose external tools are not dispatched,
use backend Code Context MCP/CLI where available rather than treating a delayed
native callback as a slow RAM index. Refresh existing hosts at a safe boundary;
do not stop unrelated running sessions.

Version 0.8.3 adds host-dispatch-aware routing to the contributed source
instructions and native tool descriptions. Native tools remain preferred when
callable. For known unattended/background sessions that cannot dispatch native
callbacks, use backend MCP; if both client surfaces are unavailable, the shared
CLI is the same daemon/index, not a policy bypass. Do not duplicate pending
requests or run a full sync to repair a host queue. This is routing guidance,
not an automatic timeout/failover guarantee. Existing chat catalogs and
extension hosts must refresh at a safe boundary to receive new descriptions.

Requirements:

- Linux workspace extension host
- installed `~/.local/bin/cctx`
- registered workspace
- `code-context.service` user service

Install or update:

```sh
bash scripts/install-vscode-extension.sh
```

Equivalent direct VSIX installation:

```sh
code --install-extension \
  /home/moonchoi/dev/code-context/vscode-extension/dist/code-context-native.vsix \
  --force
```

The installer enables the local `contribLanguageModelToolSets` proposal in
the desktop user's `~/.vscode/argv.json`. Fully quit all VS Code processes and reopen
VS Code after installation; **Reload Window is not sufficient** for runtime
arguments. The status bar changes to
`CCTX Available` only after all seven tools appear in
`vscode.lm.tools`, `codeWorkspaces` succeeds through
`vscode.lm.invokeTool`, the daemon responds, and the current workspace resolves
to a registered root.
This is extension-side callability, not proof that the current chat exposes or
selects the tools. The self-test reports chat picker enablement as unverified.
It is also not a promise that this root's source cache is ready; a callable
source tool may immediately return `CACHE_MISS`.

By default, a trusted unregistered workspace starts asynchronous preparation
after returning a first-source cache miss, never merely by reloading a window:

1. use root scope `.` and all supported source globs so future nested projects and languages remain visible;
2. run the bounded `cctx plan`;
3. register only if the plan passes every admission/exclusion limit;
4. expose native tools without a click.

Discovery uses one bounded root-file probe and one bounded nested-file probe;
it does not enumerate 1,000 files or scan every top-level directory. Existing
automatic plans are widened through the Rust CLI's settings-preserving update path, retaining
excludes, prewarm, and memory budgets. The extension remembers the exact last
automatic plan in a validated, 64-entry bounded fingerprint registry. If the
current registration differs, it treats the root as manually managed and does
not overwrite it.

Set `codeContextNative.autoRegisterWorkspace` to `false` to require the
interactive `CCTX Register` flow. In that mode, choose **Customize** only when
the inferred values need adjustment.

Use **Code Context: Run Native Tool Self-Test** from the Command Palette to
invoke `codeStatus` through `vscode.lm.invokeTool` and verify the complete
native registry path. This status call is observational and does not load or
reconcile an index; `index_state: unloaded` is healthy before the first lookup.

Version 0.4.3 activates eagerly, contributes an on-demand saved-source chat
instruction, verifies actual Native invocation at startup, validates complete
plan/register responses, rejects malformed daemon payloads rather than
emitting success-shaped placeholders, refreshes stale root registries, and
returns search/context/read results as compact model-readable text rather than
raw JSON. This reduces prompt overhead and avoids a follow-up `jq` command for
ordinary source lookup.

Version 0.4.4 removes manual retry/status instructions from read failures. The
daemon transparently recovers eligible snapshot misses and save races;
permanent policy exclusions still fail with a specific explanation.

Version 0.5.0 adds `#cctxNativeFiles` / MCP `code_files` for file discovery,
safe documentation/configuration extension coverage, context radius 0..100,
workspace-folder change handling, and on-demand registration of Git-verified
worktrees of open trusted repositories. Unrelated paths are not auto-registered.
Automatic registration is serialized to preserve plan provenance. Existing
manual scopes/globs are not widened. Hidden files stay excluded; default secret
and private-key patterns remain outside the index. Use host policy-aware tools
for permitted excluded files instead of repeatedly retrying the index.

Use the adoption audit in the root README to measure selection and nonempty
results separately. A green self-test or repeated catalog names is not an
adoption or hit-rate measurement.

Version 0.6.0 uses shared worktree preparation rather than a separate native-only
registration implementation. A local operator first runs
`cctx --root /parent authorize-worktrees` on the registered trusted Git parent.
MCP-only and CLI sessions can then prepare the same Git-listed worktrees, using
the parent's coverage/exclusion/memory policy. Opening a folder or supplying a
model-generated path does not authorize arbitrary sibling directories.
Use `cctx --root /project doctor` to detect stale running clients; the native
self-test now verifies daemon capability schema 2 before reporting callability.
Installed extension version, current daemon build, and chat tool exposure are
separate facts. No active process is killed or window reloaded automatically.

Existing manual registrations still need an explicit `cctx migrate` review.
The migration and eligibility audit commands in the root README distinguish
coverage gaps from avoidable built-in reads; they do not block legitimate
fallbacks, shell commands, hidden targets, or host content-policy decisions.

Version 0.7.0 displays shared search freshness notices, including empty results.
Checked stale files fail with `STALE_INDEX` and an explicit root/path plus live
disk/read guidance; cached context is rechecked against disk hashes. The notice
distinguishes metadata checks from context hash checks and never claims an
atomic/full-disk view. Refresh the affected extension host at a safe boundary
to receive the new native formatter and instructions.

Version 0.7.1 removes reload-time source enumeration and automatic registration,
defers activation to `onStartupFinished`, and coalesces overlapping readiness
events. Readiness only checks the registry with preparation disabled. First-use
preparation still requires workspace trust, the auto-registration setting and
the existing scope/ignore/admission checks. Registered automatic plans are
computed without scanning for source existence. Status and self-test no longer
trigger a large cold index build, source hashing or watcher reconciliation.
Native startup logs explicitly record `source_scan=none` and on-demand indexing.

Version 0.7.2 clarifies read failures with the actual root/path and requested
line bounds. Out-of-bounds reads receive one local refresh in case the saved
file just grew, then report the current indexed line count instead of repeating
an impossible range. Missing/excluded batch members remain explicit failures,
not partial successes; instructions route known hidden/excluded targets to
policy-aware host reads without changing access scope.

Version0.8.0 supports explicit large-read pages and labeled range splitting,
and requires daemon capability schema3. Standard development filenames such
as Makefile/Dockerfile are explicit positive globs; hidden-path exclusions stay
unchanged. The shared daemon isolates bounded cold initialization and root-local
sync, exposes bounded timing/eviction telemetry, and retains active indexes with
a15minute source-activity lease. The256root metadata registration allowance is
independent of adaptive source residency and memory limits. The backend uses
host/cgroup headroom, preserves system reserve, and supports a manual MiB ceiling.

Native status is updated by call-completion events, with no source polling:
`N` denotes this native extension host; `S` success, `C` cache not ready,
`P` policy denied, and `E` invalid-input/system errors. The tooltip distinguishes
those last two categories and shows the last operation/time/root. Counts include
metadata calls and are not a whole-session or daemon-wide success rate. Click
for daemon status and aggregate native/MCP outcome counts.

Direct VS Code chats can use the Native tools when the tool set is enabled.
Delegated/headless sessions and Copilot CLI may not receive client extension
tools; they use the coexisting MCP tools against the same daemon.
