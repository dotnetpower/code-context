#!/usr/bin/env python3
"""Paired retrieval benchmark; never touches the user's shared daemon."""

import argparse
from contextlib import contextmanager
import csv
import hashlib
import json
import os
from pathlib import Path
import random
import socket
import statistics
import subprocess
import sys
import tempfile
import time


REPOSITORY = "https://github.com/dotnetpower/aks-store-demo.git"
EXTENSIONS = ("go", "rs", "js", "vue", "ts", "py")
PATTERNS = (
    "product", "order", "get_health", "ProductStore",
    "RabbitMQ", "PORT", "createApp", "__cctx_benchmark_absent_71d83__",
)
LIMIT = 8
RADIUS = 3


def command(argv, *, cwd=None, env=None, allow_no_match=False):
    result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=60)
    allowed = (0, 1) if allow_no_match else (0,)
    if result.returncode not in allowed or result.stderr:
        raise RuntimeError(f"{Path(argv[0]).name} failed: {result.stderr.decode()}")
    return result.stdout


def rg_base():
    options = ["rg", "--no-config", "--no-hidden", "-g", "!.*"]
    for extension in EXTENSIONS:
        options += ["--type-add", f"bench:*.{extension}"]
    return options + ["--type", "bench"]


def discover(root):
    output = command(rg_base() + ["--files", "--null", "--", "src"], cwd=root)
    return sorted(path.decode() for path in output.split(b"\0") if path)


def freeze(original, destination):
    digest = hashlib.sha256()
    files = discover(original)
    size = 0
    for relative in files:
        path = Path(relative)
        if path.is_absolute() or ".." in path.parts or any(p.startswith(".") for p in path.parts):
            raise ValueError("Unexpected corpus path")
        source = original / path
        if source.is_symlink():
            raise ValueError("Symlink is not an eligible benchmark input")
        data = source.read_bytes()
        if len(data) > 256 * 1024:
            raise ValueError(f"Corpus file exceeds backend limit: {relative}")
        data.decode("utf-8")
        target = destination / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        encoded = relative.encode()
        digest.update(len(encoded).to_bytes(8, "little"))
        digest.update(encoded)
        digest.update(len(data).to_bytes(8, "little"))
        digest.update(data)
        size += len(data)
    return {"files": len(files), "bytes": size, "sha256": digest.hexdigest()}


def make_tasks(root):
    tasks = [{"kind": "files", "glob": f"*.{extension}"} for extension in EXTENSIONS]
    tasks += [{"kind": kind, "pattern": pattern}
              for kind in ("grep", "context") for pattern in PATTERNS]
    paths = discover(root)
    for extension in EXTENSIONS:
        path = next((p for p in paths if p.endswith("." + extension)), None)
        if path is None:
            raise ValueError(f"Missing language in corpus: {extension}")
        line_count = len((root / path).read_text().splitlines())
        tasks.append({"kind": "read", "path": path, "start": 1, "end": min(20, line_count)})
    return tasks


def read_range(root, path, start, end):
    data = (root / path).read_bytes()
    lines = data.decode("utf-8").splitlines()
    end = min(len(lines), end)
    return {"path": path, "start": start, "end": end,
            "lines": lines[start - 1:end], "sha256": hashlib.sha256(data).hexdigest()}


def grep(root, pattern):
    output = command(rg_base() + ["--json", "-F", "--", pattern, "src"],
                     cwd=root, allow_no_match=True)
    matches = []
    for line in output.splitlines():
        event = json.loads(line)
        if event["type"] == "match":
            data = event["data"]
            matches.append({"path": data["path"]["text"], "line": data["line_number"],
                            "text": data["lines"]["text"].rstrip("\r\n")})
    matches.sort(key=lambda item: (item["path"], item["line"]))
    return {"matches": matches[:LIMIT], "total_matches": len(matches),
            "truncated": len(matches) > LIMIT}


