#!/usr/bin/env python3
"""Compare regex/case query latency and exact results over daemon IPC."""
import argparse
import json
from pathlib import Path
import socket
import statistics
import subprocess
import time


def rpc(root, name, arguments):
    home = Path.home() / ".local/state/code-context/daemon.sock"
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(30)
        stream.connect(str(home))
        stream.sendall(json.dumps({"root": str(root), "name": name,
                                   "arguments": arguments}).encode() + b"\n")
        response = json.loads(stream.makefile("rb").readline(2 * 1024 * 1024 + 1))
    if "error" in response:
        raise RuntimeError(response["error"])
    return response["result"]["data"]


def oracle(root, case):
    command = ["rg", "--no-config", "--no-hidden", "-g", "!.*",
               "--type-add", "cctx:*.py", "--type-add", "cctx:*.ts",
               "--type-add", "cctx:*.tsx", "--type", "cctx", "--json"]
    if not case.get("regex"):
        command.append("-F")
    if case.get("ignore_case"):
        command.append("-i")
    if case.get("glob"):
        command += ["-g", case["glob"]]
    command += ["--", case["pattern"], *(case.get("paths") or ["src", "api", "frontend", "tests"])]
    result = subprocess.run(command, cwd=root, capture_output=True, timeout=30)
    if result.returncode not in (0, 1) or result.stderr:
        raise RuntimeError(result.stderr.decode())
    matches = []
    for line in result.stdout.splitlines():
        event = json.loads(line)
        if event["type"] == "match":
            data = event["data"]
            matches.append({"path": data["path"]["text"], "line": data["line_number"],
                            "text": data["lines"]["text"].removesuffix("\n")})
    matches.sort(key=lambda item: (item["path"], item["line"], item["text"]))
    return {"matches": matches[:1000], "total_matches": len(matches),
            "truncated": len(matches) > 1000}


def measure(operation):
    started = time.perf_counter_ns()
    result = operation()
    return (time.perf_counter_ns() - started) / 1e6, result


def summarize(values):
    values = sorted(values)
    return {"mean_ms": statistics.mean(values), "p50_ms": values[(len(values)-1)//2],
            "p95_ms": values[(len(values)*95+99)//100-1], "max_ms": max(values)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--cases", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--rounds", type=int, default=20)
    parser.add_argument("--baseline", type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output exists")
    cases = json.loads(args.cases.read_text())
    baseline = json.loads(args.baseline.read_text()) if args.baseline else None
    observations = {}
    for case in cases:
        arguments = {key: value for key, value in case.items() if key != "name"}
        arguments["limit"] = 1000
        expected = oracle(args.root, case)
        cold_profile = rpc(args.root, "code_profile", arguments)
        actual = cold_profile["result"]
        comparable = {key: actual[key] for key in ("matches", "total_matches", "truncated")}
        if comparable != expected:
            raise RuntimeError(("result mismatch", case["name"]))
        warm_profile = rpc(args.root, "code_profile", arguments)
        full_profile = rpc(args.root, "code_profile_full", arguments)
        if full_profile["result"] != cold_profile["result"]:
            raise RuntimeError(("planner/full-scan mismatch", case["name"]))
        rows = {"rg": list(baseline["observations"][case["name"]]["rg"]) if baseline else [],
                "shared": [], "full_scan": []}
        for round_number in range(args.rounds + 1):
            if baseline:
                modes = ("shared", "full_scan") if round_number % 2 == 0 else (
                    "full_scan", "shared")
            else:
                modes = ("rg", "shared", "full_scan") if round_number % 2 == 0 else (
                    "full_scan", "shared", "rg")
            for mode in modes:
                elapsed, _ = measure(
                    (lambda: oracle(args.root, case)) if mode == "rg"
                    else ((lambda: rpc(args.root, "code_grep", arguments)) if mode == "shared"
                          else (lambda: rpc(args.root, "code_profile_full", arguments))))
                if round_number:
                    rows[mode].append(elapsed)
        observations[case["name"]] = {
            "case": case,
            "matches": expected["total_matches"],
            "rg": rows["rg"],
            "shared": rows["shared"],
            "full_scan": rows["full_scan"],
            "cold_profile": cold_profile["profile"],
            "warm_profile": warm_profile["profile"],
            "full_scan_profile": full_profile["profile"],
        }
        print(f"{case['name']}: {expected['total_matches']} matches")
    report = {"schema": 2, "cases": len(cases), "rounds": args.rounds,
              "observations": observations,
              "summaries": {name: {"rg": summarize(row["rg"]), "shared": summarize(row["shared"]),
                                   "full_scan": summarize(row["full_scan"])}
                            for name, row in observations.items()},
              "method": {"warm_cache": True, "same_result_contract": True,
                         "internal_phase_profile": True,
                         "planner": "safe HIR literal candidates + cost gate + bounded compiled cache",
                         "rg_timing_source": str(args.baseline) if baseline else "current run"}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        json.dump(report, output, indent=2)


if __name__ == "__main__":
    main()
