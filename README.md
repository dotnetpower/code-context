<p align="center">
  <img src="vscode-extension/resources/banner.png" alt="Code Context — local context, less friction" width="100%">
</p>

# Code Context

Fast local file discovery, source search, smart block-clamped context, zero-schema data/log probes (`pctx`), and compaction continuity (`cctx journal`) for coding agents on **Linux, native Windows (`cctx.exe`), WSL, and SSH**.
Runs as **7 native VS Code Language Model Tools** (`#cctxNativeContext`, `#cctxNativeGrep`, `#cctxNativeRead`, `#cctxNativeFiles`) and an MCP server backed by one shared Rust cache, `<8 KiB` inline guards, and rich document anchors (`#toc`, `#slide-N-M`, `#heading`). A cold cache returns immediately so normal tools can continue.

> 💰 **Real-bill LLM cost reduction — not raw byte vanity:** Controlled Copilot A/B trials cut **net session billing by 25.4%** (`4.76B → 3.55B nanoAIU`) and **wall time by 24.7%** (`100%` accuracy). Across **159 live sessions / 7,578 turns (`24h`)**, `cctx` modeled **`$48.88` (`-26.6%`) lower spend** (`~$183.78 → $134.90` @ `$0.001/AIC`; `~$488.82` @ `$0.01` API list ref.) by avoiding **`~1.58B` tokens** — **54.0%** from **avoided LLM turns (`851.8M`)** and **44.5%** from **avoided prompt-cache re-reads (`701.8M`, `98.7×`)**, **not** single-output byte trimming (`7.1M`, `0.5%`).

## 💰 Net LLM cost analysis & live `cctx usage`

```sh
$ cctx usage --hours 24       # or: cctx usage --hours 24 --details | --json
⚡ CCTX USAGE · Last 24 hours
💰 $48.88 saved · 26.6% less (estimated: Without cctx ~$183.78 → With cctx $134.90 @ $0.001/AIC; not a bill)
EFFICIENCY: Tokens avoided ~1.58B (851.8M turns · 701.8M cache 98.7x · 16.1M BPE) · Warm hits 95.5% · Adoption 90.3%
```

