#!/usr/bin/env python3
import argparse
import json
import os
import re
import fnmatch
import tempfile
import time
from collections import Counter
from datetime import datetime
from math import ceil
from pathlib import Path
from statistics import median

CATALOG_NAMES = (
    "cctxNativeFiles",
    "cctxNativeGrep",
    "cctxNativeContext",
    "cctxNativeRead",
    "code-context-native_codeGrep",
    "code-context-native_codeContext",
    "code-context-native_codeRead",
    "code-context-native_codeFiles",
    "code-context-code_grep",
    "code-context-code_context",
    "code-context-code_read",
    "code-context-code_files",
)


def classify(name: str) -> str:
    if name == "tool_search_tool":
        return "other"
    if name.startswith(("cctxNative", "code-context-native")):
        return "native"
    if name.startswith("code-context-") or name in {
        "code_files",
        "code_grep",
        "code_context",
        "code_read",
        "code_status",
        "code_sync",
        "code_workspaces",
    }:
        return "mcp"
    lowered = name.lower()
    if name in {
        "rg",
        "view",
        "glob",
        "usages",
        "read_file",
        "grep_search",
        "file_search",
        "find_by_name",
        "list_dir",
        "copilot_findFiles",
        "copilot_findTextInFiles",
        "copilot_readFile",
    } or any(
        part in lowered for part in ("search", "grep", "readfile")
    ):
        return "builtin_search_read"
    return "other"


def is_lookup_tool(name: str) -> bool:
    return classify(name) == "builtin_search_read" or any(
        name.endswith(suffix) for suffix in (
            "code_files", "code_grep", "code_context", "code_read",
            "codeFiles", "codeGrep", "codeContext", "codeRead",
            "NativeFiles", "NativeGrep", "NativeContext", "NativeRead",
        )
    )


def event_timestamp(event: dict) -> float | None:
    metadata = event.get("_ahpLog", {})
    if not isinstance(metadata, dict):
        return None
    value = metadata.get("ts", event.get("timestamp"))
    if isinstance(value, (int, float)):
        return float(value / 1000 if value > 10_000_000_000 else value)
    if isinstance(value, str):
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
        except ValueError:
            return None
    return None


def recent_trace_paths(home: Path, cutoff: float) -> tuple[list[Path], int]:
    paths = []
    skipped = 0
    for path in home.glob(".vscode-server/data/logs/*/ahp/*.jsonl"):
        try:
            if path.stat().st_mtime >= cutoff:
                paths.append(path)
        except FileNotFoundError:
            skipped += 1
    return paths, skipped


def recent_session_paths(home: Path, cutoff: float) -> tuple[list[Path], int]:
    paths = []
    skipped = 0
    state_home = Path(os.environ.get("XDG_STATE_HOME", str(home)))
    for path in (state_home / ".copilot/session-state").glob("*/events.jsonl"):
        try:
            if path.stat().st_mtime >= cutoff:
                paths.append(path)
        except FileNotFoundError:
            skipped += 1
    return paths, skipped


def result_usefulness(value, depth: int = 0) -> bool | None:
    if depth > 6:
        return None
    if isinstance(value, str):
        cleaned = re.sub(r"\n?CCTX_TRACE \{[^\n]*\}\s*$", "", value)
        if cleaned.startswith("Output too large to read at once"):
            marker = "Preview (first 500 chars):\n"
            if marker in cleaned:
                preview = cleaned.split(marker, 1)[1]
                header = re.match(r"^(?:matches|files)=(\d+)\s+truncated=(?:true|false)", preview)
                if header:
                    return int(header[1]) > 0
                if re.search(r"(?:^|\n)--- .+:\d+-\d+(?: sha256=[0-9a-f]{64})?\n", preview):
                    return True
                if preview.startswith(("{", "[")) and '"total_matches":0' not in preview and '"snippets":[]' not in preview:
                    return True
            return None
        try:
            parsed = json.loads(cleaned)
        except json.JSONDecodeError:
            header = re.match(r"^(?:matches|files)=(\d+)\s+truncated=(?:true|false)", cleaned)
            if header:
                return int(header[1]) > 0
            if cleaned == "snippets=0":
                return False
            if re.search(r"(?:^|\n)--- .+:\d+-\d+(?: sha256=[0-9a-f]{64})?\n", cleaned):
                return True
            if cleaned.startswith("page=true complete="):
                return "--- " in cleaned
            return None
        return result_usefulness(parsed, depth + 1)
    if isinstance(value, dict):
        total = value.get("total_matches")
        if isinstance(total, int) and not isinstance(total, bool) and total >= 0:
            return total > 0
        for key in ("search", "snippets", "content", "text", "value"):
            if key in value:
                measured = result_usefulness(value[key], depth + 1)
                if measured is not None:
                    return measured
    if isinstance(value, list):
        if not value:
            return False
        if all(isinstance(item, dict) and isinstance(item.get("lines"), list) for item in value):
            return any(item["lines"] for item in value)
        measured = [result_usefulness(item, depth + 1) for item in value]
        if any(item is True for item in measured):
            return True
        if all(item is False for item in measured):
            return False
    return None


def extract_result_text(value, depth: int = 0) -> str:
    if depth > 6 or value is None:
        return ""
    if isinstance(value, str):
        return re.sub(r"\n?CCTX_TRACE \{[^\n]*\}\s*$", "", value)
    if isinstance(value, dict):
        for key in ("content", "text", "value", "output"):
            if key in value and value[key] is not None:
                extracted = extract_result_text(value[key], depth + 1)
                if extracted:
                    return extracted
    if isinstance(value, list):
        parts = [extract_result_text(item, depth + 1) for item in value]
        return "\n".join(part for part in parts if part)
    return ""


def is_turn_compressed_call(tool: str, arguments: dict) -> bool:
    if not isinstance(arguments, dict):
        return False
    if tool.endswith(("code_context", "codeContext", "NativeContext")):
        return True
    if tool.endswith(("code_read", "codeRead", "NativeRead")):
        ranges = arguments.get("ranges")
        if isinstance(ranges, list):
            if len(ranges) > 1:
                return True
            if any(
                isinstance(item, dict) and "#" in str(item.get("path") or item.get("filePath") or "")
                for item in ranges
            ):
                return True
        path = str(arguments.get("path") or arguments.get("filePath") or "")
        if "#" in path:
            return True
    return False


def estimate_call_tokens_saved(tool: str, arguments: dict, payload_tokens: int, success: bool | None) -> int:
    if success is not True or classify(tool) not in {"native", "mcp"} or not is_lookup_tool(tool):
        return 0
    if tool.endswith(("code_context", "codeContext", "NativeContext")):
        return max(1600, int(round(payload_tokens * 1.85)))
    if tool.endswith(("code_read", "codeRead", "NativeRead")):
        has_anchor = is_turn_compressed_call(tool, arguments)
        floor = 1400 if has_anchor else 900
        multiplier = 1.75 if has_anchor else 1.10
        return max(floor, int(round(payload_tokens * multiplier)))
    if tool.endswith(("code_grep", "codeGrep", "NativeGrep", "code_files", "codeFiles", "NativeFiles")):
        return max(1100, int(round(payload_tokens * 1.35)))
    return 0


def avoided_turns_for_call(tool: str, turn_compressed: bool, success: bool | None) -> float:
    if success is not True or classify(tool) not in {"native", "mcp"} or not is_lookup_tool(tool):
        return 0.0
    return 1.0 if turn_compressed else 0.25


def outcome_counts(calls: list[dict]) -> dict:
    successes = [call for call in calls if call["success"] is True]
    failed_cctx_errors = [
        json.dumps(call.get("error"), ensure_ascii=False)
        for call in calls
        if call["success"] is False and classify(call.get("tool", "")) in {"native", "mcp"}
    ]
    return {
        "useful_results": sum(call.get("useful") is True for call in successes),
        "empty_results": sum(call.get("useful") is False for call in successes),
        "unmeasured_successes": sum(call.get("useful") is None for call in successes),
        "expected_cache_misses": sum("CACHE_MISS root=" in error for error in failed_cctx_errors),
        "cache_load_failures": sum("CACHE_LOAD_FAILED root=" in error for error in failed_cctx_errors),
    }


