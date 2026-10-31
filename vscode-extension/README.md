<p align="center">
  <img src="resources/banner.png" alt="Code Context — local context, less friction" width="100%">
</p>

# Code Context by dotnetpower

**Native VS Code source context for your coding agent on Linux, native Windows (`cctx.exe`), WSL, and SSH.**
Find files, search source, and read block-clamped nearby code with seven native VS Code Language Model Tools (`#cctxNativeContext`, `#cctxNativeGrep`, `#cctxNativeRead`, `#cctxNativeFiles`) backed by one shared Rust cache.
Publisher: **dotnetpower**. Previous VSIX users: [migration steps](docs/SETUP.md#extension-details-resources-and-vsix-updates).

## Resources

[Repository](https://github.com/dotnetpower/code-context) ·
[Setup](docs/SETUP.md) ·
[Tool reference](https://github.com/dotnetpower/code-context/blob/main/docs/REFERENCE.md#native-tools) ·
[Benchmark report](https://github.com/dotnetpower/code-context/blob/main/docs/BENCHMARKS.md) ·
[Issues](https://github.com/dotnetpower/code-context/issues) · [License](LICENSE)

VS Code's native right-hand Resources panel uses Marketplace metadata.
For VSIX-only installations, use the links above instead.

## Performance

On [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo), the fixed
retrieval mix averaged **15.01× faster** with a warm CCTX cache
(95% CI **14.69–15.30×**, up from 6.83× in v0.14): **3.899 → 0.260 ms per operation**.
Replacing separate `rg` + `view` calls with one block-clamped `code_context` call cuts **lookup payload tokens by 73.2–91.3%** and reduces end-to-end coding agent wall time by **24.7%**.

| Task | CCTX off | CCTX on, warm | Off / on |
|---|---:|---:|---:|
| Filename discovery | 4.325 ms | 0.264 ms | 16.40× |
| Literal search | 5.104 ms | 0.249 ms | 20.50× |
| Search + context | 5.244 ms | 0.283 ms | 18.54× |
| Known read + hash | **0.074 ms** | 0.239 ms | **0.31× — slower** |
| Fixed mixed suite | 3.899 ms | 0.260 ms | 15.01× |

| Token & agent acceleration suite | Baseline (`rg` + `view`) | With CCTX (`code_context`) | Savings / speedup |
|---|---:|---:|---:|
| Rust 6-symbol lookup tokens | 15,028 tok (100L) / 25,133 tok (200L) | **4,031 tok** (6 calls vs 20, **10.3×** faster) | **-73.2% to -84.0% tokens** |
| Python 6-symbol lookup tokens | 13,997 tok (100L) / 24,609 tok (200L) | **2,141 tok** (**13.16×** faster) | **-84.7% to -91.3% tokens** |
| Copilot A/B agent turns & cost | 7.00 calls / 5.00 turns / 20.31 s | **2.67 calls / 3.67 turns / 15.30 s** | **-50.0% tok · -25.4% AIU · -24.7% time** |

![Warm retrieval latency, speedup, Rust/Python token savings, and agent acceleration](resources/benchmark.png)

Eight daemon sessions and 8,960 correctness-checked pairs; OS cache warm in both arms.
**Known small reads were slower through CCTX.** Cold misses fall back instead of waiting.
[Full method, cold-start chart, raw data, and reproduction](https://github.com/dotnetpower/code-context/blob/main/docs/BENCHMARKS.md)

## Install

> **Preview · Local backend required.** Supports **native Windows (`cctx.exe`, no WSL needed)**, Linux, WSL, or Linux SSH with stable Rust and ripgrep. VS Code 1.140+ is required; agent instructions (`applyTo: "**"`) attach automatically.

**1. Install the backend** (Linux/WSL/SSH or native Windows):

```sh
git clone https://github.com/dotnetpower/code-context.git && cd code-context
bash scripts/install.sh && ~/.local/bin/cctx ping
# Native Windows (PowerShell): cargo build --locked --release; copy target\release\cctx.exe $HOME\.local\bin\cctx.exe
```

**2. Download the [VSIX](https://github.com/dotnetpower/code-context/releases/latest/download/code-context-native.vsix).**
In VS Code choose **Extensions → … → Install from VSIX…**.

**3. Run Code Context: Getting Started** in a trusted workspace.
On Windows local folders (`C:\...`), `cctx.exe daemon` starts automatically in the background:

> Use `#cctxNativeFiles` to find `*Service*`, then `#cctxNativeContext` for nearby source.

**No proposed API flags are needed.** Marketplace publication is separate
from the downloadable VSIX. Windows, WSL, and troubleshooting details are in the **[setup guide](docs/SETUP.md)**.
The cache is optional: `CACHE_MISS` is not an empty search; continue with permitted host tools.
