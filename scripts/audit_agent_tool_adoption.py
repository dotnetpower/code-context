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
    if name.startswith("code-context-"):
        return "mcp"
    lowered = name.lower()
    if name in {"rg", "view", "glob", "usages"} or any(
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
    target_root = arguments.get("root") or cwd
    paths = []
    is_read = tool in {"view", "read_file"} or tool.endswith(("NativeRead", "codeRead", "code_read"))
    if is_read:
        if tool in {"view", "read_file"}:
            value = arguments.get("path", arguments.get("filePath"))
            paths = [value] if isinstance(value, str) else []
        else:
            ranges = arguments.get("ranges", [])
            paths = [item["path"] for item in ranges if isinstance(item, dict) and isinstance(item.get("path"), str)]
    else:
        values = arguments.get("paths", arguments.get("path"))
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
            if relative in admitted:
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


def session_adoption(calls: list[dict], sample_limit: int) -> dict:
    lookup = [call for call in calls if is_lookup_tool(call["tool"])]
    reasons = Counter(call["eligibility"]["reason"] for call in lookup)
    eligible = sorted(
        (call for call in lookup if call["eligibility"]["classification"] == "eligible"),
        key=lambda call: call["started_at"],
    )[:sample_limit]
    cctx = [call for call in eligible if classify(call["tool"]) in {"native", "mcp"}]
    return {
        "lookup_calls": len(lookup),
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
    sessions: set[str] = set()
    skipped = 0
    catalog_mentions = Counter()
    for path in paths:
        cwd = None
        try:
            lines = path.open(errors="replace")
        except FileNotFoundError:
            skipped += 1
            continue
        with lines:
            for raw in lines:
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
                timestamp = event_timestamp(event)
                if timestamp is None or timestamp < cutoff or timestamp > now:
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
                        },
                    )
                    tool = action.get("toolName", "unknown")
                    call["tool"] = tool if isinstance(tool, str) else "unknown"
                    call["channel"] = call_channel
                    call["started_at"] = timestamp
                    arguments = action.get("arguments", {})
                    if not isinstance(arguments, dict):
                        arguments = {}
                    call["eligibility"] = request_eligibility(call["tool"], arguments, cwd, coverage, now)
                    call["cwd"] = cwd
                elif action_type == "chat/toolCallComplete":
                    call = calls.setdefault(
                        key,
                        {
                            "tool": "unknown",
                            "channel": call_channel,
                            "started_at": None,
                            "completed_at": None,
                            "success": None,
                        },
                    )
                    call["completed_at"] = timestamp
                    result = action.get("result", {})
                    if not isinstance(result, dict):
                        result = {}
                    success = result.get("success")
                    call["success"] = success if isinstance(success, bool) else None
                    call["useful"] = result_usefulness(result) if success is True else None
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
        grouped.setdefault(group, {})[tool] = {
            "calls": count,
            "successes": success_counts[tool],
            "failures": failure_counts[tool],
            "incomplete": count - success_counts[tool] - failure_counts[tool],
            **outcome_counts([call for call in calls.values() if call["tool"] == tool]),
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
    report["lookup_adoption"] = {
        "lookup_calls": len(lookup_calls),
        "code_context_calls": len(cctx_calls),
        "selection_percent": round(100 * len(cctx_calls) / len(lookup_calls), 2) if lookup_calls else None,
        **outcome_counts(cctx_calls),
    }
    failure_breakdown = Counter()
    for call in cctx_calls:
        if call.get("success") is False:
            err = json.dumps(call.get("error"), ensure_ascii=False)
            if "CACHE_MISS root=" in err:
                failure_breakdown["cache_miss"] += 1
            elif "CACHE_LOAD_FAILED root=" in err:
                failure_breakdown["cache_load_failed"] += 1
            elif "explicit absolute Linux root" in err or "not inside a registered workspace" in err or "not explicitly registered" in err:
                failure_breakdown["missing_or_unregistered_root"] += 1
            elif "READ_POLICY_DENIED" in err or "TARGET_NOT_INDEXED" in err or "only non-hidden" in err:
                failure_breakdown["policy_or_hidden_target"] += 1
            elif "READ_RANGE_" in err or "radius 0.." in err or "exceeds snapshot EOF" in err or "maximum 500 lines" in err:
                failure_breakdown["range_or_limit_bounds"] += 1
            elif "RESPONSE_BUDGET_EXCEEDED" in err:
                failure_breakdown["response_budget_exceeded"] += 1
            elif "READ_UNAVAILABLE" in err or "unavailable after automatic recovery" in err:
                failure_breakdown["file_unavailable"] += 1
            else:
                failure_breakdown["other_error"] += 1
    report["cctx_failure_breakdown"] = dict(failure_breakdown)
    report["schema"] = 5
    report["per_session"] = {
        channel: session_adoption([call for call in calls.values() if call["channel"] == channel], sample_limit)
        for channel in sorted({call["channel"] for call in calls.values()})
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


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--hours", type=positive_hours, default=4.0)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source", choices=("session", "wire"), default="session")
    parser.add_argument("--coverage", type=Path)
    parser.add_argument("--session", action="append", default=[])
    parser.add_argument("--sample-limit", type=int, default=20)
    parser.add_argument("--after", help="Inclusive ISO-8601 timestamp for a post-deployment observation window.")
    args = parser.parse_args()
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
    atomic_write(args.output, encoded)
    print(encoded)


if __name__ == "__main__":
    main()