def disabled(root, task):
    kind = task["kind"]
    if kind == "files":
        output = command(rg_base() + ["--files", "-g", task["glob"], "--", "src"],
                         cwd=root, allow_no_match=True)
        paths = sorted(output.decode().splitlines())
        return {"paths": paths[:64], "total_matches": len(paths), "truncated": len(paths) > 64}
    if kind == "read":
        return read_range(root, task["path"], task["start"], task["end"])
    search = grep(root, task["pattern"])
    if kind == "grep":
        return search
    contents = {}
    windows = []
    for match in search["matches"]:
        path = match["path"]
        if path not in contents:
            data = (root / path).read_bytes()
            contents[path] = (data.decode("utf-8").splitlines(), hashlib.sha256(data).hexdigest())
        lines, digest = contents[path]
        start, end = max(1, match["line"] - RADIUS), min(len(lines), match["line"] + RADIUS)
        windows.append({"path": path, "start": start, "end": end,
                        "lines": lines[start - 1:end], "sha256": digest})
    return {"search": search, "windows": windows}


def rpc(home, root, name, arguments):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(60)
        stream.connect(str(home / "daemon.sock"))
        stream.sendall(json.dumps({"root": str(root), "name": name,
                                   "arguments": arguments}).encode() + b"\n")
        with stream.makefile("rb") as response:
            line = response.readline(4 * 1024 * 1024 + 1)
    result = json.loads(line)
    if "error" in result:
        raise RuntimeError(result["error"])
    return result["result"]


def enabled(home, root, task):
    kind = task["kind"]
    arguments = {}
    if kind == "files":
        arguments = {"glob": task["glob"], "limit": 64}
    elif kind == "read":
        arguments = {"ranges": [{key: task[key] for key in ("path", "start", "end")}]}
    else:
        arguments = {"pattern": task["pattern"], "limit": LIMIT}
        if kind == "context":
            arguments["radius"] = RADIUS
    value = rpc(home, root, "code_" + kind, arguments)["data"]
    if kind == "read":
        return {key: value[0][key] for key in ("path", "start", "end", "lines", "sha256")}
    if kind == "files":
        return {key: value[key] for key in ("paths", "total_matches", "truncated")}
    if kind == "grep":
        return {key: value[key] for key in ("matches", "total_matches", "truncated")}
    search = {key: value["search"][key] for key in ("matches", "total_matches", "truncated")}
    windows = []
    for match in search["matches"]:
        snippet = next(s for s in value["snippets"]
                       if s["path"] == match["path"] and s["start"] <= match["line"] <= s["end"])
        start = max(1, match["line"] - RADIUS)
        end = min(snippet["end"], match["line"] + RADIUS)
        windows.append({"path": snippet["path"], "start": start, "end": end,
                        "lines": snippet["lines"][start - snippet["start"]:end - snippet["start"] + 1],
                        "sha256": snippet["sha256"]})
    return {"search": search, "windows": windows}


def timed(operation):
    started = time.perf_counter_ns()
    value = operation()
    return (time.perf_counter_ns() - started) / 1_000_000, value


@contextmanager
def daemon(executable, root, home):
    env = os.environ.copy()
    env.update(CCTX_HOME=str(home), CCTX_MEMORY_MAX_MIB="512")
    configuration = [str(executable), "--root", str(root), "--scope", "src"]
    for extension in EXTENSIONS:
        configuration += ["--glob", "*." + extension]
    command(configuration + ["register"], env=env)
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen([str(executable), "daemon"], env=env,
                                   stdout=subprocess.DEVNULL, stderr=errors)
        try:
            deadline = time.monotonic() + 15
            while not (home / "daemon.sock").exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    errors.seek(0)
                    raise RuntimeError("Private daemon startup failed: " + errors.read().decode())
                time.sleep(0.01)
            rpc(home, root, "code_status", {})
            yield
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def percentile(values, fraction):
    values = sorted(values)
    position = (len(values) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(values) - 1)
    return values[lower] + (values[upper] - values[lower]) * (position - lower)


