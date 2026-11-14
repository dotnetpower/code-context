#!/usr/bin/env python3
"""Configure Copilot SDK and VS Code subagent definitions to support Code Context (cctx) and Test Context (tctx)."""

from __future__ import annotations

import argparse
import os
import tempfile
from pathlib import Path

MARKER = "# [cctx-subagent-instructions]"

EXPLORE_EXTRA_TOOLS = (
    "tool_search_tool",
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "cctxNativeStatus",
    "cctxNativeSync",
    "cctxNativeWorkspaces",
    "code-context-native_codeFiles",
    "code-context-native_codeGrep",
    "code-context-native_codeContext",
    "code-context-native_codeRead",
    "code-context-native_codeStatus",
    "code-context-native_codeSync",
    "code-context-native_codeWorkspaces",
    "code-context/*",
    "code-context-code_files",
    "code-context-code_grep",
    "code-context-code_context",
    "code-context-code_read",
    "code-context-code_status",
    "code-context-code_sync",
    "code-context-code_workspaces",
    "tctxNativeRun",
    "tctxNativeAffected",
    "tctxNativeGraph",
    "tctxNativeStatus",
    "tctxNativeSync",
    "tctxNativeInvalidate",
    "tctxNativeWorkspaces",
    "test-context-native_testRun",
    "test-context-native_testAffected",
    "test-context-native_testGraph",
    "test-context-native_testStatus",
    "test-context-native_testSync",
    "test-context-native_testInvalidate",
    "test-context-native_testWorkspaces",
    "test-context/*",
    "test-context-test_run",
    "test-context-test_affected",
    "test-context-test_graph",
    "test-context-test_status",
    "test-context-test_sync",
    "test-context-test_invalidate",
    "test-context-test_workspaces",
    "mcp__code-context__*",
    "mcp__test-context__*",
)

YAML_PROMPT_BLOCK = """\
  # [cctx-subagent-instructions]
  **Code Context (`cctx`) & Test Context (`tctx`) Priority (MANDATORY FIRST PATH):**
  - For locating or reading saved source in a registered workspace, ALWAYS use Code Context FIRST before `view`, `grep`, `rg`, `glob`, or `bash`:
    1. Prefer native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles` FIRST when callable; otherwise (when no UI client is connected) use MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files` (or `mcp__code-context__*`).
    2. Always pass the explicit absolute `root` parameter (shown in `<environment_context>` or your task prompt). Keep `cctxNativeContext` / `code_context` `limit <= 64` (`radius 0..100`) and keep `cctxNativeRead` / `code_read` batches `<= 120` total lines (`<8KiB`) so responses stay inline.
    3. Do NOT pass hidden paths (`.github/*`, `.vscode/*`, `.gitignore`) or `/tmp/*` files to Code Context; use `view` directly for those.
    4. If Code Context or Test Context tools are deferred or not visible in your active tool list, call `tool_search` (`tool_search_tool`) FIRST with at most 4 exact names per call (`tool_search_tool` caps results at 5): `"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles"` (fallback `"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files"`; tests `"tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected"`).
    5. If a native `cctxNative*` or `tctxNative*` call fails with `"No client with the advertised tool definition"`, `"No matching client connected"`, or `"skipped from another client"`, immediately switch to `code-context-code_*` / `test-context-test_*` MCP tools (or `cctx --backend shared --root <root>` / `tctx --root <root> run` CLI).
    6. On `CACHE_MISS`, use a permitted host read/search for that single lookup only, then resume using Code Context on subsequent turns.
    7. For running or inspecting tests/lints (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`, `ruff`, `eslint`) or AST test dependency graphs in a registered workspace, ALWAYS use native `tctxNativeRun`, `tctxNativeAffected`, `tctxNativeGraph` (or MCP `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph` when native dispatch is unavailable) FIRST before `bash`, passing the explicit absolute `root`.
"""

