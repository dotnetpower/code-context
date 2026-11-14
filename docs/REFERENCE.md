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

- `~/.local/bin/cctx` (plus `~/.local/bin/pctx -> cctx` multi-call symlink for declarative probes)
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
cctx usage --hours 24
cctx --root /path/to/project prompt --style formula --target instructions
cctx --root /path/to/project prompt --compress "Which function calculates retry delay, and what caps the result?" --json
pctx http "https://openapi.example.org/{svc}?Type=json&pSize=2" --var svc=CityPark,ParkingPlace
pctx json /path/to/collection.json --bbox 125.5,33.1,131.0,38.6 --count-by properties.category
pctx table /path/to/dataset.parquet --bbox 125.5,33.1,131.0,38.6
pctx image /path/to/render.png --expect 1440x480
pctx batch -c "git log -n 200 --stat" -c "docker logs app" -q "timeout" -q "retry"
pctx log @s1 -q "fatal"
cctx --root /path/to/project journal record --kind decision --pin -s "Keep <8KiB inline cap"
cctx --root /path/to/project journal resume -q "inline cap"
```

`pctx` (`cctx probe {http|json|table|image|log|batch|journal}`) replaces ad-hoc `python3 -c`, `curl`, `jq`, and `sqlite3` inspection scripts with zero-schema-tax Rust primitives: concurrent `{var}` matrix HTTP probing with automatic query/env secret redaction, streaming JSON/JSONL/GeoJSON/STAC/`/tmp` offload schema & `--bbox` filtering, pure-Rust Parquet Thrift footer + read-only SQLite + CSV inspection, PNG scanline Rec.709 luminance/exposure + JPEG/WebP/GIF/SVG verification, spooled unstructured log & multi-command batch retrieval (`pctx batch` / `pctx log @sN` with fence/stacktrace-aware chunking and Okapi BM25 + Trigram RRF), and a per-workspace compaction continuity journal (`cctx journal` / `pctx journal`) that restores a secret-scrubbed `<=2 KiB` `<cctx_journal_state>` block + BM25 prompt recall across context compactions via fail-open, zero-`PreToolUse`-deny hooks (`cctx journal hook-config`), bounded strictly inside `<8 KiB`.

`cctx prompt` renders BPE-aligned symbolic formula (`formula`), contract-compatible
hybrid (`hybrid`), or prose (`prose`) instructions for `.instructions.md`, MCP
`initialize`, or subagent definitions, and `--compress` converts natural-language
code-lookup task prompts into symbolic formula prompts with token-savings telemetry.

`cctx usage` also reports per-tool failure causes (`client_disconnected`,
`cache_miss`, `range_out_of_bounds`, `response_budget_exceeded`, `file_unavailable`, ...),
P95/P99 latency, offloaded outputs, built-in fallbacks with the CCTX tool to use
instead, per-workspace adoption, shared-stall detection (slow calls from several
sessions finishing at the same instant), host-dispatch delay from the native
`CCTX_TRACE` timing footer, and optimization advisories (`long_session`,
`spend_concentration`, `model_without_cctx`, `unregistered_workspace`,
`low_context_share`, `sequential_slicing`, `offload_rate`,
`other_tool_offload`, `cache_miss_rate`, `client_disconnected`). Cost and token savings are trace-replayed per
`(session, agent)` stream using each turn's `copilotUsage.tokenDetails` rates,
pre-compaction history re-reads, and warm-turn context sizes; `--json` adds
`insights` and the savings-model `assumptions`. The terminal view leads with
estimated savings and a without/with comparison at `$0.001/AIC`; these are USD
equivalents, not verified billing reductions. Alternative `$0.01/AIC` valuations
are labeled as reference values, not additional savings.

The default view shows savings, efficiency, up to three prioritized advisory
groups, and a compact model cost table. Repeated recommendations are grouped;
additional findings are counted with a `--details` hint. The three highest-spend
models are shown individually, with remaining models aggregated as `Other`;
the total always includes every model.

The table wraps model names within the available width and switches to compact
stacked rows when even the minimal columns cannot fit. Output fits the detected
terminal width (`COLUMNS`, or 80 columns, when detection is unavailable).
Only the title, savings and advisory heading use an emoji. Savings are green,
headings cyan, and `OPTIMIZATION ADVISORIES` yellow. Colors are disabled for
piped/file output, `TERM=dumb`, or when `NO_COLOR` is set.

```sh
$ cctx usage --hours 2
⚡ CCTX USAGE · Last 2 hours

💰 $4.77 saved · 15.4% less (estimated)
   Without cctx ~$30.90 → With cctx $26.13
   USD equivalent @ $0.001/AIC; not a bill.

EFFICIENCY
  Tokens avoided ~173.21M
  Warm hits 93.6% · Source adoption 86.2%

💡 OPTIMIZATION ADVISORIES
  1. Reduce cold-cache costs; compact long sessions and avoid idle gaps.
  Full evidence: --details

MODELS · USD equivalent
┌────────────────┬────────┬───────────┐
│ Model          │   Used │ Est.saved │
├────────────────┼────────┼───────────┤
│ Model A (high) │ $25.00 │     $4.50 │
│ Model B (high) │  $1.13 │     $0.27 │
├────────────────┼────────┼───────────┤
│ Total          │ $26.13 │     $4.77 │
└────────────────┴────────┴───────────┘

Full report: cctx usage --hours 2 --details
```

`cctx usage --hours 2 --details` preserves the full token-savings breakdown,
LLM usage, reference valuation, model/surface/session cards, tool reliability,
built-in fallbacks, workspace adoption, every advisory and the savings assumptions.
Full details use wrapping cards with unabridged model, tool and workspace names.
`--json` always returns the complete structured report, with or without `--details`.

Native tool results carry `started_unix_ms` and `total_ms` in the `CCTX_TRACE`
footer. Calls slower than 5 s also log a warning and append a `CCTX_SLOW` line
that names the phase timings and tells the agent to switch to the MCP tools or
`cctx --backend shared` instead of polling.

MCP `code_files`, `code_grep`, `code_context` and `code_read` responses are kept
under a 7.4 KiB inline budget so hosts do not offload them to temporary files.
A trimmed response sets `truncated`, `omitted` and `inline_notice`; read tails
move into `next_ranges` with `complete=false`. A plain `code_read` (a bare array
of snippets) that must be trimmed is returned in the paged object shape
(`snippets`, `complete`, `next_ranges`); responses that fit are unchanged.
`CACHE_MISS` states the expected load time for that root, derived from its last
cold load or the recent median, so retries wait for the background load. Roots with sustained lookups keep
their cache longer than the 15-minute base lease (2x at 50 and 4x at 200 lookups).

Search patterns are case-sensitive literals unless `--regex` or
`--ignore-case` is explicit. Results use relative paths and 1-based lines.
Matching-line counts and truncation are explicit. A zero-match success differs
from a refusal, unavailable cache, or system failure.

For Copilot CLI, subagents without UI client dispatch, or disconnected-turn
fallback, register the existing backend as an MCP server (do not choose MCP in
VS Code when `#cctxNative*` is callable):

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
| `#cctxNativeRead` | Batched ranges with whole-file hashes (supports `path:line:col`, `#Lstart-Lend`, `path.md#heading-slug`, `#toc`, and `#slide-N-M`) |
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
| `CCTX_PROMPT_STYLE` | Optional MCP/subagent instruction style (`formula`, `hybrid`, `prose`) |

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