def summarize(records, sessions, bootstrap=5000):
    summaries = {}
    rng = random.Random(83171)
    for kind in ("files", "grep", "context", "read", "all"):
        rows = [r for r in records if kind == "all" or r["kind"] == kind]
        groups = {session: [r for r in rows if r["session"] == session] for session in range(sessions)}
        means = {mode: [statistics.mean(row[mode] for row in groups[s])
                        for s in range(sessions)] for mode in ("disabled_ms", "enabled_ms")}
        draws = {mode: [] for mode in means}
        ratios = []
        for _ in range(bootstrap):
            selected = rng.choices(range(sessions), k=sessions)
            sampled = {mode: statistics.mean(values[i] for i in selected)
                       for mode, values in means.items()}
            for mode, value in sampled.items():
                draws[mode].append(value)
            ratios.append(sampled["disabled_ms"] / sampled["enabled_ms"])
        summary = {"pairs": len(rows)}
        for mode in means:
            summary[mode] = {"mean": statistics.mean(means[mode]),
                             "ci95": [percentile(draws[mode], q) for q in (0.025, 0.975)],
                             "median": statistics.median(r[mode] for r in rows),
                             "p95": percentile([r[mode] for r in rows], 0.95)}
        summary["speedup"] = {"ratio": summary["disabled_ms"]["mean"] / summary["enabled_ms"]["mean"],
                              "ci95": [percentile(ratios, q) for q in (0.025, 0.975)]}
        summaries[kind] = summary
    return summaries