MD_PROMPT_BLOCK = """
<!-- [cctx-subagent-instructions] -->
## Code Context (`cctx`) & Test Context (`tctx`) Priority (Mandatory First Path)

- For locating or reading saved source in a registered workspace, ALWAYS use `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` (or MCP `code-context-code_*` when native dispatch is unavailable) FIRST before built-in search/read tools.
- For running or inspecting tests/lints or AST test dependency graphs in a registered workspace, ALWAYS use `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` (or MCP `test-context-test_*` when native dispatch is unavailable) FIRST before shell test/lint commands.
- Always pass the explicit absolute `root` parameter when known, keep `cctxNativeContext` `limit <= 64`, and keep read batches `<= 120` lines (`<8KiB`). Never send `.github/*`, `.vscode/*`, or `/tmp/*` paths to Code Context.
- If tools are deferred, discover them with `tool_search` using `<= 4` exact names per call: `"cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles"` (fallback `"code-context-code_context code-context-code_read code-context-code_grep code-context-code_files"`; tests `"tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected"`).
- On `CACHE_MISS`, use a permitted host search/read for that single lookup only, then resume using Code Context on subsequent turns.
"""

YAML_FORMULA_PROMPT_BLOCK = """\
  # [cctx-subagent-instructions]
  CCTX & TCTX Formula Rules:
  - Source: cctxNative*{Context,Read,Grep,Files} > MCP(code-context-code_* | mcp__code-context__*) > cctx --backend shared --root <root> > Host(view,rg,glob)
  - Tests & Probes: tctxNative*{Run,Affected,Graph,Status} > MCP(test-context-test_* | mcp__test-context__*) > tctx --root <root> run > bash ; pctx {http|json|table|image} > python3 -c/curl/jq
  - Guards: pass explicit root; symbol+context => cctxNativeContext/code-context-code_context(paths=[file],radius:25..50,limit<=64) (!1..150 slicing); multi-range => 1 cctxNativeRead/code-context-code_read(ranges,<=120L,<8KiB); .github/*,.vscode/*,.gitignore,/tmp/*,/dev/shm/* => view(!CCTX).
  - Deferred (even if grep/view visible on turn 0): call tool_search_tool (tool_search) FIRST: "cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles" (<=5 cap) | MCP "code-context-code_context code-context-code_read code-context-code_grep code-context-code_files" | tests "tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected" (regex: "cctxNative|code-context|tctxNative|test-context").
  - Transitions: Error("No client with the advertised tool definition"|"No matching client connected"|"skipped from another client") => code-context-code_* / test-context-test_* MCP | CLI ; CACHE_MISS => 1 Host read -> resume CCTX.
"""

MD_FORMULA_PROMPT_BLOCK = """
<!-- [cctx-subagent-instructions] -->
## CCTX & TCTX Formula Rules
- Source: #cctxNative*{Context,Read,Grep,Files} > MCP(code-context-code_* | mcp__code-context__*) > Host(rg,view,glob)
- Tests & Probes: #tctxNative*{Run,Affected,Graph,Status} > MCP(test-context-test_* | mcp__test-context__*) > bash ; pctx {http|json|table|image} > python3 -c/curl/jq
- Guards: pass root; limit<=64; Read(ranges,<=120L,<8KiB); .github/*,.vscode/*,/tmp/*,/dev/shm/* => Host view.
- Deferred: call tool_search_tool (tool_search) FIRST: "cctxNativeContext cctxNativeRead cctxNativeGrep cctxNativeFiles" | MCP "code-context-code_context code-context-code_read code-context-code_grep code-context-code_files" | tests "tctxNativeRun tctxNativeAffected test-context-test_run test-context-test_affected" (regex "cctxNative|code-context|tctxNative|test-context").
- CACHE_MISS => 1 Host read -> resume CCTX.
"""


def select_blocks(style: str = "formula") -> tuple[str, str]:
    normalized = (style or os.environ.get("CCTX_PROMPT_STYLE", "formula")).strip().lower()
    if normalized in {"prose", "standard", "text"}:
        return YAML_PROMPT_BLOCK, MD_PROMPT_BLOCK
    return YAML_FORMULA_PROMPT_BLOCK, MD_FORMULA_PROMPT_BLOCK

