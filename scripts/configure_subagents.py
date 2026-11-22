#!/usr/bin/env python3
"""Configure Copilot SDK and VS Code subagent definitions to support Code Context (cctx) and Test Context (tctx)."""

from __future__ import annotations

import argparse
import json
import os
import tempfile
from pathlib import Path

MARKER = "# [cctx-subagent-instructions]"
END_MARKER = "# [/cctx-subagent-instructions]"
MD_MARKER = "<!-- [cctx-subagent-instructions] -->"
MD_END_MARKER = "<!-- [/cctx-subagent-instructions] -->"

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
    1. Prefer backend MCP `code-context-code_context`, `code-context-code_read`, `code-context-code_grep`, `code-context-code_files` (or `mcp__code-context__*`) FIRST when callable so `Reload Window` or headless execution never stalls on UI client dispatch; otherwise use native `cctxNativeContext`, `cctxNativeRead`, `cctxNativeGrep`, `cctxNativeFiles`.
    2. Always pass the explicit absolute `root` parameter (shown in `<environment_context>` or your task prompt). Keep `code_context` / `cctxNativeContext` `limit <= 64` (`radius 0..100`) and keep `code_read` / `cctxNativeRead` batches `<= 120` total lines (`<8KiB`) so responses stay inline.
    3. Do NOT pass hidden paths (`.github/*`, `.vscode/*`, `.gitignore`) or `/tmp/*` files to Code Context; use `view` directly for those.
    4. If Code Context or Test Context tools are deferred or not visible in your active tool list, call `tool_search` (`tool_search_tool`) FIRST with at most 2 exact names per call, in two parallel calls (results cap at 5 and GitHub-* tools crowd out the rest; a 4-name query dropped `code_read`/`code_files`): `"code-context-code_context code-context-code_grep"` and `"code-context-code_read code-context-code_files"` (fallback `"cctxNativeContext cctxNativeGrep"` and `"cctxNativeRead cctxNativeFiles"`; tests `"test-context-test_run test-context-test_affected"`). Confirm all four loaded before the first lookup.
    4b. `bash` is not exempt: `grep`, `rg`, `sed -n`, `cat`, `head`, `tail`, `find`, `ls` on saved source map to `code_grep`, `code_read`, `code_files` (or one `code_context`); keep `bash` for builds, tests and git.
    5. If a native `cctxNative*` or `tctxNative*` call fails with `"No client with the advertised tool definition"`, `"No matching client connected"`, or `"skipped from another client"`, immediately switch to `code-context-code_*` / `test-context-test_*` MCP tools (or `cctx --backend shared --root <root>` / `tctx --root <root> run` CLI).
    6. On `CACHE_MISS`, use a permitted host read/search for that single lookup only, then resume using Code Context on subsequent turns.
    7. For running or inspecting tests/lints (`pytest`, `cargo test`, `vitest`, `jest`, `node --test`, `ruff`, `eslint`) or AST test dependency graphs in a registered workspace, ALWAYS use backend MCP `test-context-test_run`, `test-context-test_affected`, `test-context-test_graph` (or native `tctxNativeRun`, `tctxNativeAffected`, `tctxNativeGraph` when MCP is unavailable) FIRST before `bash`, passing the explicit absolute `root`.
  # [/cctx-subagent-instructions]
"""

MD_PROMPT_BLOCK = """
<!-- [cctx-subagent-instructions] -->
## Code Context (`cctx`) & Test Context (`tctx`) Priority (Mandatory First Path)

