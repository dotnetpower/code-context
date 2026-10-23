#!/usr/bin/env python3
"""Paired Copilot CLI benchmark: built-in search vs code-context availability."""
import argparse
import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


TASK = """Read only. Use the fastest available source search/read tools, not shell commands.
Answer all four questions about this project. For each answer give the exact function name,
relative file path, 1-based function-start line, and the requested behavior. Be concise.

Q1. Which function prevents selling more inventory than is available, and how is failure represented?
Q2. Which function calculates retry delay, what formula does it use, and what caps the result?
Q3. Which function decides whether a session remains valid at the exact expiration timestamp?
Q4. Which function builds the tenant cache key, and what normalization is applied to the tenant?
"""


EXPECTED = {
    "q1": [r"stock_after_sale", r"src/inventory/ledger_037\.rs", r"checked[_ ]?sub|checked subtraction", r"\bnone\b"],
    "q2": [r"next_retry_delay", r"src/net/retry_211\.rs", r"saturat", r"cap_ms"],
    "q3": [r"valid_session", r"src/auth/session_402\.rs", r"now_ms\s*<\s*expires_at_ms", r"\bfalse\b|\binvalid\b"],
    "q4": [r"tenant_cache_key", r"src/cache/key_777\.rs", r"\btrim", r"to_ascii_lowercase|ascii[^.\n]*(?:lowercase|lowercasing)|lowercases? ascii"],
}


def generate(root: Path, count: int) -> None:
    for directory in ["src/inventory", "src/net", "src/auth", "src/cache", "src/generated"]:
        (root / directory).mkdir(parents=True, exist_ok=True)
    targets = {
        "src/inventory/ledger_037.rs": """pub fn stock_after_sale(available: u32, sold: u32) -> Option<u32> {
    available.checked_sub(sold)
}
""",
        "src/net/retry_211.rs": """pub fn next_retry_delay(base_ms: u64, attempt: u32, cap_ms: u64) -> u64 {
    let multiplier = 1_u64.checked_shl(attempt.min(20)).unwrap_or(u64::MAX);
    base_ms.saturating_mul(multiplier).min(cap_ms)
}
""",
        "src/auth/session_402.rs": """pub fn valid_session(now_ms: u64, expires_at_ms: u64, revoked: bool) -> bool {
    !revoked && now_ms < expires_at_ms
}
""",
        "src/cache/key_777.rs": """pub fn tenant_cache_key(tenant: &str, object_id: u64) -> String {
    format!("{}:{object_id}", tenant.trim().to_ascii_lowercase())
}
""",
    }
    for path, content in targets.items():
        (root / path).write_text(content)
    filler = (
        "// synthetic benchmark source; no private code\n"
        "pub fn generated_{i}(input: u64) -> u64 {{\n"
        "    let retry_delay = input.saturating_add({n});\n"
        "    let available_stock = retry_delay ^ 0x5a5a;\n"
        "    let session_expiry = available_stock.wrapping_mul(3);\n"
        "    session_expiry.rotate_left(({n} % 31) as u32)\n"
        "}}\n"
        "// terms: inventory retry session tenant cache expiration available\n"
    )
    for index in range(count):
        path = root / f"src/generated/module_{index:05}.rs"
        path.write_text(filler.format(i=index, n=index % 997))
    (root / "src/lib.rs").write_text(
        "pub mod inventory {}\npub mod net {}\npub mod auth {}\npub mod cache {}\n"
    )