TARGET_YAML_AGENTS = (
    "explore.agent.yaml",
    "code-review.agent.yaml",
    "rubber-duck.agent.yaml",
    "security-review.agent.yaml",
    "research.agent.yaml",
    "task.agent.yaml",
    "sidekick/github-context.yaml",
    "sidekick/github-context-memory.yaml",
)

MD_EXTRA_TOOLS = (
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "cctxNativeStatus",
    "cctxNativeWorkspaces",
    "code-context/*",
    "tctxNativeRun",
    "tctxNativeAffected",
    "tctxNativeGraph",
    "tctxNativeStatus",
    "tctxNativeWorkspaces",
    "test-context/*",
)


def atomic_write(path: Path, content: str) -> None:
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp_name, path)
    except BaseException:
        Path(tmp_name).unlink(missing_ok=True)
        raise


def patch_yaml_agent(path: Path, style: str = "formula") -> bool:
    if not path.is_file():
        return False
    yaml_block, _ = select_blocks(style)
    original = path.read_text(encoding="utf-8")
    updated = original

    has_explicit_tools = (
        "\ntools:\n" in updated
        and '  - "*"\n' not in updated
        and "strictToolsList: true" not in updated
    )
    if has_explicit_tools:
        missing_tools = [t for t in EXPLORE_EXTRA_TOOLS if f"  - {t}\n" not in updated]
        if missing_tools:
            block = "  # Code Context & Test Context tools (read-only)\n" + "".join(
                f"  - {t}\n" for t in missing_tools
            )
            if "  - lsp\n" in updated:
                updated = updated.replace("  - lsp\n", "  - lsp\n" + block, 1)
            elif "\npromptParts:\n" in updated:
                updated = updated.replace("\npromptParts:\n", "\n" + block + "promptParts:\n", 1)
            elif "\nprompt: |\n" in updated:
                updated = updated.replace("\nprompt: |\n", "\n" + block + "prompt: |\n", 1)

    if MARKER in updated:
        head, _sep, tail = updated.partition(f"  {MARKER}")
        if "\nsidekick:\n" in tail:
            _old_block, _, after_sidekick = tail.partition("\nsidekick:\n")
            updated = head.rstrip() + "\n\n" + yaml_block + "\nsidekick:\n" + after_sidekick
        else:
            updated = head.rstrip() + "\n\n" + yaml_block
    elif "\nprompt: |\n" in updated:
        if "\nsidekick:\n" in updated:
            before_sk, _, after_sk = updated.partition("\nsidekick:\n")
            updated = before_sk.rstrip() + "\n\n" + yaml_block + "\nsidekick:\n" + after_sk
        else:
            updated = updated.rstrip() + "\n\n" + yaml_block

    if updated != original:
        atomic_write(path, updated)
        return True
    return False


def patch_explore_agent_md(path: Path, style: str = "formula") -> bool:
    if not path.is_file():
        return False
    _, md_block = select_blocks(style)
    original = path.read_text(encoding="utf-8")
    updated = original
    if "tools: [" in updated:
        use_quotes = "tools: ['" in updated or 'tools: ["' in updated
        for tool_name in reversed(MD_EXTRA_TOOLS):
            token = f"'{tool_name}'" if use_quotes else tool_name
            if tool_name not in updated:
                updated = updated.replace("tools: [", f"tools: [{token}, ", 1)
    elif "\ntools:\n" in updated and '  - "*"\n' not in updated:
        for tool_name in reversed(MD_EXTRA_TOOLS):
            if f"  - {tool_name}\n" not in updated:
                updated = updated.replace("\ntools:\n", f"\ntools:\n  - {tool_name}\n", 1)
    if "<!-- [cctx-subagent-instructions] -->" in updated:
        head, _sep, _tail = updated.partition("<!-- [cctx-subagent-instructions] -->")
        updated = head.rstrip() + "\n" + md_block
    else:
        updated = updated.rstrip() + "\n" + md_block
    if updated != original:
        atomic_write(path, updated)
        return True
    return False


