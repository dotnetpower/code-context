#!/usr/bin/env python3
"""Stable persistent-MCP hot-path benchmark for optimization rounds."""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import time


QUERIES = [
    "get_azure_bearer_token_provider",
    "class AutonomousDialogueEngine",
    "def strip_particles(",
    "AZURE_OPENAI_DEPLOYMENT_NAME",
    "__absent_e49a__",
    "def generate_bori_reply(",
]


def percentile(values, percentage):
    return values[(len(values) * percentage + 99) // 100 - 1]


def summarize(values):
    values = sorted(values)
    return {
        "samples": len(values),
        "mean_ms": statistics.mean(values),
        "median_ms": statistics.median(values),
        "p95_ms": percentile(values, 95),
        "p99_ms": percentile(values, 99),
        "max_ms": max(values),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--blocks", type=int, default=5)
    parser.add_argument("--rounds", type=int, default=100)
    parser.add_argument("--tool", choices=["code_grep", "code_context"], default="code_grep")
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output exists")
    process = subprocess.Popen(
        [str(Path.home() / ".local/bin/cctx"), "shared-mcp"],
        cwd="/home/moonchoi/dev/agent-san",
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    request_id = 0

    def rpc(method, params):
        nonlocal request_id
        request_id += 1
        process.stdin.write(
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
            )
            + "\n"
        )
        process.stdin.flush()
        response = json.loads(process.stdout.readline())
        if "error" in response:
            raise RuntimeError(response)
        return response

    started = time.perf_counter_ns()
    rpc("initialize", {"protocolVersion": "2025-06-18"})
    rpc("tools/list", {})
    startup_ms = (time.perf_counter_ns() - started) / 1e6
    blocks = []
    try:
        for _ in range(args.blocks):
            values = []
            for _ in range(args.rounds):
                for query in QUERIES:
                    started = time.perf_counter_ns()
                    response = rpc(
                        "tools/call",
                        {
                            "name": args.tool,
                            "arguments": {
                                "pattern": query,
                                "limit": 20,
                                **({"radius": 3} if args.tool == "code_context" else {}),
                            },
                        },
                    )
                    values.append((time.perf_counter_ns() - started) / 1e6)
                    if response["result"]["isError"]:
                        raise RuntimeError(response)
            blocks.append(summarize(values))
    finally:
        process.stdin.close()
        process.wait(timeout=5)
    report = {
        "schema": 1,
        "label": args.label,
        "tool": args.tool,
        "startup_ms": startup_ms,
        "queries": QUERIES,
        "blocks": blocks,
        "summary": {
            "median_block_mean_ms": statistics.median(row["mean_ms"] for row in blocks),
            "median_block_median_ms": statistics.median(
                row["median_ms"] for row in blocks
            ),
            "median_block_p95_ms": statistics.median(row["p95_ms"] for row in blocks),
            "worst_max_ms": max(row["max_ms"] for row in blocks),
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        json.dump(report, output, indent=2)
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
