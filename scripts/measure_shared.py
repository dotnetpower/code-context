#!/usr/bin/env python3
"""Measure local shared-daemon paths without changing the source workspace."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time


def run(command, env=None):
    result = subprocess.run(command, env=env, capture_output=True, timeout=90)
    if result.returncode not in (0, 1):
        raise RuntimeError(result.stderr.decode())
    return result.stdout


def rpc(home, root, name, arguments):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(30)
        stream.connect(str(home / "daemon.sock"))
        stream.sendall(json.dumps({"root": str(root) if root else None,
                                   "name": name, "arguments": arguments}).encode() + b"\n")
        with stream.makefile("rb") as output:
            response = json.loads(output.readline(2 * 1024 * 1024 + 1))
    if "error" in response:
        raise RuntimeError(response["error"])
    return response["result"]


def normalize(value):
    if isinstance(value, dict):
        return {k: normalize(v) for k, v in value.items() if k != "consistency"}
    if isinstance(value, list):
        return [normalize(v) for v in value]
    return value


def context_projection(value):
    value = normalize(value)
    if isinstance(value, dict):
        for match in value.get("search", {}).get("matches", []):
            match.pop("text", None)
    return value


def measured(operation):
    start = time.perf_counter_ns()
    value = operation()
    milliseconds = (time.perf_counter_ns() - start) / 1_000_000
    return {"ms": milliseconds, "bytes": len(json.dumps(normalize(value)).encode())}


def summary(samples):
    values = sorted(x["ms"] for x in samples)
    return {"mean_ms": statistics.mean(values), "p50_ms": values[(len(values) - 1) // 2],
            "p95_ms": values[max(0, (len(values) * 95 + 99) // 100 - 1)],
            "max_ms": max(values), "samples": len(values)}


class Mcp:
    def __init__(self, executable, env):
        self.process = subprocess.Popen([str(executable), "shared-mcp"], env=env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE)
        self.index = 0
        self.request("initialize", {"protocolVersion": "2025-06-18",
                                    "capabilities": {}, "clientInfo": {"name": "shared-bench", "version": "1"}})
        self.process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        self.process.stdin.flush()
        tools = self.request("tools/list", {})
        if len(tools["tools"]) != 6:
            raise RuntimeError("unexpected MCP tool count")

    def request(self, method, params):
        self.index += 1
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.index,
                                             "method": method, "params": params}).encode() + b"\n")
        self.process.stdin.flush()
        response = json.loads(self.process.stdout.readline(2 * 1024 * 1024 + 1))
        if response.get("id") != self.index or "error" in response:
            raise RuntimeError(response)
        return response["result"]

    def call(self, root, name, arguments):
        result = self.request("tools/call", {"name": name, "arguments": {"root": str(root), **arguments}})
        if result["isError"]:
            raise RuntimeError(result)
        value = json.loads(result["content"][0]["text"])
        return value.get("data", value) if isinstance(value, dict) else value

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        if self.process.returncode:
            raise RuntimeError(self.process.stderr.read().decode())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=30)
    args = parser.parse_args()
    if not 10 <= args.rounds <= 200:
        parser.error("rounds must be 10..200")
    executable = Path(__file__).resolve().parents[1] / "target/release/cctx"
    queries = json.loads(args.queries.read_text())
    if not queries or len(queries) > 100:
        parser.error("1..100 queries required")
    scopes = ["src", "api", "frontend", "tests"]
    globs = ["*.py", "*.ts", "*.tsx"]
    if args.output.exists():
        parser.error("refusing to overwrite report")
    original = args.root.resolve()
    env = os.environ.copy()
    daemon = None
    mcp = None
    with tempfile.TemporaryDirectory(prefix="cctx-shared-") as temporary:
        temporary = Path(temporary)
        root = temporary / "corpus"
        root.mkdir()
        home = temporary / "state"
        env["CCTX_HOME"] = str(home)
        rg = ["rg", "--no-config", "--no-hidden", "-g", "!.*"]
        for pattern in globs:
            rg += ["--type-add", "cctx:" + pattern]
        rg += ["--type", "cctx"]
        discover = rg.copy()
        for name in [".gitignore", ".ignore"]:
            if (original / name).is_file():
                discover += ["--ignore-file", str(original / name)]
        files = run(discover + ["--files", "--null", "--", *scopes],
                    env=None) if Path.cwd() == original else subprocess.check_output(
                        discover + ["--files", "--null", "--", *scopes], cwd=original)
        paths = sorted(set(Path(p.decode()) for p in files.split(b"\0") if p))
        digest = hashlib.sha256()
        total_bytes = 0
        for path in paths:
            if path.is_absolute() or ".." in path.parts or any(p.startswith(".") for p in path.parts):
                raise RuntimeError("unexpected discovered path")
            source = original / path
            if source.is_symlink():
                raise RuntimeError("symlink unsupported")
            contents = source.read_bytes()
            target = root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(contents)
            name = str(path).encode()
            digest.update(len(name).to_bytes(8, "little"))
            digest.update(name)
            digest.update(len(contents).to_bytes(8, "little"))
            digest.update(contents)
            total_bytes += len(contents)
        for scope in scopes:
            (root / scope).mkdir(exist_ok=True)
        configuration = ["--root", str(root)]
        for scope in scopes:
            configuration += ["--scope", scope]
        for pattern in globs:
            configuration += ["--glob", pattern]
        run([str(executable), *configuration, "register"], env)
        daemon = subprocess.Popen([str(executable), "daemon"], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            start = time.monotonic()
            while not (home / "daemon.sock").exists():
                if daemon.poll() is not None:
                    raise RuntimeError(daemon.stderr.read().decode())
                if time.monotonic() - start > 10:
                    raise RuntimeError("daemon did not become ready")
                time.sleep(0.01)
            start = time.perf_counter_ns()
            initial = rpc(home, root, "code_sync", {})["data"]
            first_load_ms = (time.perf_counter_ns() - start) / 1e6
            mcp = Mcp(executable, env)

            def oracle(query, regex=False, ignore_case=False, paths=None, glob=None):
                options = ["--json"]
                if not regex:
                    options.append("-F")
                if ignore_case:
                    options.append("-i")
                if glob:
                    options += ["-g", glob]
                data = subprocess.run(rg + options + ["--", query, *(paths or scopes)],
                                      cwd=root, capture_output=True, timeout=30)
                if data.returncode not in (0, 1) or data.stderr:
                    raise RuntimeError(data.stderr.decode())
                hits = []
                for line in data.stdout.splitlines():
                    event = json.loads(line)
                    if event["type"] == "match":
                        item = event["data"]
                        hits.append({"path": item["path"]["text"], "line": item["line_number"],
                                     "text": item["lines"]["text"].removesuffix("\n")})
                hits.sort(key=lambda h: (h["path"], h["line"], h["text"]))
                return {"matches": hits[:8], "total_matches": len(hits), "truncated": len(hits) > 8}

            observations = {name: {q: [] for q in queries} for name in
                            ["warm_rg", "daemon_rpc", "shared_mcp", "shared_cli", "shared_context", "live_context"]}
            for query in queries:
                expected = oracle(query)
                actual = rpc(home, root, "code_grep", {"pattern": query, "limit": 8})["data"]
                if expected != normalize(actual):
                    raise RuntimeError(("oracle mismatch", query))
                contextual = mcp.call(root, "code_context", {"pattern": query, "limit": 8})
                direct = rpc(home, root, "code_context", {"pattern": query, "limit": 8})["data"]
                if context_projection(contextual) != context_projection(direct):
                    raise RuntimeError("context mismatch")
                baseline = json.loads(run([str(executable), *configuration, "--backend", "live",
                                           "context", query, "--limit", "8"], env))
                if context_projection(contextual) != context_projection(baseline):
                    raise RuntimeError(("live/shared context mismatch", query))
            names = list(observations)
            for round_number in range(args.rounds + 1):
                for index, query in enumerate(queries):
                    operations = {
                        "warm_rg": lambda: oracle(query),
                        "daemon_rpc": lambda: rpc(home, root, "code_grep", {"pattern": query, "limit": 8})["data"],
                        "shared_mcp": lambda: mcp.call(root, "code_grep", {"pattern": query, "limit": 8}),
                        "shared_cli": lambda: json.loads(run(
                            [str(executable), "--root", str(root), "grep", query, "--limit", "8"], env))["data"],
                        "shared_context": lambda: mcp.call(root, "code_context", {"pattern": query, "limit": 8}),
                        "live_context": lambda: json.loads(run(
                            [str(executable), *configuration, "--backend", "live", "context", query, "--limit", "8"], env)),
                    }
                    for offset in range(len(names)):
                        name = names[(round_number + index + offset) % len(names)]
                        result = measured(operations[name])
                        if round_number:
                            observations[name][query].append(result)
                print(f"shared benchmark {round_number}/{args.rounds}", flush=True)

            feature_samples = {}
            for name, arguments in [
                ("regex", {"pattern": "^async def ", "regex": True, "limit": 8}),
                ("case_folding", {"pattern": "GET_AZURE", "ignore_case": True, "limit": 8}),
                ("path_glob", {"pattern": "get_azure", "paths": ["src/utils"], "glob": "*.py", "limit": 8}),
            ]:
                oracle_arguments = {key: value for key, value in arguments.items() if key not in ("limit", "pattern")}
                expected = oracle(arguments["pattern"], **oracle_arguments)
                actual = mcp.call(root, "code_grep", arguments)
                if expected != normalize(actual):
                    raise RuntimeError(("feature parity mismatch", name))
                feature_samples[name] = {"warm_rg": [], "shared_mcp": []}
                for _ in range(20):
                    feature_samples[name]["warm_rg"].append(measured(
                        lambda: oracle(arguments["pattern"], **oracle_arguments)))
                    feature_samples[name]["shared_mcp"].append(measured(
                        lambda: mcp.call(root, "code_grep", arguments)))
            read_path = next(path for path in paths if (root / path).stat().st_size > 0)
            read_arguments = {"ranges": [{"path": str(read_path), "start": 1, "end": 3}]}
            baseline_read = lambda: json.loads(run([str(executable), *configuration, "--backend", "live",
                                                    "read", str(read_path), "1", "3"], env))
            if normalize(mcp.call(root, "code_read", read_arguments)) != normalize(baseline_read()):
                raise RuntimeError("read/hash parity mismatch")
            feature_samples["known_read"] = {"live_cli": [], "shared_mcp": []}
            for _ in range(20):
                feature_samples["known_read"]["live_cli"].append(measured(baseline_read))
                feature_samples["known_read"]["shared_mcp"].append(measured(
                    lambda: mcp.call(root, "code_read", read_arguments)))

            changed_path = root / paths[0]
            old_contents = changed_path.read_bytes()
            changes = []
            for index in range(20):
                marker = f"cctx_update_probe_{index}_e49a"
                start = time.perf_counter_ns()
                changed_path.write_bytes(old_contents + b"\n" + marker.encode() + b"\n")
                while True:
                    value = rpc(home, root, "code_grep", {"pattern": marker, "limit": 8})["data"]
                    if value["total_matches"] == 1:
                        break
                    if (time.perf_counter_ns() - start) / 1e9 > 5:
                        raise RuntimeError("watcher update did not converge")
                    time.sleep(0.001)
                latency = (time.perf_counter_ns() - start) / 1e6
                status = rpc(home, root, "code_status", {})["data"]
                changes.append({"visible_ms": latency, "update": status["last_update"]})
            changed_path.write_bytes(old_contents)
            start = time.perf_counter_ns()
            synced = rpc(home, root, "code_sync", {})["data"]
            strict_sync_ms = (time.perf_counter_ns() - start) / 1e6
            ping = rpc(home, None, "ping", {})
            memory = {}
            for line in Path(f"/proc/{ping['pid']}/status").read_text().splitlines():
                if line.startswith(("VmRSS:", "VmHWM:")):
                    key, value = line.split(":", 1)
                    memory[key] = value.strip()
            summaries = {mode: summary([v for rows in per_query.values() for v in rows])
                         for mode, per_query in observations.items()}
            for query in queries:
                sizes = {v["bytes"] for name in ["warm_rg", "daemon_rpc", "shared_mcp", "shared_cli"]
                         for v in observations[name][query]}
                if len(sizes) != 1:
                    raise RuntimeError("unequal search payloads")
            report = {"schema": 1, "corpus": {"files": len(paths), "bytes": total_bytes,
                       "sha256": digest.hexdigest()}, "rounds": args.rounds,
                      "first_root_load_ms": first_load_ms, "initial_status": initial,
                      "summaries": summaries, "observations": observations,
                      "updates": changes, "strict_sync_ms": strict_sync_ms,
                      "feature_samples": feature_samples,
                      "feature_summaries": {name: {mode: summary(values) for mode, values in per_mode.items()}
                                            for name, per_mode in feature_samples.items()},
                      "final_status": synced, "memory": memory,
                      "method": {"cache": "warm; no OS cache flushing",
                                 "comparison": "equal sorted first 8 lines and full counts; JSON normalization removes only consistency",
                                 "updates": "20 real source writes in private frozen copy; wall time to visible watcher-driven query",
                                 "agent_selection": "not measured", "agent_task_time": "not measured",
                                 "order": "rotating modes, one warmup round", "physical_io": "not measured"}}
            args.output.parent.mkdir(parents=True, exist_ok=True)
            with args.output.open("x") as output:
                json.dump(report, output, indent=2)
            print(json.dumps({"summaries": summaries, "memory": memory,
                              "update_visibility": summary([{"ms": v["visible_ms"]} for v in changes]),
                              "first_root_load_ms": first_load_ms, "strict_sync_ms": strict_sync_ms}, indent=2))
        finally:
            try:
                if mcp:
                    mcp.close()
            finally:
                daemon.terminate()
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()


if __name__ == "__main__":
    main()