def discover_definition_dirs(home: Path) -> list[Path]:
    patterns = (
        ".vscode-server/bin/*/node_modules/@github/copilot-sdk-*/definitions",
        ".vscode-server/bin/*/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
        ".vscode-server/cli/servers/*/server/node_modules/@github/copilot-sdk-*/definitions",
        ".vscode-server/cli/servers/*/server/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
        ".cache/copilot/pkg/*/*/definitions",
        ".cache/github-copilot-sdk/*/*/definitions",
        ".cache/github-copilot-sdk/*/*/*/definitions",
    )
    found: list[Path] = []
    seen: set[Path] = set()
    for pattern in patterns:
        for candidate in sorted(home.glob(pattern)):
            if candidate.is_dir() and candidate not in seen:
                seen.add(candidate)
                found.append(candidate)
    if str(home).startswith("/home/") and Path("/mnt/c/Users").is_dir():
        win_patterns = (
            "*/AppData/Local/Programs/Microsoft VS Code/*/resources/app/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
            "*/AppData/Local/Programs/Microsoft VS Code/resources/app/extensions/copilot/node_modules/@github/copilot/sdk/definitions",
            "*/AppData/Local/Programs/Microsoft VS Code/*/resources/app/node_modules.asar.unpacked/@github/copilot-sdk-*/definitions",
            "*/AppData/Local/Programs/Microsoft VS Code/resources/app/node_modules.asar.unpacked/@github/copilot-sdk-*/definitions",
        )
        for pattern in win_patterns:
            for candidate in sorted(Path("/mnt/c/Users").glob(pattern)):
                if candidate.is_dir() and candidate not in seen:
                    seen.add(candidate)
                    found.append(candidate)
    return found


def discover_explore_md_files(home: Path) -> list[Path]:
    patterns = (
        ".vscode-server/data/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
        ".config/Code/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
        ".vscode-server/data/agentPlugins/**/agents/*.agent.md",
    )
    found: list[Path] = []
    seen: set[Path] = set()
    for pattern in patterns:
        for candidate in sorted(home.glob(pattern)):
            if candidate.is_file() and candidate not in seen:
                seen.add(candidate)
                found.append(candidate)
    if str(home).startswith("/home/") and Path("/mnt/c/Users").is_dir():
        win_md_pattern = "*/AppData/Roaming/Code/User/globalStorage/github.copilot-chat/*-agent/*.agent.md"
        for candidate in sorted(Path("/mnt/c/Users").glob(win_md_pattern)):
            if candidate.is_file() and candidate not in seen:
                seen.add(candidate)
                found.append(candidate)
    return found


def configure_all(
    home: Path,
    extra_dirs: list[Path] | None = None,
    style: str = "formula",
) -> dict[str, int | str]:
    dirs = list(extra_dirs or []) + discover_definition_dirs(home)
    patched_yaml = 0
    checked_yaml = 0
    for def_dir in dirs:
        for name in TARGET_YAML_AGENTS:
            target = def_dir / name
            if target.is_file():
                checked_yaml += 1
                if patch_yaml_agent(target, style=style):
                    patched_yaml += 1
    patched_md = 0
    checked_md = 0
    for md_path in discover_explore_md_files(home):
        checked_md += 1
        if patch_explore_agent_md(md_path, style=style):
            patched_md += 1
    return {
        "style": style,
        "definition_dirs": len(dirs),
        "checked_yaml": checked_yaml,
        "patched_yaml": patched_yaml,
        "checked_md": checked_md,
        "patched_md": patched_md,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=Path.home())
    parser.add_argument("--definitions-dir", type=Path, action="append", default=[])
    parser.add_argument(
        "--style",
        choices=("formula", "hybrid", "prose"),
        default=os.environ.get("CCTX_PROMPT_STYLE", "formula"),
        help="Prompt instruction style to inject into subagent definitions (default: formula)",
    )
    args = parser.parse_args()
    summary = configure_all(args.home, args.definitions_dir, style=args.style)
    print(
        f"Subagent definitions configured (style={summary['style']}): dirs={summary['definition_dirs']} "
        f"yaml={summary['patched_yaml']}/{summary['checked_yaml']} "
        f"md={summary['patched_md']}/{summary['checked_md']}"
    )


if __name__ == "__main__":
    main()