def request_eligibility(tool: str, arguments: dict, cwd: str | None, coverage: dict | None, now: float) -> dict:
    unknown = {"classification": "unknown", "reason": "no_fresh_coverage_or_structured_target"}
    if not coverage or not isinstance(coverage.get("generated_at_unix"), (int, float)):
        return unknown
    if not 0 <= now - coverage["generated_at_unix"] <= 600:
        return {"classification": "unknown", "reason": "coverage_snapshot_stale"}
    # Do not parse, execute, or rewrite arbitrary shell commands.
    if tool in {"bash", "run_in_terminal"}:
        return {"classification": "unknown", "reason": "shell_command_not_interpreted"}
    target_root = (
        arguments.get("root")
        or arguments.get("workspaceRoot")
        or arguments.get("workspace_root")
        or arguments.get("projectRoot")
        or arguments.get("project_root")
        or arguments.get("cwd")
        or cwd
    )
    paths = []
    is_read = tool in {"view", "read_file"} or tool.endswith(("NativeRead", "codeRead", "code_read"))
    if is_read:
        if tool in {"view", "read_file"}:
            value = (
                arguments.get("path")
                or arguments.get("filePath")
                or arguments.get("file_path")
                or arguments.get("filepath")
            )
            paths = [value] if isinstance(value, str) else []
        else:
            ranges = arguments.get("ranges", [])
            if isinstance(ranges, list) and ranges:
                paths = [
                    item.get("path") or item.get("filePath") or item.get("file_path")
                    for item in ranges
                    if isinstance(item, dict)
                    and isinstance(item.get("path") or item.get("filePath") or item.get("file_path"), str)
                ]
            elif isinstance(arguments.get("paths"), list):
                paths = [p for p in arguments["paths"] if isinstance(p, str)]
            else:
                single = (
                    arguments.get("path")
                    or arguments.get("filePath")
                    or arguments.get("file_path")
                    or arguments.get("file")
                )
                paths = [single] if isinstance(single, str) else []
    else:
        values = (
            arguments.get("paths")
            or arguments.get("path")
            or arguments.get("file_path")
            or arguments.get("filePath")
            or arguments.get("dir")
            or arguments.get("directory")
        )
        if isinstance(values, str):
            paths = [values]
        elif isinstance(values, list):
            paths = [path for path in values if isinstance(path, str)]
        else:
            paths = [target_root] if isinstance(target_root, str) else []
    if not paths:
        return unknown
    verdicts = []
    for target in paths:
        if is_read and tool in {"view", "read_file"} and "copilot-tool-output" in target:
            verdicts.append(("legitimate_fallback", "copilot_offloaded_tool_output"))
            continue
        path = Path(target)
        if not path.is_absolute():
            if not target_root or not Path(target_root).is_absolute():
                return unknown
            path = Path(target_root) / path
        if ".." in path.parts:
            return {"classification": "unknown", "reason": "path_traversal"}
        # lexical matching avoids following symlinks or reading a policy-excluded target
        candidates = [entry for entry in coverage.get("roots", [])
                      if isinstance(entry, dict) and isinstance(entry.get("registration"), dict)
                      and isinstance(entry["registration"].get("root"), str)
                      and path.is_relative_to(entry["registration"]["root"])]
        if not candidates:
            preparation = coverage.get("preparation_candidates", {})
            if not isinstance(preparation, dict):
                return {"classification": "unknown", "reason": "preparation_metadata_invalid"}
            preparable = preparation.get("roots", [])
            if any(isinstance(item, dict) and isinstance(item.get("root"), str)
                   and path.is_relative_to(item["root"]) for item in preparable):
                verdicts.append(("unknown", "authorized_worktree_requires_preparation"))
            elif preparation.get("truncated") or preparation.get("errors"):
                verdicts.append(("unknown", "preparation_metadata_incomplete"))
            else:
                verdicts.append(("legitimate_fallback", "unregistered_root"))
            continue
        entry = max(candidates, key=lambda item: len(Path(item["registration"]["root"]).parts))
        spec = entry["registration"]
        relative = path.relative_to(spec["root"]).as_posix()
        if any(part.startswith(".") and part != "." for part in Path(relative).parts):
            verdicts.append(("legitimate_fallback", "hidden_target"))
            continue
        if entry.get("error") or not isinstance(entry.get("files"), list):
            verdicts.append(("unknown", "inventory_unavailable"))
            continue
        admitted = entry["files"]
        if is_read:
            clean_rel = relative.split("#", 1)[0]
            if ":" in clean_rel and clean_rel.rsplit(":", 1)[1].replace("-", "").replace(":", "").isdigit():
                clean_rel = clean_rel.split(":", 1)[0]
            if relative in admitted or clean_rel in admitted:
                verdicts.append(("eligible", "admitted_file"))
            else:
                verdicts.append(("legitimate_fallback", "not_in_permitted_inventory"))
        else:
            glob = arguments.get("glob")
            selected = [p for p in admitted if relative == "." or p == relative or p.startswith(relative.rstrip("/") + "/")]
            if glob:
                selected = [p for p in selected if fnmatch.fnmatchcase(p, glob) or fnmatch.fnmatchcase(Path(p).name, glob)]
            # Scope-wide built-in searches can include excluded files; do not call
            # them equivalent merely because some indexed files overlap.
            if tool in {"rg", "grep", "glob", "grep_search", "file_search"}:
                verdicts.append(("unknown", "builtin_search_corpus_not_proven_equivalent"))
            elif selected:
                verdicts.append(("eligible", "admitted_search_corpus"))
            else:
                verdicts.append(("unknown", "no_admitted_search_target"))
    if all(state == "eligible" for state, _ in verdicts):
        return {"classification": "eligible", "reason": verdicts[0][1]}
    if any(state == "legitimate_fallback" for state, _ in verdicts):
        return {"classification": "legitimate_fallback", "reason": next(reason for state, reason in verdicts if state == "legitimate_fallback")}
    return {"classification": "unknown", "reason": verdicts[0][1]}