| Cost driver ([full cost anatomy](docs/BENCHMARKS.md#net-llm-cost-analysis)) | Built-in (`rg` + `view`) | Post-hoc hook / MCP sandbox plugins | **Code Context (`cctx` + `pctx` + `journal`)** |
|---|---|---|---|
| **Standing prompt / schema tax** | `0 tok/turn` | **`+2,000 to +6,300 tok/turn`** (9–11 MCP schemas) | **`-1,292 to -2,123 tok/turn`** (BPE formula; **`0`** extra MCP schemas) |
| **Execution flow & VS Code support** | 2–4 turns (`rg` → slice `view`) | `PreToolUse` deny → LLM writes JS/shell → re-run (`PostToolUse` replace is no-op in VS Code) | **1 turn** (`#cctxNativeContext` block-clamped; **`0` `PreToolUse` denials**) |
| **Controlled A/B coding session bill** | Baseline (`100%` pass) | **`+2.2% to +34.6% MORE expensive`** (`+15.5%` on Try-It) | **`-25.4% net AIU cost`** (`-24.7%` wall time, **`100%` accuracy**) |
| **24h multi-session spend (`159` sess)** | `~$183.78` (`183.8K AIC` est.) | Extra turns + output tokens offset byte cuts | **`$134.90`** (**`-$48.88 USD` / `-26.6%` less** · `~$488.82` @ `$0.01` API list ref.) |

## Measured speed, token & zero-schema probe benchmarks

On [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo), the fixed mixed retrieval suite averaged **17.22× faster with a warm CCTX cache** (95% CI **17.04–17.40×**, up from 6.83× in v0.14): **3.996 → 0.232 ms per operation**, with a **19.735 ms** cold sync median (**5.01× faster** than v0.14).

![Warm retrieval latency, speedup, token savings, and net LLM session cost reduction](benchmarks/results/aks-store-demo/latency-speedup.png)

| Retrieval task (AKS Store Demo) | CCTX off | CCTX on, warm | Off / on |
|---|---:|---:|---:|
| Filename discovery | 4.466 ms | 0.254 ms | 17.57× |
| Literal search | 5.234 ms | 0.230 ms | 22.77× |
| Search + context | 5.343 ms | 0.250 ms | 21.39× |
| Known file read + hash | **0.081 ms** | 0.190 ms | **0.43× — slower** |
| Fixed mixed suite | 3.996 ms | 0.232 ms | 17.22× |

| Token, probe & agent benchmark | Baseline / raw input | With `cctx` / `pctx` / `journal` | Reduction / speedup |
|---|---:|---:|---:|
| Rust / Python 6-symbol lookup tokens | 15,028–25,133 / 13,997–24,609 tok | **4,031 tok (`10.3×`) / 2,141 tok (`13.16×`)** | **-73.2% to -91.3% tokens** |
| Rich documents (`.pptx`/`.docx`/`.md` `#toc` & `#slide-N`) | Full-document read | **Virtual `#toc` + `#slide-N-M` (5.4× DEFLATE)** | **-86.3% to -98.9% tokens** |
| Copilot A/B turns, AIU bill & wall time | 7.00 calls · 5.00 turns · 4.76B AIU · 20.31 s | **2.67 calls · 3.67 turns · 3.55B AIU · 15.30 s** | **-61.9% calls · -25.4% cost · -24.7% time** |
| Auto-loaded instructions (`cctx prompt`) | 9,127 B / 2,048 tok prose | **BPE formula spec (`756 tok`, EN/KO `--compress`)** | **-63.1% standing tokens/turn** |
| `pctx json` (5,000 API records, `--where` + `--count-by`) | 1,021,281 B (`0` schema, `0` JS turn) | **662 B (`7.82 ms` median, secret-redacted)** | **-99.94% bytes** |
| `pctx batch` + `pctx log @latest` (2,400-line log, RRF) | 226,918 B (`2` queries + anomaly digest) | **4,079 B (`21.8 ms`) / 2,273 B (`11.0 ms` `@s1`)** | **-98.2% to -99.0% bytes** |
| `cctx journal resume` (40-step compaction continuity) | 434,271 B transcript (`0` `PreToolUse` denies) | **1,829 B `<cctx_journal_state>` (`5.51 ms`, `<=2 KiB`)** | **-99.58% bytes** |

Eight daemon sessions, 40 rounds, 28 tasks, **8,960 correctness-checked pairs**. Off bypasses CCTX with ripgrep/direct read/hash; on uses daemon RPC. Small known reads are faster without IPC; a cold miss plus fallback had a **4.409 ms median**, not a warm-cache speedup. See [cost anatomy, cold-start chart, raw samples, and reproduction](docs/BENCHMARKS.md).

## Install

**Requirements:** Linux, **native Windows (`cctx.exe`, no WSL required)**, WSL, or Linux SSH; stable Rust; ripgrep (`rg`); VS Code 1.140+ (auto-attaches `applyTo: "**"` agent instructions).

**1. Install the backend** (Linux/WSL/SSH or native Windows PowerShell):

```sh
git clone https://github.com/dotnetpower/code-context.git && cd code-context && bash scripts/install.sh && ~/.local/bin/cctx ping
# Native Windows (PowerShell): cargo build --locked --release; copy target\release\cctx.exe $HOME\.local\bin\cctx.exe
```

**2. Install [Code Context by dotnetpower from Marketplace](https://marketplace.visualstudio.com/items?itemName=dotnetpower.code-context-native)** (`code --install-extension dotnetpower.code-context-native` or [VSIX](https://github.com/dotnetpower/code-context/releases/latest/download/code-context-native.vsix); [publisher migration guide](vscode-extension/docs/SETUP.md#extension-details-resources-and-vsix-updates)). The backend is installed separately; installing the extension alone is not enough.

**3. Open a trusted project** and run **Code Context: Getting Started** or `cctx usage --hours 24` (`cctx.exe daemon` auto-starts on Windows):

> Use `#cctxNativeFiles` to find `*Service*`, then `#cctxNativeContext` to inspect a symbol.

No proposed API flags are needed. Marketplace and GitHub VSIX use the same extension identity.

## Learn more

[Website & docs](https://dotnetpower.github.io/code-context/) · [Setup and troubleshooting](vscode-extension/docs/SETUP.md) · [CLI, MCP, `pctx` & `journal` reference](docs/REFERENCE.md) · [Benchmarks & cost analysis](docs/BENCHMARKS.md) · [Architecture](DESIGN.md) · [Packaging and publishing](vscode-extension/docs/PUBLISHING.md) · [Releases](https://github.com/dotnetpower/code-context/releases) · [Issues](https://github.com/dotnetpower/code-context/issues) · [MIT license](LICENSE)
