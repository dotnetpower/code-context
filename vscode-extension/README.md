<p align="center">
  <img src="resources/banner.png" alt="Code Context — local context, less friction" width="100%">
</p>

# Code Context by dotnetpower

**Local source context for your coding agent.** Find files, search source, and
read nearby code with seven native tools backed by one shared Rust cache.
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
retrieval mix averaged **6.83× faster** with a warm CCTX cache
(95% CI **6.65–7.06×**): **3.809 → 0.557 ms per operation**.

| Task | CCTX off | CCTX on, warm | Off / on |
|---|---:|---:|---:|
| Filename discovery | 4.257 ms | 0.566 ms | 7.52× |
| Literal search | 4.965 ms | 0.428 ms | 11.60× |
| Search + context | 5.122 ms | 0.499 ms | 10.26× |
| Known read + hash | **0.070 ms** | 0.799 ms | **0.09× — slower** |
| Fixed mixed suite | 3.809 ms | 0.557 ms | 6.83× |

![Warm retrieval latency and speedup, including slower known reads](resources/benchmark.png)

Eight daemon sessions and 8,960 correctness-checked pairs; OS cache warm in both
arms. Off bypasses CCTX with ripgrep/direct reads; on uses daemon RPC.
**Known small reads were slower through CCTX.** Cold misses fall back instead
of waiting. This is not a measurement of VS Code dispatch, LLM latency, or
whole-agent speed.

[Full method, cold-start chart, raw data, and reproduction](https://github.com/dotnetpower/code-context/blob/main/docs/BENCHMARKS.md)

## Install

> **Preview · Local backend required.** Linux, WSL, or Linux SSH with stable
> Rust, ripgrep, and a systemd user service. VS Code 1.140+ is required.

**1. Install the backend** inside the Linux host:

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
```

**2. Download the [VSIX](https://github.com/dotnetpower/code-context/releases/latest/download/code-context-native.vsix).**
In VS Code choose **Extensions → … → Install from VSIX…**.

**3. Run Code Context: Getting Started** in a trusted workspace.
Enable the tools in your agent's tool picker and try:

> Use `#cctxNativeFiles` to find `*Service*`, then `#cctxNativeContext` for nearby source.

**No proposed API flags are needed.** Marketplace publication is separate
from the downloadable VSIX. Windows Agents connection settings, manual
registration, and troubleshooting are in the **[setup guide](docs/SETUP.md)**.

The cache is optional: `CACHE_MISS` is not an empty search. Continue with permitted
host tools. Source remains local to the backend until selected results are
returned to the calling agent; trust, scopes, ignores, and exclusions still apply.