def session_adoption(calls: list[dict], sample_limit: int, usage_records: list[dict] | None = None) -> dict:
    usage_records = usage_records or []
    lookup = [call for call in calls if is_lookup_tool(call["tool"])]
    reasons = Counter(call["eligibility"]["reason"] for call in lookup)
    eligible = sorted(
        (call for call in lookup if call["eligibility"]["classification"] == "eligible"),
        key=lambda call: call["started_at"],
    )[:sample_limit]
    cctx = [call for call in eligible if classify(call["tool"]) in {"native", "mcp"}]
    all_cctx = [call for call in lookup if classify(call["tool"]) in {"native", "mcp"}]
    all_builtin = [call for call in lookup if classify(call["tool"]) == "builtin_search_read"]
    cctx_outcomes = outcome_counts(all_cctx)
    cctx_warm_hits = max(0, len(all_cctx) - cctx_outcomes["expected_cache_misses"] - cctx_outcomes["cache_load_failures"])
    model_turns = Counter(rec["display_name"] for rec in usage_records if rec.get("display_name"))
    for call in calls:
        if call.get("model") and not model_turns:
            model_turns[call["model"]] += 1
    cwd = next((call.get("cwd") for call in calls if call.get("cwd")), None) or next(
        (rec.get("cwd") for rec in usage_records if rec.get("cwd")), None
    )
    input_tok = sum(rec.get("input_tokens", 0) for rec in usage_records)
    cache_read_tok = sum(rec.get("cache_read_tokens", 0) for rec in usage_records)
    output_tok = sum(rec.get("output_tokens", 0) for rec in usage_records)
    reasoning_tok = sum(rec.get("reasoning_tokens", 0) for rec in usage_records)
    nano_aiu = sum(rec.get("nano_aiu", 0) for rec in usage_records)
    used_aic = round(nano_aiu / 1e9, 4) if nano_aiu else 0.0
    direct_saved = sum(call.get("estimated_tokens_saved", 0) for call in all_cctx)
    turn_compressed = sum(1 for call in all_cctx if call.get("success") is True and call.get("turn_compressed"))
    avoided_turns = sum(avoided_turns_for_call(call["tool"], bool(call.get("turn_compressed")), call.get("success")) for call in all_cctx)
    turn_saved = int(round(avoided_turns * 220_000))
    compounded_saved = direct_saved * 35
    saved_aic = round((direct_saved * 0.00055) + (avoided_turns * 4.20), 4)
    eligible_lookup = [
        call for call in lookup
        if not call.get("non_source_host_only") and not call.get("pre_edit_view")
    ]
    return {
        "workspace": Path(cwd).name if isinstance(cwd, str) and cwd else "-",
        "cwd": cwd,
        "primary_model": model_turns.most_common(1)[0][0] if model_turns else None,
        "models": dict(model_turns),
        "turns": len(usage_records),
        "input_tokens": input_tok,
        "cache_read_tokens": cache_read_tok,
        "prompt_cache_hit_percent": round(100 * cache_read_tok / input_tok, 2) if input_tok else None,
        "output_tokens": output_tok,
        "reasoning_tokens": reasoning_tok,
        "used_aic": used_aic,
        "used_usd_0001": round(used_aic * 0.001, 4),
        "used_usd_api_001": round(used_aic * 0.01, 4),
        "lookup_calls": len(lookup),
        "eligible_lookup_calls": len(eligible_lookup),
        "cctx_calls": len(all_cctx),
        "builtin_calls": len(all_builtin),
        "cctx_selection_percent": round(100 * len(all_cctx) / len(eligible_lookup), 2) if eligible_lookup else None,
        "cctx_cache_hit_percent": round(100 * cctx_warm_hits / len(all_cctx), 2) if all_cctx else None,
        "cctx_payload_tokens": sum(call.get("payload_tokens", 0) for call in all_cctx),
        "builtin_payload_tokens": sum(call.get("payload_tokens", 0) for call in all_builtin),
        "turn_compression_calls": turn_compressed,
        "direct_tokens_saved": direct_saved,
        "compounded_history_tokens_saved": compounded_saved,
        "estimated_turn_tokens_saved": turn_saved,
        "estimated_total_tokens_saved": direct_saved + compounded_saved + turn_saved,
        "saved_aic": saved_aic,
        "saved_usd_0001": round(saved_aic * 0.001, 4),
        "saved_usd_api_001": round(saved_aic * 0.01, 4),
        "classifications": dict(Counter(call["eligibility"]["classification"] for call in lookup)),
        "reasons": dict(reasons),
        "first_eligible_sample": {
            "limit": sample_limit, "count": len(eligible), "cctx_calls": len(cctx),
            "selection_percent": round(100 * len(cctx) / len(eligible), 2) if eligible else None,
            "avoidable_builtin_calls": len(eligible) - len(cctx),
            "failures": sum(call["success"] is False for call in cctx),
            **outcome_counts(cctx),
        },
        "time_basis": "current coverage replay, not proof of historical registration, host policy or cache readiness",
        "tools": dict(Counter(call["tool"] for call in calls)),
        "shell_commands_unclassified": sum(call["tool"] in {"bash", "run_in_terminal"} for call in calls),
    }


