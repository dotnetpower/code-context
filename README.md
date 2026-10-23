<p align="center">
  <img src="vscode-extension/resources/banner.png" alt="Code Context — local context, less friction" width="100%">
</p>

# Code Context

Fast local file discovery, source search, and context for coding agents.
One shared Rust cache serves VS Code native tools and MCP clients.
A cold cache returns immediately so normal tools can continue.

## Install

**Requirements:** Linux, WSL, or Linux SSH; stable Rust; ripgrep (`rg`);
a systemd user service. The VS Code extension requires VS Code 1.140+.

**1. Install the backend** in the Linux host:

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
~/.local/bin/cctx ping
```

**2. Install the extension.** Download
**[code-context-native.vsix](https://github.com/dotnetpower/code-context/releases/latest/download/code-context-native.vsix)**,
then choose **Extensions → … → Install from VSIX…** in VS Code.
The backend is installed separately; a VSIX alone is not enough.
Existing VSIX users should follow the [publisher migration guide](vscode-extension/docs/SETUP.md#extension-details-resources-and-vsix-updates).

**3. Open a trusted project** and run **Code Context: Getting Started**.
Enable Code Context tools in the agent's tool picker, then try:

> Use `#cctxNativeFiles` to find `*Service*`, then `#cctxNativeContext` to inspect a symbol.

No proposed API flags are needed. For Windows Agents, SSH, manual registration,
or MCP-only clients, see the [setup guide](vscode-extension/docs/SETUP.md) and
[backend reference](docs/REFERENCE.md). Marketplace publication is separate
from the downloadable VSIX.

## Measured performance

On [AKS Store Demo](https://github.com/dotnetpower/aks-store-demo), the fixed
mixed retrieval suite averaged **6.83× faster with a warm CCTX cache**
(95% CI **6.65–7.06×**): **3.809 → 0.557 ms per operation**.

![Retrieval latency and paired-session speedup with 95% confidence intervals](benchmarks/results/aks-store-demo/latency-speedup.png)

| Task | CCTX off | CCTX on, warm | Off / on |
|---|---:|---:|---:|
| Filename discovery | 4.257 ms | 0.566 ms | 7.52× |
| Literal search | 4.965 ms | 0.428 ms | 11.60× |
| Search + context | 5.122 ms | 0.499 ms | 10.26× |
| Known file read + hash | **0.070 ms** | 0.799 ms | **0.09× — slower** |
| Fixed mixed suite | 3.809 ms | 0.557 ms | 6.83× |

Eight daemon sessions, 40 rounds, 28 tasks, **8,960 correctness-checked pairs**.
Both arms use a warm OS cache. Off bypasses CCTX with ripgrep/direct read/hash;
on uses daemon RPC. Small known reads are faster without IPC. A cold miss plus fallback
had a **4.325 ms median**, not a warm-cache speedup.

These are **retrieval microbenchmarks**, not whole-agent or VS Code chat speedups.
See [method, cold-start chart, raw samples, and reproduction](docs/BENCHMARKS.md)
for uncertainty and limitations.

## Learn more

- [Setup and troubleshooting](vscode-extension/docs/SETUP.md)
- [CLI, MCP, cache behavior, memory, and privacy](docs/REFERENCE.md)
- [Architecture](DESIGN.md)
- [Packaging and publishing](vscode-extension/docs/PUBLISHING.md)
- [Releases](https://github.com/dotnetpower/code-context/releases) ·
  [Issues](https://github.com/dotnetpower/code-context/issues) · [MIT license](vscode-extension/LICENSE)