def parse_events(stdout: bytes) -> dict:
    events = []
    for line in stdout.decode(errors="replace").splitlines():
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    tools = []
    tool_durations = []
    starts = {}
    answer = ""
    model = None
    usage = []
    model_calls = 0
    model_dispatch_ms = 0
    tool_result_bytes = 0
    tool_result_tokens = 0
    for event in events:
        kind = event.get("type")
        data = event.get("data", {})
        if kind == "user.message":
            model = data.get("responsesReasoning", {}).get("model") or model
        elif kind == "model.call_start":
            model_calls += 1
        elif kind == "model.call_finished":
            model_dispatch_ms += data.get("dispatchDurationMs") or 0
        elif kind == "tool.execution_start":
            call_id = data.get("toolCallId")
            starts[call_id] = event.get("timestamp")
            tools.append(data.get("toolName"))
        elif kind == "tool.execution_complete":
            call_id = data.get("toolCallId")
            if starts.get(call_id) and event.get("timestamp"):
                began = datetime.datetime.fromisoformat(starts[call_id].replace("Z", "+00:00"))
                ended = datetime.datetime.fromisoformat(event["timestamp"].replace("Z", "+00:00"))
                tool_durations.append((ended - began).total_seconds() * 1000)
            metrics = data.get("toolTelemetry", {}).get("metrics", {})
            tool_result_bytes += metrics.get("mcp_result_content_bytes") or 0
            tool_result_tokens += metrics.get("result_token_count") or 0
        elif kind == "assistant.message" and data.get("content"):
            answer = data["content"]
        elif kind == "session.usage_checkpoint":
            usage.append(data)
        elif kind == "result":
            answer = data.get("content") or data.get("result") or answer
    return {
        "model": model,
        "tools": tools,
        "tool_calls": len(tools),
        "tool_duration_ms": sum(tool_durations),
        "model_calls": model_calls,
        "model_dispatch_ms": model_dispatch_ms,
        "tool_result_bytes": tool_result_bytes,
        "tool_result_tokens": tool_result_tokens,
        "answer": answer,
        "usage_checkpoints": usage,
        "event_types": sorted({event.get("type") for event in events if event.get("type")}),
    }


def score(answer: str) -> dict:
    normalized = re.sub(r"[`*]", "", answer).lower()
    details = {}
    for question, terms in EXPECTED.items():
        details[question] = {
            "matched": [term for term in terms if re.search(term, normalized, re.IGNORECASE)],
            "required": terms,
        }
        details[question]["correct"] = len(details[question]["matched"]) == len(terms)
    return {
        "questions_correct": sum(value["correct"] for value in details.values()),
        "all_correct": all(value["correct"] for value in details.values()),
        "details": details,
    }


def configure(home: Path, executable: Path, state: Path, enabled: bool, tools=None) -> None:
    home.mkdir(parents=True)
    if enabled:
        config = {
            "mcpServers": {
                "code-context": {
                    "type": "local",
                    "command": str(executable),
                    "args": ["shared-mcp"],
                    "env": {"CCTX_HOME": str(state)},
                    "tools": tools or ["*"],
                }
            }
        }
    else:
        config = {"mcpServers": {}}
    (home / "mcp-config.json").write_text(json.dumps(config))