def build_report(paths: list[Path], hours: float, now: float | None = None,
                 coverage: dict | None = None, sample_limit: int = 20) -> dict:
    now = time.time() if now is None else now
    cutoff = now - hours * 3600
    calls: dict[tuple[str, str], dict] = {}
    usage_records: list[dict] = []
    sessions: set[str] = set()
    skipped = 0
    catalog_mentions = Counter()
    for path in paths:
        cwd = None
        last_session_model = None
        last_session_display = None
        last_model_by_agent: dict[str, tuple[str, str]] = {}
        subagent_types_by_id: dict[str, str] = {}
        try:
            lines = path.open(errors="replace")
        except FileNotFoundError:
            skipped += 1
            continue
        with lines:
            for raw in lines:
                if (
                    '"tool.execution_' not in raw
                    and '"session.usage_record"' not in raw
                    and '"session.start"' not in raw
                    and '"subagent.started"' not in raw
                    and '"chat/toolCall' not in raw
                    and not any(name in raw for name in CATALOG_NAMES)
                ):
                    continue
                try:
                    event = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                if not isinstance(event, dict):
                    continue
                if event.get("type") == "session.start":
                    context = event.get("data", {}).get("context", {})
                    if isinstance(context, dict) and isinstance(context.get("cwd"), str):
                        cwd = context["cwd"]
                if event.get("type") == "subagent.started":
                    sdata = event.get("data", {})
                    if isinstance(sdata, dict):
                        atype = str(sdata.get("agentName") or sdata.get("agentType") or "subagent")
                        for sid in (event.get("agentId"), sdata.get("toolCallId")):
                            if isinstance(sid, str) and sid:
                                subagent_types_by_id[sid] = atype
                                if isinstance(sdata.get("model"), str) and sdata["model"]:
                                    last_model_by_agent[sid] = (sdata["model"], sdata["model"])
                    continue
                timestamp = event_timestamp(event)
                if timestamp is None or timestamp < cutoff or timestamp > now:
                    continue
                if event.get("type") == "session.usage_record":
                    data = event.get("data", {})
                    usage = data.get("usage", {}) if isinstance(data, dict) else {}
                    if isinstance(usage, dict):
                        model_id = str(usage.get("model") or "unknown")
                        display_name = str(usage.get("modelDisplayName") or model_id)
                        effort = str(usage.get("reasoningEffort") or "-")
                        agent_id = event.get("agentId") or usage.get("parentToolCallId")
                        is_subagent = bool(
                            agent_id
                            or usage.get("initiator") == "sub-agent"
                            or usage.get("interactionType") == "conversation-subagent"
                        )
                        if isinstance(agent_id, str) and agent_id:
                            last_model_by_agent[agent_id] = (model_id, display_name)
                        if not is_subagent:
                            last_session_model = model_id
                            last_session_display = display_name
                        agent_role = (
                            subagent_types_by_id.get(str(agent_id), "subagent")
                            if is_subagent
                            else "main"
                        )
                        session_channel = f"session:{path.parent.name}"
                        sessions.add(session_channel)
                        copilot_usage = usage.get("copilotUsage", {})
                        nano_aiu = copilot_usage.get("totalNanoAiu", 0) if isinstance(copilot_usage, dict) else 0
                        usage_records.append({
                            "channel": session_channel,
                            "cwd": cwd,
                            "timestamp": timestamp,
                            "model": model_id,
                            "display_name": display_name,
                            "reasoning_effort": effort,
                            "is_subagent": is_subagent,
                            "agent_role": agent_role,
                            "input_tokens": int(usage.get("inputTokens") or 0),
                            "output_tokens": int(usage.get("outputTokens") or 0),
                            "cache_read_tokens": int(usage.get("cacheReadTokens") or 0),
                            "cache_write_tokens": int(usage.get("cacheWriteTokens") or 0),
                            "reasoning_tokens": int(usage.get("reasoningTokens") or 0),
                            "tool_token_count": int(usage.get("toolTokenCount") or 0),
                            "duration_ms": float(usage.get("duration") or 0.0),
                            "ttft_ms": float(usage.get("timeToFirstTokenMs") or 0.0),
                            "nano_aiu": int(nano_aiu or 0),
                        })
                    continue
                params = event.get("params", {})
                if not isinstance(params, dict):
                    continue
                channel = params.get("channel", "")
                if not isinstance(channel, str):
                    channel = ""
                if timestamp >= cutoff:
                    for name in CATALOG_NAMES:
                        if name in raw:
                            catalog_mentions[name] += 1
                    if channel.startswith("ahp-chat:"):
                        sessions.add(channel)
                action = params.get("action", {})
                if event.get("type") in {"tool.execution_start", "tool.execution_complete"}:
                    data = event.get("data", {})
                    if not isinstance(data, dict):
                        continue
                    channel = f"session:{path.parent.name}"
                    if timestamp >= cutoff:
                        sessions.add(channel)
                    action = {
                        **data,
                        "type": "chat/toolCallStart" if event["type"] == "tool.execution_start" else "chat/toolCallComplete",
                    }
                    if event["type"] == "tool.execution_complete":
                        action["result"] = {**data, "content": data.get("result")}
                if not isinstance(action, dict):
                    continue
                action_type = action.get("type")
                call_id = action.get("toolCallId")
                if not isinstance(call_id, str) or not call_id:
                    continue
                call_channel = channel or f"trace:{path}"
                key = (call_channel, call_id)
                if action_type == "chat/toolCallStart":
                    call = calls.setdefault(
                        key,
                        {
                            "tool": "unknown",
                            "channel": channel,
                            "started_at": None,
                            "completed_at": None,
                            "success": None,
                            "arguments": {},
                        },
                    )
                    tool = action.get("toolName", "unknown")
                    call["tool"] = tool if isinstance(tool, str) else "unknown"
                    call["channel"] = call_channel
                    call["started_at"] = timestamp
                    arguments = action.get("arguments", {})
                    if not isinstance(arguments, dict):
                        arguments = {}
                    call["arguments"] = arguments
                    if call["tool"] == "task" and isinstance(arguments.get("agent_type"), str):
                        subagent_types_by_id[call_id] = arguments["agent_type"]
                    call["eligibility"] = request_eligibility(call["tool"], arguments, cwd, coverage, now)
                    call["cwd"] = cwd
                    agent_id = event.get("agentId") or action.get("parentToolCallId")
                    parent_tc = action.get("parentToolCallId")
                    is_subagent = bool(agent_id)
                    agent_role = (
                        subagent_types_by_id.get(str(agent_id))
                        or (subagent_types_by_id.get(str(parent_tc)) if parent_tc else None)
                        or "subagent"
                    ) if is_subagent else "main"
                    call["is_subagent"] = is_subagent
                    call["agent_role"] = agent_role
                    fallback_model, fallback_display = (
                        last_model_by_agent.get(str(agent_id), (last_session_model, last_session_display))
                        if is_subagent
                        else (last_session_model, last_session_display)
                    )
                    call_model = action.get("model")
                    call["model"] = str(call_model) if isinstance(call_model, str) and call_model else fallback_model
                    call["model_display"] = fallback_display or call["model"]
                    call["turn_compressed"] = is_turn_compressed_call(call["tool"], arguments)
                    target_str = str(
                        arguments.get("path")
                        or arguments.get("paths")
                        or arguments.get("filePath")
                        or arguments.get("file_path")
                        or ""
                    )
                    call["non_source_host_only"] = classify(call["tool"]) == "builtin_search_read" and (
                        any(
                            token in target_str
                            for token in (
                                "/tmp/",
                                "copilot-tool-output",
                                "/.copilot",
                                "/.github",
                                "/.vscode",
                                "/.git",
                                ".gitignore",
                                ".png",
                                ".jpg",
                                ".pdf",
                                ".vsix",
                            )
                        )
                        or any(
                            part.startswith(".") and part not in {".", ".."}
                            for part in Path(target_str).parts
                            if target_str and not target_str.startswith("[")
                        )
                    )
                    call["cctx_cli_fallback"] = call["tool"] in {"bash", "run_in_terminal"} and (
                        "cctx --backend shared" in str(arguments.get("command") or "")
                    )
                elif action_type == "chat/toolCallComplete":
                    call = calls.setdefault(
                        key,
                        {
                            "tool": "unknown",
                            "channel": call_channel,
                            "started_at": None,
                            "completed_at": None,
                            "success": None,
                            "arguments": {},
                        },
                    )
                    call["completed_at"] = timestamp
                    if not call.get("model"):
                        agent_id = event.get("agentId") or action.get("parentToolCallId")
                        fallback_model, fallback_display = (
                            last_model_by_agent.get(str(agent_id), (last_session_model, last_session_display))
                            if agent_id
                            else (last_session_model, last_session_display)
                        )
                        call_model = action.get("model")
                        call["model"] = str(call_model) if isinstance(call_model, str) and call_model else fallback_model
                        call["model_display"] = fallback_display or call["model"]
                    result = action.get("result", {})
                    if not isinstance(result, dict):
                        result = {}
                    success = result.get("success")
                    call["success"] = success if isinstance(success, bool) else None
                    call["useful"] = result_usefulness(result) if success is True else None
                    text_payload = extract_result_text(result)
                    payload_chars = len(text_payload)
                    payload_tokens = ceil(payload_chars / 4) if payload_chars > 0 else 0
                    call["payload_chars"] = payload_chars
                    call["payload_tokens"] = payload_tokens
                    call["offloaded"] = text_payload.startswith("Output too large to read at once")
                    if call.get("tool") == "tool_search_tool":
                        call["tool_search_cctx_hit"] = any(
                            token in text_payload
                            for token in ("cctxNative", "code-context-code_")
                        )
                        call["tool_search_no_client_miss"] = (
                            "No client is connected" in text_payload
                            and "No tool name matched" in text_payload
                        )
                    error = result.get("error")
                    if error:
                        call["error"] = error
    calls = {
        key: call
        for key, call in calls.items()
        if isinstance(call["started_at"], (int, float)) and call["started_at"] >= cutoff
    }
    for call in calls.values():
        if not is_lookup_tool(call["tool"]):
            call["useful"] = None
        call["estimated_tokens_saved"] = estimate_call_tokens_saved(
            call["tool"],
            call.get("arguments", {}),
            call.get("payload_tokens", 0),
            call.get("success"),
        )
    tool_counts = Counter(call["tool"] for call in calls.values())
    success_counts = Counter(
        call["tool"] for call in calls.values() if call["success"] is True
    )
    failure_counts = Counter(
        call["tool"] for call in calls.values() if call["success"] is False
    )
    grouped: dict[str, dict] = {}
    for tool, count in tool_counts.most_common():
        group = classify(tool)
        tool_call_list = [call for call in calls.values() if call["tool"] == tool]
        payload_toks = [call.get("payload_tokens", 0) for call in tool_call_list if call.get("payload_tokens", 0) > 0]
        grouped.setdefault(group, {})[tool] = {
            "calls": count,
            "successes": success_counts[tool],
            "failures": failure_counts[tool],
            "incomplete": count - success_counts[tool] - failure_counts[tool],
            **outcome_counts(tool_call_list),
            "payload_tokens": sum(payload_toks),
            "avg_payload_tokens": round(sum(payload_toks) / len(payload_toks), 1) if payload_toks else 0,
            "offloaded_calls": sum(1 for call in tool_call_list if call.get("offloaded")),
            "estimated_tokens_saved": sum(call.get("estimated_tokens_saved", 0) for call in tool_call_list),
        }
    group_totals = {}
    for group in sorted({classify(call["tool"]) for call in calls.values()}):
        group_calls = [call for call in calls.values() if classify(call["tool"]) == group]
        durations = sorted(
            (call["completed_at"] - call["started_at"]) * 1000
            for call in group_calls
            if isinstance(call["completed_at"], (int, float))
            and isinstance(call["started_at"], (int, float))
            and call["completed_at"] >= call["started_at"]
        )
        payload_toks = [call.get("payload_tokens", 0) for call in group_calls if call.get("payload_tokens", 0) > 0]
        group_totals[group] = {
            "calls": len(group_calls),
            "successes": sum(call["success"] is True for call in group_calls),
            "failures": sum(call["success"] is False for call in group_calls),
            "incomplete": sum(call["success"] is None for call in group_calls),
            **outcome_counts(group_calls),
            "sessions": len({call["channel"] for call in group_calls}),
            "duration_samples": len(durations),
            "median_duration_ms": round(median(durations), 3) if durations else None,
            "p95_duration_ms": (
                round(durations[max(0, ceil(len(durations) * 0.95) - 1)], 3)
                if durations
                else None
            ),
            "payload_tokens": sum(payload_toks),
            "avg_payload_tokens": round(sum(payload_toks) / len(payload_toks), 1) if payload_toks else 0,
            "offloaded_calls": sum(1 for call in group_calls if call.get("offloaded")),
            "estimated_tokens_saved": sum(call.get("estimated_tokens_saved", 0) for call in group_calls),
        }
    report = {
        "schema": 4,
        "hours": hours,
        "trace_files": len(paths),
        "skipped_rotated_files": skipped,
        "chats_with_events": len(sessions),
        "tool_calls": len(calls),
        "groups": grouped,
        "group_totals": group_totals,
        "catalog_mentions": dict(catalog_mentions),
        "catalog_exposure": {
            "verified": False,
            "reason": "Name mentions are not proof of model-visible tool availability or picker enablement.",
        },
        "measurement_notes": [
            "Useful means a nonempty recognized result, not semantic relevance or task success.",
            "Missing, truncated, or unrecognized result payloads remain unmeasured, never counted as hits.",
            "Tool discovery, status, sync, and workspace-list calls are not source lookup hits.",
            "CACHE_MISS stays a failed tool call but is counted separately, never as useful/empty source or a daemon outage. Permitted host fallback after a cache miss is legitimate; coverage eligibility alone does not prove a warm cache.",
        ],
    }
    lookup_calls = [
        call for call in calls.values()
        if is_lookup_tool(call["tool"])
    ]
    cctx_calls = [call for call in lookup_calls if classify(call["tool"]) in {"native", "mcp"}]
    builtin_lookup_calls = [call for call in lookup_calls if classify(call["tool"]) == "builtin_search_read"]
    report["lookup_adoption"] = {
        "lookup_calls": len(lookup_calls),
        "code_context_calls": len(cctx_calls),
        "selection_percent": round(100 * len(cctx_calls) / len(lookup_calls), 2) if lookup_calls else None,
        **outcome_counts(cctx_calls),
    }

    # Build per-model usage & token accounting
    model_ids = sorted(
        {rec["model"] for rec in usage_records if rec.get("model")}
        | {call["model"] for call in calls.values() if call.get("model")}
    )
    model_usage = {}
    for model_id in model_ids:
        m_records = [rec for rec in usage_records if rec.get("model") == model_id]
        m_calls = [call for call in calls.values() if call.get("model") == model_id]
        m_lookup = [call for call in m_calls if is_lookup_tool(call["tool"])]
        m_cctx = [call for call in m_lookup if classify(call["tool"]) in {"native", "mcp"}]
        m_builtin = [call for call in m_lookup if classify(call["tool"]) == "builtin_search_read"]
        m_outcomes = outcome_counts(m_cctx)
        m_warm_hits = max(0, len(m_cctx) - m_outcomes["expected_cache_misses"] - m_outcomes["cache_load_failures"])
        display_name = next((rec["display_name"] for rec in m_records if rec.get("display_name")), model_id)
        efforts = Counter(rec["reasoning_effort"] for rec in m_records if rec.get("reasoning_effort") and rec["reasoning_effort"] != "-")
        in_tok = sum(rec["input_tokens"] for rec in m_records)
        cache_read = sum(rec["cache_read_tokens"] for rec in m_records)
        cache_write = sum(rec["cache_write_tokens"] for rec in m_records)
        out_tok = sum(rec["output_tokens"] for rec in m_records)
        rsn_tok = sum(rec["reasoning_tokens"] for rec in m_records)
        nano_aiu = sum(rec["nano_aiu"] for rec in m_records)
        turn_durs = sorted(rec["duration_ms"] for rec in m_records if rec["duration_ms"] > 0)
        ttfts = sorted(rec["ttft_ms"] for rec in m_records if rec["ttft_ms"] > 0)
        direct_saved = sum(call.get("estimated_tokens_saved", 0) for call in m_cctx)
        turn_comp = sum(1 for call in m_cctx if call.get("success") is True and call.get("turn_compressed"))
        m_avoided_turns = sum(avoided_turns_for_call(call["tool"], bool(call.get("turn_compressed")), call.get("success")) for call in m_cctx)
        turn_saved = int(round(m_avoided_turns * 220_000))
        m_compounded_saved = direct_saved * 35
        used_aic = round(nano_aiu / 1e9, 4) if nano_aiu else 0.0
        saved_aic = round((direct_saved * 0.00055) + (m_avoided_turns * 4.20), 4)
        m_sessions = {rec["channel"] for rec in m_records} | {call["channel"] for call in m_calls}
        m_eligible_lookup = [
            call for call in m_lookup
            if not call.get("non_source_host_only") and not call.get("pre_edit_view")
        ]
        model_usage[model_id] = {
            "model": model_id,
            "display_name": display_name,
            "primary_effort": efforts.most_common(1)[0][0] if efforts else "-",
            "reasoning_efforts": dict(efforts),
            "sessions": len(m_sessions),
            "turns": len(m_records),
            "input_tokens": in_tok,
            "cache_read_tokens": cache_read,
            "cache_write_tokens": cache_write,
            "uncached_input_tokens": max(0, in_tok - cache_read),
            "prompt_cache_hit_percent": round(100 * cache_read / in_tok, 2) if in_tok else None,
            "output_tokens": out_tok,
            "reasoning_tokens": rsn_tok,
            "total_tokens": in_tok + out_tok + rsn_tok,
            "total_aiu": used_aic,
            "used_aic": used_aic,
            "used_usd_0001": round(used_aic * 0.001, 4),
            "used_usd_api_001": round(used_aic * 0.01, 4),
            "median_turn_ms": round(median(turn_durs), 1) if turn_durs else None,
            "median_ttft_ms": round(median(ttfts), 1) if ttfts else None,
            "cctx_calls": len(m_cctx),
            "cctx_useful_hits": m_outcomes["useful_results"],
            "cctx_expected_cache_misses": m_outcomes["expected_cache_misses"],
            "cctx_cache_hit_percent": round(100 * m_warm_hits / len(m_cctx), 2) if m_cctx else None,
            "builtin_calls": len(m_builtin),
            "cctx_adoption_percent": round(100 * len(m_cctx) / len(m_eligible_lookup), 2) if m_eligible_lookup else None,
            "cctx_payload_tokens": sum(call.get("payload_tokens", 0) for call in m_cctx),
            "builtin_payload_tokens": sum(call.get("payload_tokens", 0) for call in m_builtin),
            "turn_compression_calls": turn_comp,
            "direct_tokens_saved": direct_saved,
            "compounded_history_tokens_saved": m_compounded_saved,
            "estimated_turn_tokens_saved": turn_saved,
            "estimated_total_tokens_saved": direct_saved + m_compounded_saved + turn_saved,
            "saved_aic": saved_aic,
            "saved_usd_0001": round(saved_aic * 0.001, 4),
            "saved_usd_api_001": round(saved_aic * 0.01, 4),
        }
    report["model_usage"] = dict(
        sorted(model_usage.items(), key=lambda item: (item[1]["turns"], item[1]["cctx_calls"], item[1]["total_tokens"]), reverse=True)
    )

    total_in = sum(rec["input_tokens"] for rec in usage_records)
    total_cache_read = sum(rec["cache_read_tokens"] for rec in usage_records)
    total_cache_write = sum(rec["cache_write_tokens"] for rec in usage_records)
    total_out = sum(rec["output_tokens"] for rec in usage_records)
    total_rsn = sum(rec["reasoning_tokens"] for rec in usage_records)
    total_nano_aiu = sum(rec["nano_aiu"] for rec in usage_records)
    total_used_aic = round(total_nano_aiu / 1e9, 4) if total_nano_aiu else 0.0
    cctx_payload = sum(call.get("payload_tokens", 0) for call in cctx_calls)
    builtin_payload = sum(call.get("payload_tokens", 0) for call in builtin_lookup_calls)
    direct_saved_all = sum(call.get("estimated_tokens_saved", 0) for call in cctx_calls)
    compounded_saved_all = direct_saved_all * 35
    turn_comp_all = sum(1 for call in cctx_calls if call.get("success") is True and call.get("turn_compressed"))
    avoided_turns_all = sum(avoided_turns_for_call(call["tool"], bool(call.get("turn_compressed")), call.get("success")) for call in cctx_calls)
    turn_saved_all = int(round(avoided_turns_all * 220_000))
    total_saved_aic = round((direct_saved_all * 0.00055) + (avoided_turns_all * 4.20), 4)
    cctx_outcomes_all = outcome_counts(cctx_calls)
    cctx_warm_all = max(0, len(cctx_calls) - cctx_outcomes_all["expected_cache_misses"] - cctx_outcomes_all["cache_load_failures"])
    eligible_lookup_all = [
        call for call in lookup_calls
        if not call.get("non_source_host_only") and not call.get("pre_edit_view")
    ]
    report["token_summary"] = {
        "total_turns": len(usage_records),
        "models_used": [info["display_name"] for info in report["model_usage"].values()],
        "input_tokens": total_in,
        "cache_read_tokens": total_cache_read,
        "cache_write_tokens": total_cache_write,
        "uncached_input_tokens": max(0, total_in - total_cache_read),
        "prompt_cache_hit_percent": round(100 * total_cache_read / total_in, 2) if total_in else None,
        "output_tokens": total_out,
        "reasoning_tokens": total_rsn,
        "total_llm_tokens": total_in + total_out + total_rsn,
        "used_aic": total_used_aic,
        "used_usd_0001": round(total_used_aic * 0.001, 4),
        "used_usd_api_001": round(total_used_aic * 0.01, 4),
        "cctx_warm_cache_hit_percent": round(100 * cctx_warm_all / len(cctx_calls), 2) if cctx_calls else None,
        "eligible_source_adoption_percent": round(100 * len(cctx_calls) / len(eligible_lookup_all), 2) if eligible_lookup_all else None,
        "excluded_non_source_calls": sum(1 for call in lookup_calls if call.get("non_source_host_only")),
        "excluded_pre_edit_views": sum(1 for call in lookup_calls if call.get("pre_edit_view")),
        "cctx_payload_tokens": cctx_payload,
        "cctx_avg_payload_tokens": round(cctx_payload / len(cctx_calls), 1) if cctx_calls else 0,
        "cctx_offloaded_calls": sum(1 for call in cctx_calls if call.get("offloaded")),
        "builtin_payload_tokens": builtin_payload,
        "builtin_avg_payload_tokens": round(builtin_payload / len(builtin_lookup_calls), 1) if builtin_lookup_calls else 0,
        "builtin_offloaded_calls": sum(1 for call in builtin_lookup_calls if call.get("offloaded")),
        "turn_compression_calls": turn_comp_all,
        "direct_tokens_saved": direct_saved_all,
        "compounded_history_tokens_saved": compounded_saved_all,
        "estimated_turn_tokens_saved": turn_saved_all,
        "estimated_total_tokens_saved": direct_saved_all + compounded_saved_all + turn_saved_all,
        "saved_aic": total_saved_aic,
        "saved_usd_0001": round(total_saved_aic * 0.001, 4),
        "saved_usd_api_001": round(total_saved_aic * 0.01, 4),
    }

    failure_breakdown = Counter()
    subagent_failure_breakdown = Counter()
    for call in cctx_calls:
        if call.get("success") is False:
            err = json.dumps(call.get("error"), ensure_ascii=False)
            if "CACHE_MISS root=" in err:
                bucket = "cache_miss"
            elif "CACHE_LOAD_FAILED root=" in err:
                bucket = "cache_load_failed"
            elif "explicit absolute Linux root" in err or "not inside a registered workspace" in err or "not explicitly registered" in err:
                bucket = "missing_or_unregistered_root"
            elif "READ_POLICY_DENIED" in err or "TARGET_NOT_INDEXED" in err or "only non-hidden" in err:
                bucket = "policy_or_hidden_target"
            elif "READ_RANGE_" in err or "radius 0.." in err or "exceeds snapshot EOF" in err or "maximum 500 lines" in err:
                bucket = "range_or_limit_bounds"
            elif "RESPONSE_BUDGET_EXCEEDED" in err:
                bucket = "response_budget_exceeded"
            elif "READ_UNAVAILABLE" in err or "unavailable after automatic recovery" in err:
                bucket = "file_unavailable"
            elif "No matching client connected" in err or "disconnected before completing" in err:
                bucket = "client_disconnected"
            else:
                bucket = "other_error"
            failure_breakdown[bucket] += 1
            if call.get("is_subagent"):
                subagent_failure_breakdown[bucket] += 1
    report["cctx_failure_breakdown"] = dict(failure_breakdown)
    report["subagent_cctx_failure_breakdown"] = dict(subagent_failure_breakdown)
    tool_search_calls = [call for call in calls.values() if call.get("tool") == "tool_search_tool"]
    report["tool_search_diagnostics"] = {
        "total_calls": len(tool_search_calls),
        "subagent_calls": sum(1 for c in tool_search_calls if c.get("is_subagent")),
        "cctx_discoveries": sum(1 for c in tool_search_calls if c.get("tool_search_cctx_hit")),
        "no_client_connected_misses": sum(1 for c in tool_search_calls if c.get("tool_search_no_client_miss")),
        "cctx_cli_fallback_calls": sum(1 for c in calls.values() if c.get("cctx_cli_fallback")),
    }
    role_names = sorted(
        {rec.get("agent_role", "main") for rec in usage_records}
        | {call.get("agent_role", "main") for call in calls.values()}
    )
    subagent_usage = {}
    for role in role_names:
        r_records = [rec for rec in usage_records if rec.get("agent_role", "main") == role]
        r_calls = [call for call in calls.values() if call.get("agent_role", "main") == role]
        r_lookup = [call for call in r_calls if is_lookup_tool(call["tool"])]
        r_elig = [
            call for call in r_lookup
            if not call.get("non_source_host_only") and not call.get("pre_edit_view")
        ]
        r_cctx = [call for call in r_lookup if classify(call["tool"]) in {"native", "mcp"}]
        r_builtin = [call for call in r_lookup if classify(call["tool"]) == "builtin_search_read"]
        r_outcomes = outcome_counts(r_cctx)
        r_warm = max(0, len(r_cctx) - r_outcomes["expected_cache_misses"] - r_outcomes["cache_load_failures"])
        r_direct_saved = sum(call.get("estimated_tokens_saved", 0) for call in r_cctx)
        r_avoided = sum(avoided_turns_for_call(call["tool"], bool(call.get("turn_compressed")), call.get("success")) for call in r_cctx)
        r_nano = sum(rec.get("nano_aiu", 0) for rec in r_records)
        r_used_aic = round(r_nano / 1e9, 4) if r_nano else 0.0
        r_saved_aic = round((r_direct_saved * 0.00055) + (r_avoided * 4.20), 4)
        subagent_usage[role] = {
            "agent_role": role,
            "is_subagent": role != "main",
            "turns": len(r_records),
            "tool_calls": len(r_calls),
            "lookup_calls": len(r_lookup),
            "eligible_lookup_calls": len(r_elig),
            "cctx_calls": len(r_cctx),
            "builtin_calls": len(r_builtin),
            "cctx_adoption_percent": round(100 * len(r_cctx) / len(r_elig), 2) if r_elig else None,
            "cctx_cache_hit_percent": round(100 * r_warm / len(r_cctx), 2) if r_cctx else None,
            "used_aic": r_used_aic,
            "saved_aic": r_saved_aic,
        }
    report["subagent_usage"] = dict(
        sorted(
            subagent_usage.items(),
            key=lambda kv: (0 if kv[0] == "main" else 1, -kv[1]["lookup_calls"], -kv[1]["turns"]),
        )
    )
    report["schema"] = 5
    all_channels = sorted({call["channel"] for call in calls.values()} | {rec["channel"] for rec in usage_records})
    report["per_session"] = {
        channel: session_adoption(
            [call for call in calls.values() if call["channel"] == channel],
            sample_limit,
            [rec for rec in usage_records if rec["channel"] == channel],
        )
        for channel in all_channels
    }
    report["coverage_timestamp"] = coverage.get("generated_at_unix") if coverage else None
    report["routing_policy"] = {
        "eligible": "Use native context/read/files first; discover MCP when native is absent.",
        "legitimate_fallback": "Use host policy-aware tools; do not retry cctx or widen access automatically.",
        "unknown": "Keep unknown out of eligible-use percentages; inspect corpus/ranges before routing.",
        "shell": "Never execute or rewrite shell text to estimate adoption.",
    }
    return report


