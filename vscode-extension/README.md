<p align="center">
  <img src="resources/banner.png" alt="Code Context — local context, less friction" width="100%">
</p>

# Code Context by dotnetpower

**Native VS Code source context, zero-schema data/log probes (`pctx`), and compaction continuity (`cctx journal`) on Linux, native Windows (`cctx.exe`), WSL, and SSH.**
Find files, search source, and read block-clamped nearby code with seven native VS Code Language Model Tools (`#cctxNativeContext`, `#cctxNativeGrep`, `#cctxNativeRead`, `#cctxNativeFiles`) backed by one shared Rust cache, `<8 KiB` inline guards, rich document anchors (`#toc`, `#slide-N-M`), and live LLM cost telemetry. Publisher: **dotnetpower** ([migration steps](docs/SETUP.md#extension-details-resources-and-vsix-updates)).

> 💰 **Net LLM cost reduction — not raw byte vanity:** Controlled Copilot A/B trials cut **net session billing by 25.4%** (`4.76B → 3.55B nanoAIU`) and **wall time by 24.7%** (`100%` accuracy). Across **159 live sessions / 7,578 turns (`24h`)**, `cctx` modeled **`$48.88` (`-26.6%`) lower spend** (`~$183.78 → $134.90` @ `$0.001/AIC`) by avoiding **`~1.58B` tokens** — **98.5%** from **avoided turns (`851.8M`)** + **avoided cache re-reads (`701.8M`, `98.7×`)**, **not** raw byte trimming (`7.1M`, `0.5%`).

## Resources

[Repository](https://github.com/dotnetpower/code-context) · [Setup](docs/SETUP.md) · [Tool reference](https://github.com/dotnetpower/code-context/blob/main/docs/REFERENCE.md#native-tools) · [Benchmark report](https://github.com/dotnetpower/code-context/blob/main/docs/BENCHMARKS.md) · [Issues](https://github.com/dotnetpower/code-context/issues) · [License](LICENSE)

VS Code's native right-hand Resources panel uses Marketplace metadata. For VSIX-only installations, use the links above instead.

## Performance, net LLM cost analysis & `cctx usage`

```sh
$ cctx usage --hours 24       # or: cctx usage --hours 24 --details | --json
⚡ CCTX USAGE · Last 24 hours
💰 $48.88 saved · 26.6% less (estimated: Without cctx ~$183.78 → With cctx $134.90 @ $0.001/AIC; not a bill)
EFFICIENCY: Tokens avoided ~1.58B (851.8M turns · 701.8M cache 98.7x · 16.1M BPE) · Warm hits 95.5% · Adoption 90.3%
```

| Cost driver | Built-in (`rg` + `view`) | Post-hoc hook / MCP sandbox plugins | **Code Context (`cctx` + `pctx` + `journal`)** |
|---|---|---|---|
| **Standing prompt / schema tax** | `0 tok/turn` | **`+2,000 to +6,300 tok/turn`** (9–11 MCP schemas) | **`-1,292 to -2,123 tok/turn`** (BPE formula; **`0`** extra MCP schemas) |
| **Execution flow & VS Code support** | 2–4 turns (`rg` → `view`) | `PreToolUse` deny → LLM writes JS/shell → re-run (`PostToolUse` replace is no-op in VS Code) | **1 turn** (`#cctxNativeContext` block-clamped; **`0` `PreToolUse` denials**) |
| **Net coding session bill** | Baseline | **`+2.2% to +34.6% MORE expensive`** (`+15.5%` Try-It) | **`-25.4% AIU` (A/B) · `-$48.88 / -26.6%` (`24h`, `159` sess)** |

On [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo), the fixed retrieval mix averaged **17.22× faster** with a warm CCTX cache (95% CI **17.04–17.40×**): **3.996 → 0.232 ms per operation**, with a **19.735 ms** cold sync median (**5.01× faster** than v0.14).

| Task | CCTX off | CCTX on, warm | Off / on |
|---|---:|---:|---:|
| Filename discovery | 4.466 ms | 0.254 ms | 17.57× |
| Literal search | 5.234 ms | 0.230 ms | 22.77× |
| Search + context | 5.343 ms | 0.250 ms | 21.39× |
| Known read + hash | **0.081 ms** | 0.190 ms | **0.43× — slower** |
| Fixed mixed suite | 3.996 ms | 0.232 ms | 17.22× |

| Token, probe & agent suite | Baseline / raw input | With `cctx` / `pctx` / `journal` | Savings / speedup |
|---|---:|---:|---:|
| Rust / Python 6-symbol lookup | 15,028–25,133 / 13,997–24,609 tok | **4,031 tok (`10.3×`) / 2,141 tok (`13.16×`)** | **-73.2% to -91.3% tokens** |
| Rich documents (`.pptx`/`.docx`/`.md`) | Full-document read | **Virtual `#toc` + `#slide-N-M` (5.4× DEFLATE)** | **-86.3% to -98.9% tokens** |
| Copilot A/B turns, AIU & wall time | 7.00 calls / 5.00 turns / 20.31 s | **2.67 calls / 3.67 turns / 15.30 s (`100%` acc)** | **-50.0% tok · -25.4% AIU · -24.7% time** |
| Standing instructions (`cctx prompt`) | 9,127 B / 2,048 tok prose | **BPE formula spec (`756 tok`, EN/KO `--compress`)** | **-63.1% instruction tokens** |
| `pctx json` / `batch`+`log @s1` / `journal` | 1.02 MB JSON · 227 KB log · 434 KB sess | **662 B (`7.8ms`) · 2.3–4.1 KB (`@s1`) · 1,829 B (`<=2KiB`)** | **-98.2% to -99.94% (`0` schema)** |

![Warm retrieval latency, speedup, token savings, and net LLM session cost reduction](resources/benchmark.png)

Eight daemon sessions and 8,960 correctness-checked pairs; OS cache warm in both arms. **Known small reads were slower through CCTX.** Cold misses fall back instead of waiting. [Full cost anatomy, method, cold-start chart, and reproduction](https://github.com/dotnetpower/code-context/blob/main/docs/BENCHMARKS.md)

## Install

> **Preview · Local backend required.** Supports **native Windows (`cctx.exe`, no WSL needed)**, Linux, WSL, or Linux SSH with stable Rust and ripgrep. VS Code 1.140+ is required; agent instructions (`applyTo: "**"`) attach automatically.

**1. Install the backend** (Linux/WSL/SSH or native Windows):

```sh
git clone https://github.com/dotnetpower/code-context.git && cd code-context && bash scripts/install.sh && ~/.local/bin/cctx ping
# Native Windows (PowerShell): cargo build --locked --release; copy target\release\cctx.exe $HOME\.local\bin\cctx.exe
```

**2. Download the [VSIX](https://github.com/dotnetpower/code-context/releases/latest/download/code-context-native.vsix)** (**Extensions → … → Install from VSIX…**) or install from Marketplace.

**3. Run Code Context: Getting Started** in a trusted workspace (hover `CCTX N` in the status bar or run `cctx usage --hours 24`; `cctx.exe daemon` auto-starts on Windows):

> Use `#cctxNativeFiles` to find `*Service*`, then `#cctxNativeContext` for nearby source.

**No proposed API flags are needed.** Marketplace publication is separate from the downloadable VSIX. Windows, WSL, and troubleshooting details are in the **[setup guide](docs/SETUP.md)**. The cache is optional: `CACHE_MISS` is not an empty search; continue with permitted host tools.