- For locating or reading saved source in a registered workspace, ALWAYS use MCP `code-context-code_*` (or `#cctxNativeContext`, `#cctxNativeRead`, `#cctxNativeGrep`, and `#cctxNativeFiles` when backend MCP is unavailable) FIRST before built-in search/read tools.
- For running or inspecting tests/lints or AST test dependency graphs in a registered workspace, ALWAYS use MCP `test-context-test_*` (or `#tctxNativeRun`, `#tctxNativeAffected`, `#tctxNativeGraph`, and `#tctxNativeStatus` when backend MCP is unavailable) FIRST before shell test/lint commands.
- Always pass the explicit absolute `root` parameter when known, keep `code_context` / `cctxNativeContext` `limit <= 64`, and keep read batches `<= 120` lines (`<8KiB`). Never send `.github/*`, `.vscode/*`, or `/tmp/*` paths to Code Context.
- If tools are deferred, discover them with `tool_search` using `<= 2` exact names per call in two parallel calls (a 4-name query drops `code_read`/`code_files`): `"code-context-code_context code-context-code_grep"` and `"code-context-code_read code-context-code_files"` (fallback `"cctxNativeContext cctxNativeGrep"` and `"cctxNativeRead cctxNativeFiles"`; tests `"test-context-test_run test-context-test_affected"`).
- `bash` is not exempt: `grep`/`rg`/`sed -n`/`cat`/`head`/`find` on saved source map to `code_grep`/`code_read`/`code_files` (or one `code_context`).
- On `CACHE_MISS`, use a permitted host search/read for that single lookup only, then resume using Code Context on subsequent turns.
<!-- [/cctx-subagent-instructions] -->
"""

YAML_FORMULA_PROMPT_BLOCK = """\
  # [cctx-subagent-instructions]
  CCTX & TCTX Formula Rules:
  - Source: MCP(code-context-code_* | mcp__code-context__*) > cctxNative*{Context,Read,Grep,Files} > cctx --backend shared --root <root> > Host(view,rg,glob)
  - Tests & Probes: MCP(test-context-test_* | mcp__test-context__*) > tctxNative*{Run,Affected,Graph,Status} > tctx --root <root> run > bash ; pctx {http|json|table|image} > python3 -c/curl/jq
  - Guards: pass explicit root; symbol+context => code-context-code_context/cctxNativeContext(paths=[file],radius:25..50,limit<=64) (!1..150 slicing); multi-range => 1 code-context-code_read/cctxNativeRead(ranges,<=120L,<8KiB); .github/*,.vscode/*,.gitignore,/tmp/*,/dev/shm/* => view(!CCTX).
  - Deferred (even if grep/view visible on turn 0): call tool_search_tool (tool_search) FIRST, <=2 names/call in 2 parallel calls (4 names drop code_read/code_files): MCP "code-context-code_context code-context-code_grep" + "code-context-code_read code-context-code_files" | Native "cctxNativeContext cctxNativeGrep" + "cctxNativeRead cctxNativeFiles" | tests "test-context-test_run test-context-test_affected"; all 4 loaded before lookup. bash grep|rg|sed -n|cat|head|find on source => same swap.
  - Transitions: Error("No client with the advertised tool definition"|"No matching client connected"|"skipped from another client") => code-context-code_* / test-context-test_* MCP | CLI ; CACHE_MISS => 1 Host read -> resume CCTX.
  # [/cctx-subagent-instructions]
"""

MD_FORMULA_PROMPT_BLOCK = """
<!-- [cctx-subagent-instructions] -->
## CCTX & TCTX Formula Rules
- Source: MCP(code-context-code_* | mcp__code-context__*) > #cctxNative*{Context,Read,Grep,Files} > Host(rg,view,glob)
- Tests & Probes: MCP(test-context-test_* | mcp__test-context__*) > #tctxNative*{Run,Affected,Graph,Status} > bash ; pctx {http|json|table|image} > python3 -c/curl/jq
- Guards: pass root; limit<=64; Read(ranges,<=120L,<8KiB); .github/*,.vscode/*,/tmp/*,/dev/shm/* => Host view.
- Deferred: call tool_search_tool (tool_search) FIRST, <=2 names/call in 2 parallel calls (4 names drop code_read/code_files): MCP "code-context-code_context code-context-code_grep" + "code-context-code_read code-context-code_files" | Native "cctxNativeContext cctxNativeGrep" + "cctxNativeRead cctxNativeFiles" | tests "test-context-test_run test-context-test_affected". bash grep|rg|sed -n|cat|head|find on source => same swap.
- CACHE_MISS => 1 Host read -> resume CCTX.
<!-- [/cctx-subagent-instructions] -->
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


def _find_standalone_marker_line(text_slice: str, marker: str, min_indent: int = 0) -> tuple[int, int] | None:
    offset = 0
    for raw_line in text_slice.splitlines(keepends=True):
        clean = raw_line.rstrip("\r\n")
        spaces = len(clean) - len(clean.lstrip(" "))
        if spaces >= min_indent and clean[spaces:].rstrip() == marker:
            return offset, offset + len(raw_line)
        offset += len(raw_line)
    return None


def _parse_literal_block_header(header_line: str) -> tuple[bool, int | None]:
    clean = header_line.rstrip("\r\n")
    if not clean.startswith("prompt:"):
        return False, None
    rest = clean[len("prompt:") :].lstrip(" \t")
    if not rest.startswith("|"):
        return False, None
    after = rest[1:]
    i = 0
    chomp_seen = False
    explicit_indent: int | None = None
    for _ in range(2):
        if i < len(after) and after[i] in "-+" and not chomp_seen:
            chomp_seen = True
            i += 1
        elif i < len(after) and after[i] in "123456789" and explicit_indent is None:
            explicit_indent = int(after[i])
            i += 1
        else:
            break
    tail = after[i:]
    if tail:
        if tail[0] not in " \t":
            return False, None
        trimmed = tail.lstrip(" \t")
        if trimmed and not trimmed.startswith("#"):
            return False, None
    return True, explicit_indent


def _locate_prompt_literal_block(text: str) -> tuple[int, int, str, bool] | None:
    idx = text.find("\nprompt:")
    if idx >= 0:
        start = idx + 1
    elif text.startswith("prompt:"):
        start = 0
    else:
        return None
    nl = text.find("\n", start)
    if nl < 0:
        return None
    ok_hdr, explicit = _parse_literal_block_header(text[start:nl])
    if not ok_hdr:
        return None
    body_start = nl + 1
    after_hdr = text[body_start:]
    top_off = _find_top_level_key_offset(after_hdr)
    body_end = (body_start + top_off) if top_off >= 0 else len(text)
    if explicit is not None:
        return body_start, body_end, " " * explicit, True
    body = text[body_start:body_end]
    max_blank_spaces = 0
    for raw_line in body.splitlines():
        line = raw_line.rstrip("\r")
        if not line.strip(" "):
            if all(ch == " " for ch in line):
                max_blank_spaces = max(max_blank_spaces, len(line))
            else:
                return None
            continue
        spaces = len(line) - len(line.lstrip(" "))
        if spaces >= 1 and max_blank_spaces <= spaces:
            return body_start, body_end, " " * spaces, False
        return None
    return body_start, body_end, " " * max(2, max_blank_spaces), False


def _reindent_yaml_block(block: str, indent: str) -> str:
    out_lines: list[str] = []
    for line in block.splitlines():
        if not line.strip():
            out_lines.append("")
        else:
            core = line[2:] if line.startswith("  ") else line.lstrip(" ")
            out_lines.append(f"{indent}{core}")
    return "\n".join(out_lines) + "\n"


def _is_top_level_yaml_key(line: str) -> bool:
    s = line.rstrip("\r\n")
    if not s or s.startswith(" ") or s.startswith("#"):
        return False
    if ":" not in s:
        return False
    key, rest = s.split(":", 1)
    return bool(key) and all(ch.isalnum() or ch in "_-" for ch in key) and (not rest or rest.startswith(" "))


def _find_top_level_key_offset(body: str) -> int:
    offset = 0
    for line in body.splitlines(keepends=True):
        if _is_top_level_yaml_key(line):
            return offset
        offset += len(line)
    return -1


def _is_legacy_cctx_line(line: str) -> bool:
    s = line.strip()
    if not s:
        return True
    prefixes = (
        "CCTX & TCTX Formula Rules",
        "**Code Context (`cctx`)",
        "- Source:",
        "- Tests & Probes:",
        "- Guards:",
        "- Deferred",
        "- Transitions:",
        "- For locating or reading saved source",
        "1. Prefer ",
        "2. Always pass ",
        "3. Do NOT pass ",
        "4. If Code Context or Test Context tools",
        "4. Even when `grep`/`view`/`glob`",
        "4b. ",
        "5. If a native ",
        "6. On `CACHE_MISS`",
        "7. For running or inspecting tests",
        "8. For HTTP/OpenAPI",
    )
    return any(s.startswith(p) for p in prefixes)


def _find_yaml_tools_section(text: str) -> tuple[int, int] | None:
    if text.startswith("tools:\n") or text.startswith("tools:\r\n"):
        key_start = 0
    else:
        pos = text.find("\ntools:\n")
        if pos < 0:
            pos = text.find("\ntools:\r\n")
        if pos < 0:
            return None
        key_start = pos + 1
    nl = text.find("\n", key_start)
    if nl < 0:
        return None
    body_start = nl + 1
    offset = body_start
    for raw_line in text[body_start:].splitlines(keepends=True):
        clean = raw_line.rstrip("\r\n")
        leading = len(clean) - len(clean.lstrip(" "))
        stripped = clean.lstrip(" ")
        if not stripped or (leading >= 2 and (stripped.startswith("#") or stripped.startswith("- "))):
            offset += len(raw_line)
        else:
            break
    return body_start, offset


_PLAIN_TOOL_CHARS = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-.*:/@'")
_ESC_MAP = {'"': '"', "\\": "\\", "/": "/", "n": "\n", "r": "\r", "t": "\t"}


def _parse_yaml_tool_scalar(raw: str, *, allow_trailing_comment: bool = False) -> str | None:
    trimmed = raw.strip()
    if not trimmed:
        return None
    if trimmed[0] in ("'", '"'):
        q = trimmed[0]
        i = 1
        chars: list[str] = []
        closed = False
        while i < len(trimmed):
            if q == '"' and trimmed[i] == "\\":
                if i + 1 >= len(trimmed):
                    return None
                if trimmed[i + 1] == "u" and i + 5 < len(trimmed):
                    hex_part = trimmed[i + 2 : i + 6]
                    if not all(c in "0123456789abcdefABCDEF" for c in hex_part):
                        return None
                    code = int(hex_part, 16)
                    if 0xD800 <= code <= 0xDFFF:
                        return None
                    chars.append(chr(code))
                    i += 6
                    continue
                if trimmed[i + 1] not in _ESC_MAP:
                    return None
                chars.append(_ESC_MAP[trimmed[i + 1]])
                i += 2
                continue
            if q == "'" and trimmed[i] == "'" and i + 1 < len(trimmed) and trimmed[i + 1] == "'":
                chars.append("'")
                i += 2
                continue
            if trimmed[i] == q:
                closed = True
                i += 1
                break
            chars.append(trimmed[i])
            i += 1
        if not closed:
            return None
        rest = trimmed[i:].strip()
        if rest and not (allow_trailing_comment and rest.startswith("#")):
            return None
        val = "".join(chars)
        return val if val else None
    body = trimmed.split(" #", 1)[0].rstrip() if (allow_trailing_comment and " #" in trimmed) else trimmed
    if (
        not body
        or body[0] in "-@*:?"
        or ": " in body
        or body.endswith(":")
        or any(c not in _PLAIN_TOOL_CHARS for c in body)
    ):
        return None
    return body


def _parse_inline_tools_brackets(line: str) -> tuple[int, int] | None:
    if not line.startswith("tools:"):
        return None
    rest = line[len("tools:") :]
    ws = len(rest) - len(rest.lstrip(" \t"))
    after_ws = rest[ws:]
    if not after_ws.startswith("["):
        return None
    open_idx = len("tools:") + ws
    i = open_idx + 1
    quote = ""
    at_item_start = True
    while i < len(line):
        ch = line[i]
        if quote:
            if ch == "\\" and quote == '"' and i + 1 < len(line):
                i += 2
                continue
            if quote == "'" and ch == "'" and i + 1 < len(line) and line[i + 1] == "'":
                i += 2
                continue
            if ch == quote:
                quote = ""
            i += 1
            continue
        if ch.isspace():
            i += 1
            continue
        if ch == ",":
            at_item_start = True
            i += 1
            continue
        if at_item_start and ch in ("'", '"'):
            quote = ch
            at_item_start = False
            i += 1
            continue
        if ch == "]":
            tail = line[i + 1 :].strip()
            if not tail or tail.startswith("#"):
                return open_idx, i
            return None
        at_item_start = False
        i += 1
    return None


def _find_yaml_inline_tools_span(text: str) -> tuple[int, int] | None:
    offset = 0
    for raw_line in text.splitlines(keepends=True):
        clean = raw_line.rstrip("\r\n")
        brackets = _parse_inline_tools_brackets(clean)
        if brackets is not None:
            return offset + brackets[0], offset + brackets[1]
        offset += len(raw_line)
    return None


def _parse_inline_yaml_items(inner: str) -> list[str] | None:
    items: list[str] = []
    i = 0
    n = len(inner)
    while i < n:
        while i < n and inner[i].isspace():
            i += 1
        if i >= n:
            break
        if inner[i] == ",":
            return None
        item_start = i
        if inner[i] in ("'", '"'):
            q = inner[i]
            i += 1
            closed = False
            while i < n:
                if q == '"' and inner[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                if q == "'" and inner[i] == "'" and i + 1 < n and inner[i + 1] == "'":
                    i += 2
                    continue
                if inner[i] == q:
                    closed = True
                    i += 1
                    break
                i += 1
            if not closed:
                return None
            while i < n and inner[i].isspace():
                i += 1
            item_end = i
            if i < n:
                if inner[i] != ",":
                    return None
                i += 1
            parsed = _parse_yaml_tool_scalar(inner[item_start:item_end], allow_trailing_comment=False)
            if parsed is None:
                return None
            items.append(parsed)
        else:
            while i < n and inner[i] != ",":
                i += 1
            parsed = _parse_yaml_tool_scalar(inner[item_start:i], allow_trailing_comment=False)
            if parsed is None:
                return None
            items.append(parsed)
            if i < n and inner[i] == ",":
                i += 1
    return items


def _parse_block_yaml_items(tools_body: str) -> list[str] | None:
    expected: int | None = None
    items: list[str] = []
    for raw_line in tools_body.splitlines():
        clean = raw_line.rstrip("\r")
        if not clean.strip():
            continue
        spaces = len(clean) - len(clean.lstrip(" "))
        if spaces < 2:
            return None
        rest = clean[spaces:]
        if rest.startswith("#"):
            continue
        if not rest.startswith("- "):
            return None
        if expected is None:
            expected = spaces
        elif spaces != expected:
            return None
        parsed = _parse_yaml_tool_scalar(rest[2:], allow_trailing_comment=True)
        if parsed is None:
            return None
        items.append(parsed)
    return items


def _has_any_top_level_tools_key(text: str) -> bool:
    for raw_line in text.splitlines():
        clean = raw_line.rstrip("\r")
        if clean == "tools:" or clean.startswith("tools: ") or clean.startswith("tools:\t"):
            return True
    return False


def _detect_tools_item_indent(tools_body: str) -> str:
    for raw_line in tools_body.splitlines():
        clean = raw_line.rstrip("\r")
        spaces = len(clean) - len(clean.lstrip(" "))
        if spaces >= 2 and clean[spaces:].startswith("- "):
            return " " * spaces
    return "  "


def _has_top_level_strict_tools(text: str) -> bool:
    return text.startswith("strictToolsList: true") or "\nstrictToolsList: true" in text


def _yaml_top_level_structure_valid(text: str) -> bool:
    state = "none"
    seen_keys: set[str] = set()
    for raw_line in text.splitlines():
        line = raw_line.rstrip("\r")
        if not line.strip():
            continue
        if not line.startswith(" "):
            if line.startswith("#"):
                continue
            if not _is_top_level_yaml_key(line):
                return False
            key, rest = line.split(":", 1)
            if key in seen_keys:
                return False
            seen_keys.add(key)
            val = rest.strip()
            if not val:
                state = "container"
            elif val.startswith("|") or val.startswith(">"):
                state = "block"
            else:
                state = "scalar"
        elif state in ("none", "scalar"):
            return False
    return True


def _inject_yaml_extra_tools(updated: str) -> str:
    if _has_top_level_strict_tools(updated):
        return updated
    inline_sec = _find_yaml_inline_tools_span(updated)
    if inline_sec is not None:
        open_abs, close_abs = inline_sec
        inner = updated[open_abs + 1 : close_abs]
        parsed_items = _parse_inline_yaml_items(inner)
        if parsed_items is None or "*" in parsed_items:
            return updated
        quote = "'" if "'" in inner else ('"' if '"' in inner else "")
        existing_items = set(parsed_items)
        additions: list[str] = []
        for t in EXPLORE_EXTRA_TOOLS:
            if t not in existing_items:
                additions.append(f"'{t}'" if (not quote and "*" in t) else f"{quote}{t}{quote}")
        if additions:
            trimmed_end = inner.rstrip()
            if not trimmed_end:
                prefix = ""
            elif trimmed_end.endswith(","):
                prefix = " "
            else:
                prefix = ", "
            return updated[:close_abs] + prefix + ", ".join(additions) + updated[close_abs:]
        return updated
    sec = _find_yaml_tools_section(updated)
    if sec is None:
        return updated
    tools_start, tools_end = sec
    tools_body = updated[tools_start:tools_end]
    parsed_block_items = _parse_block_yaml_items(tools_body)
    if parsed_block_items is None or "*" in parsed_block_items:
        return updated
    item_indent = _detect_tools_item_indent(tools_body)
    missing_tools = [t for t in EXPLORE_EXTRA_TOOLS if t not in parsed_block_items]
    if not missing_tools:
        return updated
    block = f"{item_indent}# Code Context & Test Context tools (read-only)\n" + "".join(
        f"{item_indent}- {t}\n" for t in missing_tools
    )
    lsp_needle = f"{item_indent}- lsp\n"
    lsp_rel = tools_body.find(lsp_needle)
    if lsp_rel >= 0:
        insert_at = tools_start + lsp_rel + len(lsp_needle)
        return updated[:insert_at] + block + updated[insert_at:]
    insert_at = tools_end
    while insert_at > tools_start and updated[:insert_at].endswith("\n\n"):
        insert_at -= 1
    if insert_at > tools_start and not updated[:insert_at].endswith("\n"):
        block = "\n" + block
    return updated[:insert_at] + block + updated[insert_at:]


def _split_after_legacy_cctx_block(after_marker_line: str) -> str:
    cursor = 0
    last_cctx_end = 0
    for line in after_marker_line.splitlines(keepends=True):
        if _is_top_level_yaml_key(line):
            return after_marker_line[cursor:]
        if not line.strip():
            cursor += len(line)
            continue
        if not _is_legacy_cctx_line(line):
            return after_marker_line[last_cctx_end:]
        cursor += len(line)
        last_cctx_end = cursor
    return after_marker_line[last_cctx_end:]


def _is_valid_yaml_agent(text: str) -> bool:
    if MARKER not in text or not _yaml_top_level_structure_valid(text):
        return False
    strict_tools = _has_top_level_strict_tools(text)
    inline_sec = _find_yaml_inline_tools_span(text)
    if inline_sec is not None:
        inner = text[inline_sec[0] + 1 : inline_sec[1]]
        parsed_items = _parse_inline_yaml_items(inner)
        if parsed_items is None:
            return False
        is_wildcard = "*" in parsed_items
        if (
            not strict_tools
            and not is_wildcard
            and ("cctxNativeContext" not in parsed_items or "code-context/*" not in parsed_items)
        ):
            return False
    else:
        sec = _find_yaml_tools_section(text)
        if sec is not None:
            tools_body = text[sec[0] : sec[1]]
            parsed_block = _parse_block_yaml_items(tools_body)
            if parsed_block is None:
                return False
            is_wildcard = "*" in parsed_block
            if (
                not strict_tools
                and not is_wildcard
                and ("cctxNativeContext" not in parsed_block or "code-context/*" not in parsed_block)
            ):
                return False
        elif _has_any_top_level_tools_key(text):
            return False
    located = _locate_prompt_literal_block(text)
    if located is None:
        return False
    p_body_start, p_body_end, expected_indent, has_explicit_indent = located
    prompt_body = text[p_body_start:p_body_end]
    saw_first_content = False
    saw_marker = False
    for line in prompt_body.splitlines():
        clean = line.rstrip("\r")
        if not clean.strip(" "):
            if any(ch != " " for ch in clean):
                return False
            if has_explicit_indent and len(clean) >= len(expected_indent):
                saw_first_content = True
                continue
            if not saw_first_content and not has_explicit_indent and len(clean) > len(expected_indent):
                return False
            continue
        saw_first_content = True
        if not clean.startswith(expected_indent):
            return False
        if clean.strip() == MARKER:
            saw_marker = True
    if not saw_marker:
        return False
    try:
        import yaml  # type: ignore[import-untyped]
    except ImportError:
        return True
    try:
        parsed = yaml.safe_load(text)
        return (
            isinstance(parsed, dict)
            and isinstance(parsed.get("prompt"), str)
            and any(l.strip() == MARKER for l in parsed["prompt"].splitlines())
        )
    except Exception:
        return False


class _SubagentDirLock:
    def __init__(self, directory: Path) -> None:
        self._path = directory / ".cctx-subagent.lock"
        self._fd: int | None = None
        self._mode: str = ""

    def __enter__(self) -> bool:
        try:
            self._fd = os.open(self._path, os.O_CREAT | os.O_RDWR, 0o600)
        except OSError:
            return False
        try:
            import fcntl

            fcntl.flock(self._fd, fcntl.LOCK_EX)
            self._mode = "fcntl"
            return True
        except ImportError:
            pass
        except OSError:
            os.close(self._fd)
            self._fd = None
            return False
        try:
            import msvcrt  # type: ignore[import-not-found]

            os.lseek(self._fd, 0, os.SEEK_SET)
            if os.fstat(self._fd).st_size == 0:
                os.write(self._fd, b"\0")
                os.lseek(self._fd, 0, os.SEEK_SET)
            msvcrt.locking(self._fd, msvcrt.LK_LOCK, 1)
            self._mode = "msvcrt"
            return True
        except Exception:
            os.close(self._fd)
            self._fd = None
            return False

    def __exit__(self, *_exc: object) -> None:
        if self._fd is not None:
            try:
                if self._mode == "fcntl":
                    import fcntl

                    fcntl.flock(self._fd, fcntl.LOCK_UN)
                elif self._mode == "msvcrt":
                    import msvcrt  # type: ignore[import-not-found]

                    os.lseek(self._fd, 0, os.SEEK_SET)
                    msvcrt.locking(self._fd, msvcrt.LK_UNLCK, 1)
            except Exception:
                pass
            os.close(self._fd)
            self._fd = None


def patch_yaml_agent(path: Path, style: str = "formula") -> bool:
    if not path.is_file():
        return False
    with _SubagentDirLock(path.parent) as locked:
        if not locked:
            return False
        raw_yaml_block, _ = select_blocks(style)
        original = path.read_text(encoding="utf-8")
        if _locate_prompt_literal_block(original) is None:
            return False
        updated = _inject_yaml_extra_tools(original)
        located = _locate_prompt_literal_block(updated)
        if located is None:
            return False
        prompt_body_start, prompt_body_end, indent, _ = located
        yaml_block = _reindent_yaml_block(raw_yaml_block, indent)
        prompt_slice = updated[prompt_body_start:prompt_body_end]

        found_marker = _find_standalone_marker_line(prompt_slice, MARKER, len(indent))
        if found_marker is not None:
            rel_line_start, rel_line_end = found_marker
            line_start = prompt_body_start + rel_line_start
            after_marker_abs = prompt_body_start + rel_line_end
            head = updated[:line_start].rstrip("\r\n")
            while len(head) > prompt_body_start and head.endswith("\n\n"):
                head = head[:-1]
            head = head.rstrip("\r\n")
            prompt_after_marker = updated[after_marker_abs:prompt_body_end]
            found_end = _find_standalone_marker_line(prompt_after_marker, END_MARKER, len(indent))
            if found_end is not None:
                rest = updated[after_marker_abs + found_end[1] :]
                if not rest.strip("\r\n"):
                    updated = head + "\n\n" + yaml_block
                elif _is_top_level_yaml_key(rest.splitlines()[0]):
                    updated = head + "\n\n" + yaml_block + "\n" + rest
                else:
                    updated = head + "\n\n" + yaml_block + rest
            else:
                after_marker_line = updated[after_marker_abs:]
                rest = _split_after_legacy_cctx_block(after_marker_line)
                if not rest.strip("\r\n"):
                    updated = head + "\n\n" + yaml_block
                elif _is_top_level_yaml_key(rest.splitlines()[0]):
                    updated = head + "\n\n" + yaml_block + "\n" + rest
                else:
                    updated = head + "\n\n" + yaml_block + rest
        elif prompt_body_end < len(updated):
            before_key = updated[:prompt_body_end].rstrip("\r\n")
            after_key = updated[prompt_body_end:]
            updated = before_key + "\n\n" + yaml_block + "\n" + after_key
        else:
            before_eof = updated.rstrip("\r\n")
            sep = "\n" if prompt_body_start == len(updated) else "\n\n"
            updated = before_eof + sep + yaml_block

        if not _is_valid_yaml_agent(updated):
            return False

        if updated != original:
            atomic_write(path, updated)
            return True
        return False


def _split_after_legacy_md_block(after_marker: str) -> str:
    prefixes = (
        "## Code Context (`cctx`)",
        "## CCTX & TCTX Formula Rules",
        "- For locating or reading saved source",
        "- For running or inspecting tests",
        "- Always pass the explicit absolute",
        "- If tools are deferred",
        "- Even when built-in",
        "- `bash` is not exempt",
        "- On `CACHE_MISS`",
        "- Source:",
        "- Tests & Probes:",
        "- Guards:",
        "- Deferred:",
        "- CACHE_MISS =>",
    )
    consumed = 0
    for line in after_marker.splitlines(keepends=True):
        s = line.strip()
        if not s or any(s.startswith(p) for p in prefixes):
            consumed += len(line)
        else:
            return after_marker[consumed:]
    return ""


def patch_explore_agent_md(path: Path, style: str = "formula") -> bool:
    if not path.is_file():
        return False
    with _SubagentDirLock(path.parent) as locked:
        if not locked:
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
        found_md = _find_standalone_marker_line(updated, MD_MARKER, 0)
        if found_md is not None:
            head = updated[: found_md[0]].rstrip()
            after_md_start = updated[found_md[1] :]
            found_md_end = _find_standalone_marker_line(after_md_start, MD_END_MARKER, 0)
            if found_md_end is not None:
                rest = after_md_start[found_md_end[1] :].lstrip("\r\n")
                updated = (head + "\n" + md_block) if not rest else (head + "\n" + md_block.rstrip() + "\n\n" + rest)
            else:
                rest = _split_after_legacy_md_block(after_md_start).lstrip("\r\n")
                updated = (head + "\n" + md_block) if not rest else (head + "\n" + md_block.rstrip() + "\n\n" + rest)
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


def discover_explore_md_files(home: Path, *, include_codex: bool = False) -> list[Path]:
    patterns: list[str] = [
        ".vscode-server/data/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
        ".config/Code/User/globalStorage/github.copilot-chat/*-agent/*.agent.md",
    ]
    if include_codex:
        patterns.append(".codex/agents/*.md")
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


CODEX_MCP_MARKER = "# [cctx-codex-mcp]"


def configure_codex_mcp(home: Path, executable: Path | None = None) -> bool:
    codex_dir = home / ".codex"
    if not codex_dir.is_dir():
        return False
    config_path = codex_dir / "config.toml"
    exe = str(executable or (home / ".local" / "bin" / "cctx"))
    snippet = (
        f"{CODEX_MCP_MARKER}\n"
        "[mcp_servers.code-context]\n"
        f"command = {json.dumps(exe)}\n"
        'args = ["shared-mcp"]\n'
    )
    original = config_path.read_text(encoding="utf-8") if config_path.is_file() else ""
    if "[mcp_servers.code-context]" in original or '[mcp_servers."code-context"]' in original:
        return False
    updated = (original.rstrip() + "\n\n" + snippet) if original.strip() else snippet
    atomic_write(config_path, updated)
    return True


def configure_all(
    home: Path,
    extra_dirs: list[Path] | None = None,
    style: str = "formula",
    codex_mcp: bool = False,
) -> dict[str, int | str | bool]:
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
    for md_path in discover_explore_md_files(home, include_codex=codex_mcp):
        checked_md += 1
        if patch_explore_agent_md(md_path, style=style):
            patched_md += 1
    codex_configured = configure_codex_mcp(home) if codex_mcp else False
    return {
        "style": style,
        "definition_dirs": len(dirs),
        "checked_yaml": checked_yaml,
        "patched_yaml": patched_yaml,
        "checked_md": checked_md,
        "patched_md": patched_md,
        "codex_mcp_configured": codex_configured,
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
    parser.add_argument(
        "--codex-mcp",
        action="store_true",
        help="Also register [mcp_servers.code-context] in ~/.codex/config.toml if ~/.codex exists",
    )
    args = parser.parse_args()
    summary = configure_all(args.home, args.definitions_dir, style=args.style, codex_mcp=args.codex_mcp)
    print(
        f"Subagent definitions configured (style={summary['style']}): dirs={summary['definition_dirs']} "
        f"yaml={summary['patched_yaml']}/{summary['checked_yaml']} "
        f"md={summary['patched_md']}/{summary['checked_md']} "
        f"codex_mcp={summary['codex_mcp_configured']}"
    )


if __name__ == "__main__":
    main()