def positive_hours(value: str) -> float:
    hours = float(value)
    if hours <= 0:
        raise argparse.ArgumentTypeError("hours must be greater than zero")
    return hours


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(descriptor, "w") as stream:
            stream.write(content)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def _fmt_tok(value: int | float | None) -> str:
    if value is None:
        return "-"
    num = float(value)
    if abs(num) >= 1_000_000_000:
        return f"{num / 1_000_000_000:.2f}B"
    if abs(num) >= 1_000_000:
        return f"{num / 1_000_000:.2f}M"
    if abs(num) >= 1_000:
        return f"{num / 1_000:.1f}K"
    return f"{int(round(num)):,}"


def _fmt_pct(value: float | None) -> str:
    return f"{value:.1f}%" if isinstance(value, (int, float)) else "-"


def _bar(percent: float | None, width: int = 8) -> str:
    if not isinstance(percent, (int, float)):
        return "░" * width + "   -  "
    clamped = max(0.0, min(100.0, float(percent)))
    filled = int(round((clamped / 100.0) * width))
    return ("█" * filled + "░" * (width - filled)) + f" {clamped:5.1f}%"


def _box_table(title: str, headers: list[str], rows: list[list[str]], aligns: list[str] | None = None) -> str:
    if not rows:
        rows = [["(none)"] + ["-"] * (len(headers) - 1)]
    aligns = aligns or (["<"] + [">"] * (len(headers) - 1))
    widths = [
        max(len(str(headers[i])), *(len(str(row[i])) for row in rows))
        for i in range(len(headers))
    ]

    def fmt_cell(text: str, width: int, align: str) -> str:
        return f"{text:>{width}}" if align == ">" else f"{text:<{width}}"

    top = "┌─" + "─┬─".join("─" * w for w in widths) + "─┐"
    mid = "├─" + "─┼─".join("─" * w for w in widths) + "─┤"
    bot = "└─" + "─┴─".join("─" * w for w in widths) + "─┘"
    head_line = "│ " + " │ ".join(fmt_cell(str(headers[i]), widths[i], aligns[i]) for i in range(len(headers))) + " │"
    body_lines = [
        "│ " + " │ ".join(fmt_cell(str(row[i]), widths[i], aligns[i]) for i in range(len(headers))) + " │"
        for row in rows
    ]
    return "\n".join([f"  {title}", top, head_line, mid, *body_lines, bot])