def run_trial(root: Path, home: Path, state: Path, arm: str, repetition: int) -> dict:
    env = os.environ.copy()
    env["COPILOT_HOME"] = str(home)
    env["CCTX_HOME"] = str(state)
    command = [
        "copilot", "-C", str(root), "-p", TASK,
        "--model", "gpt-6.1-sol",
        "--reasoning-effort", "medium",
        "--allow-all-tools",
        "--disable-builtin-mcps",
        "--no-custom-instructions",
        "--no-ask-user",
        "--output-format", "json",
        "--no-auto-update",
        "--log-dir", str(home / "logs"),
    ]
    started = time.perf_counter()
    try:
        result = subprocess.run(command, env=env, cwd=root, capture_output=True, timeout=180)
        parsed = parse_events(result.stdout)
        parsed.update(
            {
                "arm": arm,
                "repetition": repetition,
                "wall_seconds": time.perf_counter() - started,
                "exit_code": result.returncode,
                "stderr": result.stderr.decode(errors="replace")[:4000],
            }
        )
        parsed["score"] = score(parsed["answer"])
        return parsed
    except subprocess.TimeoutExpired as error:
        return {
            "arm": arm,
            "repetition": repetition,
            "wall_seconds": time.perf_counter() - started,
            "timeout": True,
            "stdout": (error.stdout or b"").decode(errors="replace")[-4000:],
            "stderr": (error.stderr or b"").decode(errors="replace")[-4000:],
            "score": {"questions_correct": 0, "all_correct": False},
        }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--files", type=int, default=6000)
    parser.add_argument("--compare-toolsets", action="store_true")
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output exists")
    if not 2 <= args.repetitions <= 5 or not 1000 <= args.files <= 20000:
        parser.error("repetitions 2..5 and files 1000..20000 required")
    executable = Path(__file__).resolve().parents[1] / "target/release/cctx"
    with tempfile.TemporaryDirectory(prefix="cctx-agent-ab-") as temporary:
        temporary = Path(temporary)
        root = temporary / "project"
        generate(root, args.files)
        state = temporary / "state"
        mcp_home = temporary / "copilot-mcp"
        builtin_home = temporary / "copilot-builtin"
        configure(
            mcp_home,
            executable,
            state,
            True,
            ["code_grep", "code_context", "code_read", "code_sync"]
            if args.compare_toolsets
            else None,
        )
        configure(builtin_home, executable, state, args.compare_toolsets)
        env = os.environ.copy()
        env["CCTX_HOME"] = str(state)
        subprocess.run(
            [
                str(executable), "--root", str(root), "--scope", "src",
                "--glob", "*.rs", "register",
            ],
            env=env,
            check=True,
            capture_output=True,
        )
        daemon = subprocess.Popen(
            [str(executable), "daemon"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        try:
            started = time.monotonic()
            while not (state / "daemon.sock").exists():
                if daemon.poll() is not None:
                    raise RuntimeError(daemon.stderr.read().decode())
                if time.monotonic() - started > 10:
                    raise RuntimeError("daemon readiness timeout")
                time.sleep(0.01)
            # Exclude lazy index construction from both steady-state arms.
            env["COPILOT_HOME"] = str(mcp_home)
            subprocess.run(
                [str(executable), "--root", str(root), "sync"],
                env=env,
                check=True,
                capture_output=True,
                timeout=30,
            )
            trials = []
            for repetition in range(args.repetitions):
                labels = ["full", "core"] if args.compare_toolsets else ["builtin", "mcp"]
                order = labels if repetition % 2 == 0 else list(reversed(labels))
                for arm in order:
                    home = builtin_home if arm in ("builtin", "full") else mcp_home
                    trial = run_trial(root, home, state, arm, repetition)
                    trials.append(trial)
                    print(
                        json.dumps(
                            {
                                "arm": arm,
                                "repetition": repetition,
                                "wall_seconds": trial["wall_seconds"],
                                "tool_calls": trial.get("tool_calls"),
                                "tools": trial.get("tools"),
                                "score": trial["score"],
                            }
                        ),
                        flush=True,
                    )
            report = {
                "schema": 1,
                "corpus": {"generated_files": args.files, "language": "Rust", "private_code": False},
                "task": TASK,
                "expected": EXPECTED,
                "model": "gpt-6.1-sol",
                "reasoning_effort": "medium",
                "repetitions": args.repetitions,
                "design": {
                    "same_prompt": True,
                    "same_model": True,
                    "fresh_session_each_trial": True,
                    "alternating_order": True,
                    "builtin_arm": "no MCP configured; natural built-in read/search tool selection",
                    "mcp_arm": "code-context configured in addition to normal built-in tools; natural selection",
                    "compare_toolsets": args.compare_toolsets,
                    "daemon_warmed": True,
                    "not_measured": ["cold index construction", "editing", "real private repository"],
                },
                "trials": trials,
            }
            args.output.parent.mkdir(parents=True, exist_ok=True)
            with args.output.open("x") as output:
                json.dump(report, output, indent=2)
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()


if __name__ == "__main__":
    main()