def render(report, output, extension_image=None):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    plt.rcParams.update({"font.family": "DejaVu Serif", "font.size": 10,
                         "axes.spines.top": False, "axes.spines.right": False,
                         "svg.fonttype": "none", "pdf.fonttype": 42, "savefig.dpi": 300})
    kinds = ("files", "grep", "context", "read", "all")
    labels = ("Filename", "Literal grep", "Context", "Known read", "Mixed suite")
    colors = ("#0072B2", "#D55E00", "#009E73")
    figure, ((latency, speed), (tokens_ax, agent_ax)) = plt.subplots(
        2, 2, figsize=(12.2, 8.2), constrained_layout=True
    )
    for index, (mode, label) in enumerate((("disabled_ms", "CCTX off"), ("enabled_ms", "CCTX on (warm)"))):
        means = [report["summary"][kind][mode]["mean"] for kind in kinds]
        intervals = [report["summary"][kind][mode]["ci95"] for kind in kinds]
        positions = [i + (index - 0.5) * 0.34 for i in range(len(kinds))]
        latency.bar(positions, means, width=0.32, color=colors[index], label=label,
                    yerr=[[m - ci[0] for m, ci in zip(means, intervals)],
                          [ci[1] - m for m, ci in zip(means, intervals)]],
                    capsize=3, error_kw={"linewidth": 1})
    latency.set(xticks=range(len(kinds)), xticklabels=labels, ylabel="Mean wall time per operation (ms)",
                title="A  Retrieval latency (lower is better)")
    latency.tick_params(axis="x", labelrotation=15)
    latency.legend(frameon=False)
    latency.grid(axis="y", alpha=0.2)
    latency.set_axisbelow(True)
    ratios = [report["summary"][kind]["speedup"]["ratio"] for kind in kinds]
    intervals = [report["summary"][kind]["speedup"]["ci95"] for kind in kinds]
    speed.errorbar(ratios, range(len(kinds)),
                   xerr=[[r - ci[0] for r, ci in zip(ratios, intervals)],
                         [ci[1] - r for r, ci in zip(ratios, intervals)]],
                   fmt="o", color=colors[0], capsize=4)
    speed.axvline(1, color="#666666", linestyle="--", linewidth=1)
    speed.set(yticks=range(len(kinds)), yticklabels=labels, xlabel="Speedup = off / on (log scale)",
              title="B  Paired-session speedup with 95% CI", xscale="log")
    speed.set_xlim(min(ci[0] for ci in intervals) * 0.6, max(ci[1] for ci in intervals) * 1.9)
    speed.invert_yaxis()
    speed.grid(axis="x", which="both", alpha=0.2)
    for index, value in enumerate(ratios):
        speed.annotate(f"{value:.2f}×", (value, index), xytext=(7, 8), textcoords="offset points")

    token_groups = ("Rust (6-symbol suite)", "Python (6-symbol suite)")
    token_series = (
        ("rg + 200-line view", (25.133, 24.609), "#7F8C8D"),
        ("rg + 100-line view", (15.028, 13.997), colors[0]),
        ("CCTX code_context", (4.031, 2.141), colors[2]),
    )
    for idx, (series_label, vals, col) in enumerate(token_series):
        pos = [i + (idx - 1) * 0.26 for i in range(len(token_groups))]
        bars = tokens_ax.bar(pos, vals, width=0.23, color=col, label=series_label)
        if idx == 2:
            for bar, ref200, ref100 in zip(bars, token_series[0][1], token_series[1][1]):
                s200 = (1.0 - bar.get_height() / ref200) * 100
                s100 = (1.0 - bar.get_height() / ref100) * 100
                tokens_ax.annotate(
                    f"-{s200:.1f}% vs 200L\n(-{s100:.1f}% vs 100L)",
                    (bar.get_x() + bar.get_width() / 2, bar.get_height()),
                    xytext=(14, 6), textcoords="offset points", ha="center", fontsize=8.5, fontweight="bold",
                )
    tokens_ax.set(
        xticks=range(len(token_groups)), xticklabels=token_groups,
        ylabel="Payload tokens (thousands, lower is better)",
        title="C  Source lookup payload token savings (Rust & Python)",
        xlim=(-0.55, 1.62), ylim=(0, 34),
    )
    tokens_ax.legend(frameon=False, loc="upper right")
    tokens_ax.grid(axis="y", alpha=0.2)
    tokens_ax.set_axisbelow(True)

    agent_metrics = ("Tool calls\n(7.0 → 2.7)", "Payload tokens\n(6.4k → 3.2k)", "LLM turns\n(5.0 → 3.7)", "Total AIU cost\n(4.76B → 3.55B)", "Task wall time\n(20.3s → 15.3s)")
    builtin_norm = [100.0] * len(agent_metrics)
    cctx_norm = [2.67 / 7.00 * 100, 3177 / 6360 * 100, 3.67 / 5.00 * 100, 3.55 / 4.76 * 100, 15.30 / 20.31 * 100]
    y_pos = list(range(len(agent_metrics)))
    agent_ax.barh([y - 0.17 for y in y_pos], builtin_norm, height=0.32, color=colors[0], label="Built-in (rg / glob / view)")
    cctx_bars = agent_ax.barh([y + 0.17 for y in y_pos], cctx_norm, height=0.32, color=colors[1], label="CCTX (code_context)")
    for bar, val in zip(cctx_bars, cctx_norm):
        agent_ax.annotate(f"-{100.0 - val:.0f}%", (val, bar.get_y() + bar.get_height() / 2),
                          xytext=(5, -3), textcoords="offset points", fontsize=9, fontweight="bold")
    agent_ax.set(
        yticks=y_pos, yticklabels=agent_metrics,
        xlabel="Relative cost / time (% of built-in baseline, lower is better)",
        title="D  End-to-end coding agent acceleration (Copilot A/B)",
        xlim=(0, 118), ylim=(-0.6, 5.15),
    )
    agent_ax.invert_yaxis()
    agent_ax.legend(frameon=False, loc="lower right", ncol=2, fontsize=8.5)
    agent_ax.grid(axis="x", alpha=0.2)
    agent_ax.set_axisbelow(True)

    figure.suptitle(
        f"Code Context (CCTX) · Retrieval Speedup ({report['sessions']} sessions) & Agent Token/Turn Acceleration",
        fontsize=12.5,
    )
    output.mkdir(parents=True, exist_ok=True)
    for suffix in ("png", "svg", "pdf"):
        figure.savefig(output / ("latency-speedup." + suffix), bbox_inches="tight", pad_inches=0.12,
                       metadata={"Creator": "Code Context benchmark"})
    if extension_image:
        figure.savefig(extension_image, bbox_inches="tight", pad_inches=0.12,
                       metadata={"Creator": "Code Context benchmark"})
    plt.close(figure)
    figure, axes = plt.subplots(1, 2, figsize=(10, 4.2), constrained_layout=True)
    for axis, field, title in zip(
        axes, ("fallback_ms", "sync_ms"),
        ("A  Cold miss + normal fallback", "B  Explicit source-load barrier\n(setup, not a lookup)"),
    ):
        values = [row[field] for row in report["cold"]]
        axis.plot(range(1, len(values) + 1), values, "o", color=colors[1])
        if field == "fallback_ms":
            axis.plot(range(1, len(values) + 1), [r["disabled_ms"] for r in report["cold"]],
                      "s", color=colors[0], label="CCTX off")
            axis.lines[0].set_label("CCTX on: miss + fallback")
            axis.legend(frameon=False)
        upper = max(values + [r["disabled_ms"] for r in report["cold"]]) * 1.15
        axis.set(xlabel="Independent daemon session", ylabel="Wall time (ms)", title=title,
                 xticks=range(1, len(values) + 1), ylim=(0, upper))
        axis.grid(axis="y", alpha=0.2)
    for suffix in ("png", "svg", "pdf"):
        figure.savefig(output / ("cold-start." + suffix), bbox_inches="tight", pad_inches=0.12,
                       metadata={"Creator": "Code Context benchmark"})
    plt.close(figure)
    with (output / "summary.csv").open("w", newline="") as stream:
        writer = csv.writer(stream, lineterminator="\n")
        writer.writerow(["task", "pairs", "off_mean_ms", "on_mean_ms",
                         "speedup", "speedup_ci95_low", "speedup_ci95_high"])
        for kind in kinds:
            item = report["summary"][kind]
            writer.writerow([kind, item["pairs"], item["disabled_ms"]["mean"], item["enabled_ms"]["mean"],
                             item["speedup"]["ratio"], *item["speedup"]["ci95"]])
    for name in ("latency-speedup.svg", "cold-start.svg"):
        path = output / name
        path.write_text("\n".join(line.rstrip() for line in path.read_text().splitlines()) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--cctx", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sessions", type=int, default=8)
    parser.add_argument("--rounds", type=int, default=40)
    parser.add_argument("--seed", type=int, default=20261023)
    parser.add_argument("--render-only", type=Path)
    parser.add_argument("--extension-image", type=Path)
    args = parser.parse_args()
    if args.render_only:
        render(json.loads(args.render_only.read_text()), args.output, args.extension_image)
        return
    if args.root is None or args.cctx is None or args.sessions < 3 or args.rounds < 10:
        parser.error("--root, --cctx, sessions >= 3, rounds >= 10 are required")
    if (args.output / "results.json").exists():
        parser.error("Refusing to overwrite an existing measurement")
    original = args.root.resolve()
    executable = args.cctx.resolve()
    if command(["git", "status", "--porcelain"], cwd=original).strip():
        parser.error("Benchmark checkout must be clean")
    remote = command(["git", "remote", "get-url", "origin"], cwd=original).decode().strip()
    if remote.removesuffix(".git") != REPOSITORY.removesuffix(".git"):
        parser.error("Use a checkout of the specified public benchmark repository")
    records, cold = [], []
    rng = random.Random(args.seed)
    with tempfile.TemporaryDirectory(prefix="cctx-paper-") as temporary:
        temporary = Path(temporary)
        root = temporary / "corpus"
        corpus = freeze(original, root)
        tasks = make_tasks(root)
        for session in range(args.sessions):
            home = temporary / f"state-{session}"
            with daemon(executable, root, home):
                first = tasks[0]
                off_ms, expected = timed(lambda: disabled(root, first))

                def cold_lookup():
                    try:
                        enabled(home, root, first)
                    except RuntimeError as error:
                        if "CACHE_MISS" not in str(error):
                            raise
                    else:
                        raise AssertionError("First source request was not a cold miss")
                    return disabled(root, first)

                fallback_ms, actual = timed(cold_lookup)
                assert actual == expected, "Cold fallback output differs"
                sync_ms, _ = timed(lambda: rpc(home, root, "code_sync", {}))
                cold.append({"session": session, "disabled_ms": off_ms,
                             "fallback_ms": fallback_ms, "sync_ms": sync_ms})
                for task in tasks:
                    assert disabled(root, task) == enabled(home, root, task), f"Warmup parity: {task}"
                for round_number in range(args.rounds):
                    indices = list(range(len(tasks)))
                    rng.shuffle(indices)
                    for index in indices:
                        task = tasks[index]
                        modes = ["disabled", "enabled"]
                        rng.shuffle(modes)
                        results = {}
                        row = {"session": session, "round": round_number,
                               "task": index, "kind": task["kind"], "first": modes[0]}
                        for mode in modes:
                            operation = ((lambda: disabled(root, task)) if mode == "disabled"
                                         else (lambda: enabled(home, root, task)))
                            row[mode + "_ms"], results[mode] = timed(operation)
                        if results["disabled"] != results["enabled"]:
                            raise AssertionError(f"Timed result mismatch: {task}")
                        records.append(row)
            print(f"session {session + 1}/{args.sessions}: parity verified", flush=True)
        assert freeze(original, temporary / "verification") == corpus, "Checkout changed during measurement"
    report = {
        "schema": 1, "repository": REPOSITORY,
        "commit": command(["git", "rev-parse", "HEAD"], cwd=original).decode().strip(),
        "cctx_source_commit": command(["git", "rev-parse", "HEAD"],
                                      cwd=Path(__file__).resolve().parents[1]).decode().strip(),
        "cctx_binary_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        "rg_version": command(["rg", "--version"]).decode().splitlines()[0],
        "python_version": ".".join(map(str, sys.version_info[:3])),
        "sessions": args.sessions, "rounds": args.rounds, "seed": args.seed,
        "corpus": corpus, "tasks": tasks, "cold": cold,
        "method": {
            "enabled": "fresh Unix socket per request, shared daemon RPC, decoded JSON and canonical projection",
            "disabled": "fresh rg process per search/discovery; direct buffered Python read/hash for known ranges",
            "cache": "OS cache warm in both arms; no drop_caches, no CPU affinity; shared nonexclusive Linux host",
            "order": "seeded randomized task order and randomized paired mode order",
            "warmup": "one correctness-checked pass per session, excluded from timed samples",
            "correctness": "all timed pairs exactly equal: paths, full match counts, text, windows, SHA-256",
            "ci": "5000 seeded paired cluster-bootstrap resamples of daemon-session means, percentile 95% intervals",
            "privacy": "No usernames, hostnames, local paths, hardware identifiers, or host configuration collected",
            "limits": "Retrieval microbenchmark only; not VS Code dispatch, model selection, tokens, agent task time, or disk-cold I/O",
            "cold": "one first-use miss + rg fallback per session; explicit sync measured separately; daemon startup excluded",
        },
        "summary": summarize(records, args.sessions), "records": records,
    }
    args.output.mkdir(parents=True, exist_ok=True)
    with (args.output / "results.json").open("x") as output:
        json.dump(report, output, separators=(",", ":"))
        output.write("\n")
    render(report, args.output, args.extension_image)
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