def format_pretty_report(report: dict) -> str:
    ts = report.get("token_summary", {})
    adopt = report.get("lookup_adoption", {})
    hours = report.get("hours", 0)
    used_aic = ts.get("used_aic", 0.0)
    saved_aic = ts.get("saved_aic", 0.0)
    cctx_cnt = adopt.get("code_context_calls", 0)
    warm_pct = ts.get("cctx_warm_cache_hit_percent")
    elig_pct = ts.get("eligible_source_adoption_percent") or adopt.get("selection_percent")
    def visual_width(s: str) -> int:
        return sum(2 if (c in "⚡💰💎🔥🎯🚀" or ord(c) >= 0x1F300) else 1 for c in s)

    title_row = f"⚡ CODE CONTEXT (cctx) — MODEL, TOKEN & COST SAVINGS DASHBOARD (Window: {hours:.1f}h)"
    hero_rows = [
        f"💰 COST SAVED (@$0.001/AIC) :  +${saved_aic * 0.001:<8.2f} USD   (~{saved_aic:,.1f} AIC saved across {cctx_cnt:,} CCTX lookups)",
        f"💎 API LIST VALUE (@$0.01)  :  +${saved_aic * 0.01:<8.2f} USD   (Frontier API list price: $2/$0.10/$10 per 1M)",
        (
            f"🔥 TOKENS SAVED             :  ~{_fmt_tok(ts.get('estimated_total_tokens_saved', 0)):<9} tok   "
            f"(Direct Payload ~{_fmt_tok(ts.get('direct_tokens_saved', 0))} + {ts.get('turn_compression_calls', 0):,}x Turn-Comp ~{_fmt_tok(ts.get('estimated_turn_tokens_saved', 0))})"
        ),
        f"🎯 WARM CACHE HIT RATE      :  {_bar(warm_pct, 10)}  ({cctx_cnt:,} CCTX lookups)",
        (
            f"🚀 ELIGIBLE SOURCE ADOPTION :  {_bar(elig_pct, 10)}  "
            f"(excl. {ts.get('excluded_non_source_calls', 0):,} /tmp & {ts.get('excluded_pre_edit_views', 0):,} pre-edit)"
        ),
    ]
    meta_rows = [
        f"• Total LLM Spend : ${used_aic * 0.001:,.2f} (@$0.001/AIC · {used_aic:,.1f} AIC)  |  ${used_aic * 0.01:,.2f} (API List @$0.01/AIC)",
        (
            f"• Total LLM Tokens: {_fmt_tok(ts.get('total_llm_tokens', 0))} across {report.get('chats_with_events', 0)} sessions / "
            f"{ts.get('total_turns', 0):,} turns (In {_fmt_tok(ts.get('input_tokens', 0))} · Cache {_fmt_pct(ts.get('prompt_cache_hit_percent'))} · "
            f"Out {_fmt_tok(ts.get('output_tokens', 0))} · Rsn {_fmt_tok(ts.get('reasoning_tokens', 0))})"
        ),
    ]
    box_width = max(92, visual_width(title_row), *(visual_width(r) for r in hero_rows), *(visual_width(r) for r in meta_rows))

    def frame_row(text: str) -> str:
        pad = max(0, box_width - visual_width(text))
        return f"║ {text}{' ' * pad} ║"

    top_b = "╔" + "═" * (box_width + 2) + "╗"
    mid_b = "╠" + "═" * (box_width + 2) + "╣"
    bot_b = "╚" + "═" * (box_width + 2) + "╝"

    lines = [
        top_b,
        frame_row(title_row),
        mid_b,
        *(frame_row(r) for r in hero_rows),
        mid_b,
        *(frame_row(r) for r in meta_rows),
        bot_b,
        "",
    ]

    model_rows = []
    for info in report.get("model_usage", {}).values():
        u_aic = info.get("used_aic", 0.0)
        s_aic = info.get("saved_aic", 0.0)
        model_rows.append([
            f"{info['display_name']} ({info['primary_effort']})",
            f"{info['sessions']}/{info['turns']}",
            f"{_fmt_tok(info['input_tokens'])} ({_fmt_pct(info['prompt_cache_hit_percent'])})",
            f"{_fmt_tok(info['output_tokens'])}+{_fmt_tok(info['reasoning_tokens'])}",
            f"{info['cctx_calls']:,} ({_fmt_pct(info['cctx_cache_hit_percent'])})",
            _fmt_pct(info["cctx_adoption_percent"]),
            f"${u_aic * 0.001:,.2f} ({u_aic:,.1f} AIC)",
            f"+${s_aic * 0.001:,.2f} / API +${s_aic * 0.01:,.2f} (~{s_aic:,.1f} AIC)",
        ])
    model_rows.append([
        "TOTAL (All Models)",
        f"{report.get('chats_with_events', 0)}/{ts.get('total_turns', 0)}",
        f"{_fmt_tok(ts.get('input_tokens', 0))} ({_fmt_pct(ts.get('prompt_cache_hit_percent'))})",
        f"{_fmt_tok(ts.get('output_tokens', 0))}+{_fmt_tok(ts.get('reasoning_tokens', 0))}",
        f"{cctx_cnt:,} ({_fmt_pct(warm_pct)})",
        _fmt_pct(elig_pct),
        f"${used_aic * 0.001:,.2f} ({used_aic:,.1f} AIC)",
        f"+${saved_aic * 0.001:,.2f} / API +${saved_aic * 0.01:,.2f} (~{saved_aic:,.1f} AIC)",
    ])
    lines.append(
        _box_table(
            "1. LLM MODEL USAGE, CACHE HIT & COST SAVINGS",
            ["Model (Effort)", "Sess/Turns", "Input (Cache%)", "Out+Rsn Tok", "CCTX (Warm%)", "Src Adopt", "LLM Spend (@$0.001)", "COST SAVED (@$0.001 / API @$0.01)"],
            model_rows,
            ["<", ">", ">", ">", ">", ">", ">", ">"],
        )
    )
    lines.append("")

    group_labels = {
        "native": "Native (#cctxNative*)",
        "mcp": "Shared MCP (code_*)",
        "builtin_search_read": "Built-in (rg/view/glob)",
        "other": "Other Agent Tools",
    }
    group_rows = []
    for group_key in ("native", "mcp", "builtin_search_read", "other"):
        g = report.get("group_totals", {}).get(group_key)
        if not g:
            continue
        calls_cnt = g.get("calls", 0)
        warm_hits = max(0, calls_cnt - g.get("expected_cache_misses", 0) - g.get("cache_load_failures", 0))
        g_warm_pct = round(100 * warm_hits / calls_cnt, 1) if calls_cnt and group_key in {"native", "mcp"} else None
        g_saved_tok = g.get("estimated_tokens_saved", 0)
        g_saved_aic = g_saved_tok * 0.0004
        group_rows.append([
            group_labels.get(group_key, group_key),
            f"{calls_cnt:,}",
            f"{g.get('useful_results', 0):,}",
            _fmt_pct(g_warm_pct),
            f"{g['median_duration_ms']:.1f}ms" if g.get("median_duration_ms") is not None else "-",
            f"{g['p95_duration_ms']:.1f}ms" if g.get("p95_duration_ms") is not None else "-",
            f"{g.get('avg_payload_tokens', 0):.0f} tok",
            f"{g.get('offloaded_calls', 0):,}",
            f"+${g_saved_aic * 0.001:,.2f} / API +${g_saved_aic * 0.01:,.2f} (~{g_saved_aic:,.1f} AIC)" if group_key in {"native", "mcp"} else "-",
        ])
    lines.append(
        _box_table(
            "2. TOOL SURFACE PERFORMANCE & COST SAVINGS",
            ["Surface", "Calls", "Useful", "Warm Hit%", "Median", "P95", "Avg Payload", ">8KiB Offload", "DIRECT COST SAVED (@$0.001 / API @$0.01)"],
            group_rows,
            ["<", ">", ">", ">", ">", ">", ">", ">", ">"],
        )
    )
    lines.append("")

    sub_usage = report.get("subagent_usage", {})
    if len(sub_usage) > 1 or any(k != "main" for k in sub_usage):
        role_rows = []
        for role, info in sub_usage.items():
            u_aic = info.get("used_aic", 0.0)
            s_aic = info.get("saved_aic", 0.0)
            label = "main (foreground)" if role == "main" else f"subagent:{role}"
            role_rows.append([
                label,
                f"{info.get('turns', 0):,}",
                f"{info.get('eligible_lookup_calls', 0):,}",
                f"{info.get('cctx_calls', 0):,}",
                f"{info.get('builtin_calls', 0):,}",
                _bar(info.get("cctx_adoption_percent"), 8),
                _fmt_pct(info.get("cctx_cache_hit_percent")),
                f"${u_aic * 0.001:,.2f} ({u_aic:,.1f} AIC)",
                f"+${s_aic * 0.001:,.2f} / API +${s_aic * 0.01:,.2f}",
            ])
        lines.append(
            _box_table(
                "2b. MAIN vs SUBAGENT ADOPTION & COST SAVINGS",
                ["Agent Role", "Turns", "Elig Lookups", "CCTX", "Built-in", "Src Adopt", "Warm Hit%", "LLM Spend (@$0.001)", "COST SAVED (@$0.001 / API @$0.01)"],
                role_rows,
                ["<", ">", ">", ">", ">", "<", ">", ">", ">"],
            )
        )
        lines.append("")

    session_items = sorted(
        report.get("per_session", {}).items(),
        key=lambda kv: (kv[1].get("turns", 0), kv[1].get("lookup_calls", 0)),
        reverse=True,
    )[:12]
    session_rows = []
    for ch, s in session_items:
        short_id = ch.removeprefix("session:").removeprefix("ahp-chat:")[:12]
        ws = str(s.get("workspace") or "-")[:20]
        mod = str(s.get("primary_model") or "-")[:20]
        u_aic = s.get("used_aic", 0.0)
        s_aic = s.get("saved_aic", 0.0)
        session_rows.append([
            short_id,
            ws,
            mod,
            f"{s.get('turns', 0):,}",
            f"${u_aic * 0.001:,.2f} ({u_aic:,.1f} AIC)",
            _bar(s.get("cctx_cache_hit_percent"), 8),
            f"{_fmt_pct(s.get('cctx_selection_percent'))} ({s.get('cctx_calls', 0)}/{s.get('eligible_lookup_calls', s.get('lookup_calls', 0))})",
            f"+${s_aic * 0.001:,.2f} / API +${s_aic * 0.01:,.2f} (~{s_aic:,.1f} AIC)",
        ])
    lines.append(
        _box_table(
            "3. TOP SESSIONS BY LLM SPEND, CACHE HIT & COST SAVINGS (Top 12)",
            ["Session", "Workspace", "Primary Model", "Turns", "LLM Spend (@$0.001)", "Warm Hit Rate", "Src Adopt (CCTX/Elig)", "COST SAVED (@$0.001 / API @$0.01)"],
            session_rows,
            ["<", "<", "<", ">", ">", "<", ">", ">"],
        )
    )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hours", type=positive_hours, default=4.0)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--pretty", action="store_true", help="Render a visual model & token usage dashboard table.")
    parser.add_argument("--source", choices=("session", "wire"), default="session")
    parser.add_argument("--coverage", type=Path)
    parser.add_argument("--session", action="append", default=[])
    parser.add_argument("--sample-limit", type=int, default=20)
    parser.add_argument("--after", help="Inclusive ISO-8601 timestamp for a post-deployment observation window.")
    args = parser.parse_args()
    if not args.output and not args.pretty:
        parser.error("--output is required unless --pretty is specified")
    if not 1 <= args.sample_limit <= 1000:
        parser.error("--sample-limit must be 1..1000")
    coverage = json.loads(args.coverage.read_text()) if args.coverage else None
    if coverage is not None and not isinstance(coverage, dict):
        parser.error("--coverage must contain an object")
    now = time.time()
    hours = args.hours
    if args.after:
        try:
            after = datetime.fromisoformat(args.after.replace("Z", "+00:00"))
        except ValueError:
            parser.error("--after must be a valid ISO-8601 timestamp")
        if after.tzinfo is None:
            parser.error("--after must include a timezone")
        if after.timestamp() > now:
            parser.error("--after must not be in the future")
        hours = (now - after.timestamp()) / 3600
    cutoff = now - hours * 3600
    paths, skipped = (
        recent_session_paths(Path.home(), cutoff)
        if args.source == "session"
        else recent_trace_paths(Path.home(), cutoff)
    )
    if args.session:
        paths = [path for path in paths if path.parent.name in args.session]
    report = build_report(paths, hours, now=now, coverage=coverage, sample_limit=args.sample_limit)
    report["observation_after"] = args.after
    report["skipped_rotated_files"] += skipped
    encoded = json.dumps(report, indent=2)
    if args.output:
        atomic_write(args.output, encoded)
    if args.pretty:
        print(format_pretty_report(report))
    else:
        print(encoded)


if __name__ == "__main__":
    main()
