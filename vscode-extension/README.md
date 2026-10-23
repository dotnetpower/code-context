<p align="center">
  <img src="resources/banner.png" alt="Code Context - local context, less friction" width="100%">
</p>

# Code Context

**The right code, ready for your AI agent.**

Find saved files, search source, and return nearby context in one call. Code
Context brings seven native tools to VS Code, powered by a shared local Rust
engine. No duplicate index per chat. No automatic scan of every worktree.

[Get the VSIX](https://github.com/dotnetpower/code-context/releases/latest) ·
[Setup guide](docs/SETUP.md) ·
[Source](https://github.com/dotnetpower/code-context) ·
[Report an issue](https://github.com/dotnetpower/code-context/issues)

> **Preview · Local backend required.** Install the `cctx` engine in Linux or
> WSL before using source tools. The extension does not silently download
> executables. Marketplace publication is separate from the downloadable VSIX;
> see the [publishing guide](docs/PUBLISHING.md).

## Why Code Context?

| Find less. Understand more. | Stay in control. |
|---|---|
| **One context call** returns matches and surrounding source. | **One shared cache** serves native tools and MCP clients. |
| **Filename discovery** is separate from content search. | **Adaptive memory** follows host and cgroup headroom. |
| **Batch reads** include whole-file hashes for verification. | **Your boundaries** remain: trust, scopes, ignores, and exclusions. |
| **Watcher updates** reconcile saved changes incrementally. | **Cold means optional**: a miss never waits for a full index build. |

This is saved-source search, not semantic retrieval or a call graph. Patterns
are literal by default; set `regex: true` for expressions such as `foo|bar`.

## Install in three steps

### 1. Install the engine

Build and install the Linux backend from the
[repository](https://github.com/dotnetpower/code-context#install):

```sh
git clone https://github.com/dotnetpower/code-context.git
cd code-context
bash scripts/install.sh
```

The backend needs a current Rust toolchain, ripgrep, and a Linux user service.
See [requirements and connection choices](docs/SETUP.md).

### 2. Install the extension

Download `code-context-native.vsix` from
[GitHub Releases](https://github.com/dotnetpower/code-context/releases/latest).
In VS Code, open **Extensions → … → Install from VSIX…**, then select the file.

Or use the CLI:

```sh
code --install-extension ./code-context-native.vsix
```

**No proposed API flags or edits to VS Code runtime arguments are required.**
The extension ID stays `moonchoi.code-context-native`, so existing installations
upgrade in place.

### 3. Connect and check

Run **Code Context: Getting Started** from the Command Palette. The walkthrough
links to connection settings, the setup guide, and a native self-test.

Open a trusted project and enable the Code Context tools in your agent's tool
settings. First-use registration is bounded and on demand; use
**Code Context: Register Current Workspace** if you prefer explicit setup.

Try this in your coding chat:

> Use `#cctxNativeFiles` to find `*Service.ts`, then `#cctxNativeContext` to inspect
> the relevant symbol and its nearby source.

Availability depends on the chat client. Headless or delegated sessions may use
the coexisting MCP surface instead; neither transport bypasses access policy.

## Your tools

| Tool reference | Best for |
|---|---|
| `#cctxNativeFiles` | Finding filenames and relative paths with a glob |
| `#cctxNativeGrep` | Exact or regex source search |
| `#cctxNativeContext` | Matches plus nearby source in one call |
| `#cctxNativeRead` | Known source ranges and live-verified file hashes |
| `#cctxNativeStatus` | Observing cache state without loading source |
| `#cctxNativeSync` | An explicitly needed full-source freshness barrier |
| `#cctxNativeWorkspaces` | Listing registered roots and coverage |

Tool names and references are stable across the packaging update. VS Code
groups the stable tool contributions under the extension; a proposed tool-set
API is no longer needed.

## Windows Agent Sessions with WSL

Install the extension in the **Windows/local host** as well as WSL. In your
Agent Sessions workspace, choose the UI host and your WSL connection:

```json
{
  "remote.extensionKind": {
    "moonchoi.code-context-native": ["ui"]
  },
  "codeContextNative.wslDistribution": "Ubuntu-22.04",
  "codeContextNative.wslExecutablePath": "/home/you/.local/bin/cctx"
}
```

Use your real distribution and Linux home directory. Keep this override scoped
to Agent Sessions; normal WSL or SSH project windows should use the Linux
workspace host. Pass the actual absolute Linux `root` when several roots are
registered. The Windows path is native framed IPC through WSL, **not MCP or a
new TCP server**.

Local macOS and Windows without a Linux/WSL backend are not supported.

## Read the status at a glance

The status bar updates when calls complete, not by polling source:

```text
CCTX N S:12 C:1 P:0 E:0
```

`N` is this native extension host. `S` is success, `C` cache not ready, `P` policy
denied, and `E` invalid-input/system errors. The tooltip shows the last call and
separates error categories. Counts include metadata calls, not all chats.
Click for daemon-wide outcomes and adaptive memory information.

## Cold cache? Keep going.

`CACHE_MISS` means no source result is available yet. Only the requested root
is queued for bounded background loading. Continue that lookup with permitted
normal tools; do not poll, switch CCTX transports, or run sync just to wait.

Unused roots can leave memory and load lazily on later demand. A status check
does not load source or prolong the cache lease. `CACHE_LOAD_FAILED` includes
the actual reason; exclusions and missing files remain explicit failures.

## Privacy and trust

The indexing and search pipeline stays local. There is no Code Context cloud
index or remote search service. Source snippets returned to a chat may still
be sent to that chat provider under its own policies.

Untrusted workspaces cannot activate or invoke the tools. Hidden paths,
symlinks, configured exclusions, and ignored files are not silently included.
Memory diagnostics and bounded request timings remain local.

## Help and development

- [Setup, settings, and troubleshooting](docs/SETUP.md)
- [Release and Marketplace publishing](docs/PUBLISHING.md)
- [Changelog](CHANGELOG.md)
- [Issues](https://github.com/dotnetpower/code-context/issues)

Licensed under [MIT](LICENSE). Artwork is original to this project.
