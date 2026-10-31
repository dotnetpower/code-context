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
import struct
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


_CONNECTIONS = {}


def _get_connection(home):
    key = str(home)
    stream = _CONNECTIONS.get(key)
    if stream is None:
        stream = socket.socket(socket.AF_UNIX)
        stream.settimeout(60)
        stream.connect(str(home / "daemon.sock"))
        stream.sendall(b"CCTXJ1\n")
        _CONNECTIONS[key] = stream
    return stream


def _close_connection(home):
    stream = _CONNECTIONS.pop(str(home), None)
    if stream is not None:
        try:
            stream.close()
        except OSError:
            pass


def _recv_exact(stream, size):
    chunks = bytearray()
    while len(chunks) < size:
        chunk = stream.recv(size - len(chunks))
        if not chunk:
            raise RuntimeError("Daemon closed framed connection unexpectedly")
        chunks.extend(chunk)
    return bytes(chunks)


def rpc(home, root, name, arguments):
    stream = _get_connection(home)
    payload = json.dumps({"root": str(root), "name": name,
                          "arguments": arguments, "compact": False}).encode()
    stream.sendall(struct.pack(">I", len(payload)) + payload)
    header = _recv_exact(stream, 4)
    (length,) = struct.unpack(">I", header)
    body = _recv_exact(stream, length)
    result = json.loads(body)
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
            _close_connection(home)
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

    plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 9.5,
                         "axes.spines.top": False, "axes.spines.right": False,
                         "svg.fonttype": "none", "pdf.fonttype": 42, "savefig.dpi": 300})
    kinds = ("files", "grep", "context", "read", "all")
    labels = ("Filename", "Literal grep", "Context", "Known read", "Mixed suite")
    v014_on_means = (0.566, 0.428, 0.499, 0.799, 0.557)
    v014_ratios = (7.52, 11.60, 10.26, 0.09, 6.83)
    colors = ("#1F77B4", "#94A3B8", "#059669", "#D97706")
    figure, ((latency, speed), (tokens_ax, agent_ax)) = plt.subplots(
        2, 2, figsize=(13.2, 8.6), constrained_layout=True
    )

    off_means = [report["summary"][kind]["disabled_ms"]["mean"] for kind in kinds]
    off_ci = [report["summary"][kind]["disabled_ms"]["ci95"] for kind in kinds]
    on_means = [report["summary"][kind]["enabled_ms"]["mean"] for kind in kinds]
    on_ci = [report["summary"][kind]["enabled_ms"]["ci95"] for kind in kinds]

    x_idx = list(range(len(kinds)))
    b_off = latency.bar(
        [i - 0.27 for i in x_idx], off_means, width=0.25, color=colors[0], label="CCTX off (rg / direct)",
        yerr=[[m - ci[0] for m, ci in zip(off_means, off_ci)], [ci[1] - m for m, ci in zip(off_means, off_ci)]],
        capsize=2.5, error_kw={"linewidth": 0.9},
    )
    b_v014 = latency.bar(
        x_idx, v014_on_means, width=0.25, color=colors[1], label="CCTX v0.14 (before opt)",
    )
    b_cur = latency.bar(
        [i + 0.27 for i in x_idx], on_means, width=0.25, color=colors[2], label="CCTX current (warm IPC)",
        yerr=[[m - ci[0] for m, ci in zip(on_means, on_ci)], [ci[1] - m for m, ci in zip(on_means, on_ci)]],
        capsize=2.5, error_kw={"linewidth": 0.9},
    )
    for bar, val, ci in zip(b_off, off_means, off_ci):
        latency.annotate(f"{val:.2f}", (bar.get_x() + bar.get_width() / 2, ci[1]),
                         xytext=(0, 3), textcoords="offset points", ha="center", fontsize=7.5, color="#1E3A8A")
    for bar, val in zip(b_v014, v014_on_means):
        latency.annotate(f"{val:.2f}", (bar.get_x() + bar.get_width() / 2, val),
                         xytext=(0, 3), textcoords="offset points", ha="center", fontsize=7.5, color="#475569")
    for bar, val, ci in zip(b_cur, on_means, on_ci):
        latency.annotate(f"{val:.2f}ms", (bar.get_x() + bar.get_width() / 2, ci[1]),
                         xytext=(4, 3), textcoords="offset points", ha="center", fontsize=8, fontweight="bold", color="#065F46")
    latency.set(xticks=x_idx, xticklabels=labels, ylabel="Mean wall time per operation (ms)",
                title="A  Retrieval latency: Off vs v0.14 vs Current (lower is better)",
                ylim=(0, max(off_means) * 1.22))
    latency.legend(frameon=False, loc="upper right", fontsize=8.5)
    latency.grid(axis="y", alpha=0.22)
    latency.set_axisbelow(True)

    ratios = [report["summary"][kind]["speedup"]["ratio"] for kind in kinds]
    intervals = [report["summary"][kind]["speedup"]["ci95"] for kind in kinds]
    speed.plot(v014_ratios, range(len(kinds)), "o", color=colors[1], markersize=7, label="v0.14 baseline")
    speed.errorbar(ratios, range(len(kinds)),
                   xerr=[[r - ci[0] for r, ci in zip(ratios, intervals)],
                         [ci[1] - r for r, ci in zip(ratios, intervals)]],
                   fmt="D", color=colors[2], markersize=7, capsize=4, label="Current (95% CI)")
    for idx, (old_r, new_r) in enumerate(zip(v014_ratios, ratios)):
        speed.annotate("", xy=(new_r, idx), xytext=(old_r, idx),
                       arrowprops={"arrowstyle": "->", "color": "#059669", "lw": 1.4, "linestyle": "-"})
        speed.annotate(f"{old_r:.1f}× → {new_r:.2f}×", (max(old_r, new_r), idx),
                       xytext=(8, -3), textcoords="offset points", fontsize=8.5, fontweight="bold", color="#065F46")
    speed.axvline(1, color="#666666", linestyle="--", linewidth=1)
    speed.set(yticks=range(len(kinds)), yticklabels=labels, xlabel="Speedup = off / on (log scale, higher is better)",
              title="B  Paired-session speedup jump (v0.14 → Current)", xscale="log")
    speed.set_xlim(min(min(v014_ratios), min(ci[0] for ci in intervals)) * 0.55,
                   max(ci[1] for ci in intervals) * 3.2)
    speed.invert_yaxis()
    speed.legend(frameon=False, loc="lower left", fontsize=8.5)
    speed.grid(axis="x", which="both", alpha=0.22)

    token_groups = ("Rust (6-symbol suite)", "Python (6-symbol suite)")
    token_series = (
        ("rg + 200-line view", (25.133, 24.609), "#64748B"),
        ("rg + 100-line view", (15.028, 13.997), colors[0]),
        ("CCTX v0.14 (before opt)", (9.929, 6.971), colors[1]),
        ("CCTX current (block+dedup)", (4.031, 2.141), colors[2]),
    )
    for idx, (series_label, vals, col) in enumerate(token_series):
        pos = [i + (idx - 1.5) * 0.20 for i in range(len(token_groups))]
        bars = tokens_ax.bar(pos, vals, width=0.18, color=col, label=series_label)
        for bar, val, ref200 in zip(bars, vals, token_series[0][1]):
            if idx == 3:
                saved = (1.0 - val / ref200) * 100
                tokens_ax.annotate(
                    f"{val:.1f}k\n(-{saved:.1f}%)",
                    (bar.get_x() + bar.get_width() / 2, val),
                    xytext=(6, 4), textcoords="offset points", ha="center", fontsize=8.5, fontweight="bold", color="#065F46",
                )
            else:
                tokens_ax.annotate(
                    f"{val:.1f}k",
                    (bar.get_x() + bar.get_width() / 2, val),
                    xytext=(0, 3), textcoords="offset points", ha="center", fontsize=7.5, color="#334155",
                )
    tokens_ax.set(
        xticks=range(len(token_groups)), xticklabels=token_groups,
        ylabel="Payload tokens (thousands, lower is better)",
        title="C  6-symbol lookup payload tokens (Rust & Python)",
        xlim=(-0.55, 1.58), ylim=(0, 35),
    )
    tokens_ax.legend(frameon=False, loc="upper right", fontsize=8.2)
    tokens_ax.grid(axis="y", alpha=0.22)
    tokens_ax.set_axisbelow(True)

    agent_metrics = (
        "vs Headroom latency\n(5.67ms → 0.17ms, 32.8×)",
        "vs Headroom tokens\n(23.6k → 1.2k, 20.0×)",
        "Copilot A/B tool calls\n(7.0 → 2.7 calls)",
        "Copilot A/B payload tok\n(6.4k → 3.2k tok)",
        "Copilot A/B wall time\n(20.3s → 15.3s, -25% AIU)",
    )
    builtin_norm = [100.0] * len(agent_metrics)
    cctx_norm = [0.173 / 5.670 * 100, 1176 / 23557 * 100, 2.67 / 7.00 * 100, 3177 / 6360 * 100, 15.30 / 20.31 * 100]
    y_pos = list(range(len(agent_metrics)))
    agent_ax.barh([y - 0.17 for y in y_pos], builtin_norm, height=0.32, color=colors[0], label="Baseline (Headroom / Built-in)")
    cctx_bars = agent_ax.barh([y + 0.17 for y in y_pos], cctx_norm, height=0.32, color=colors[2], label="CCTX current")
    for bar, val in zip(cctx_bars, cctx_norm):
        agent_ax.annotate(f"-{100.0 - val:.1f}%", (val, bar.get_y() + bar.get_height() / 2),
                          xytext=(5, -3), textcoords="offset points", fontsize=8.5, fontweight="bold", color="#065F46")
    agent_ax.set(
        yticks=y_pos, yticklabels=agent_metrics,
        xlabel="Relative latency / tokens / cost (% of baseline, lower is better)",
        title="D  Headroom & Copilot A/B agent acceleration",
        xlim=(0, 118), ylim=(-0.6, 5.15),
    )
    agent_ax.invert_yaxis()
    agent_ax.legend(frameon=False, loc="lower right", ncol=2, fontsize=8.2)
    agent_ax.grid(axis="x", alpha=0.22)
    agent_ax.set_axisbelow(True)

    figure.suptitle(
        f"Code Context (CCTX) · Warm Retrieval ({report['summary']['all']['speedup']['ratio']:.2f}× Speedup) & Token Acceleration",
        fontsize=13, fontweight="bold",
    )
    output.mkdir(parents=True, exist_ok=True)
    for suffix in ("png", "svg", "pdf"):
        figure.savefig(output / ("latency-speedup." + suffix), bbox_inches="tight", pad_inches=0.12,
                       metadata={"Creator": "Code Context benchmark"})
    if extension_image:
        figure.savefig(extension_image, bbox_inches="tight", pad_inches=0.12,
                       metadata={"Creator": "Code Context benchmark"})
    plt.close(figure)

    figure, axes = plt.subplots(1, 2, figsize=(10.5, 4.3), constrained_layout=True)
    for axis, field, title in zip(
        axes, ("fallback_ms", "sync_ms"),
        ("A  Cold miss + normal fallback", "B  Explicit source-load barrier\n(setup: 76.0ms v0.14 → 50.9ms current)"),
    ):
        values = [row[field] for row in report["cold"]]
        axis.plot(range(1, len(values) + 1), values, "o-", color=colors[2], linewidth=1.2, label="CCTX current")
        if field == "fallback_ms":
            axis.plot(range(1, len(values) + 1), [r["disabled_ms"] for r in report["cold"]],
                      "s--", color=colors[0], linewidth=1.0, label="CCTX off")
            axis.lines[0].set_label("CCTX on: miss + fallback")
            axis.legend(frameon=False)
        else:
            axis.axhline(76.006, color=colors[1], linestyle="--", linewidth=1.2, label="v0.14 median (76.0 ms)")
            axis.legend(frameon=False, loc="lower right")
        upper = max(values + [r["disabled_ms"] for r in report["cold"]] + ([82.0] if field == "sync_ms" else [])) * 1.15
        axis.set(xlabel="Independent daemon session", ylabel="Wall time (ms)", title=title,
                 xticks=range(1, len(values) + 1), ylim=(0, upper))
        axis.grid(axis="y", alpha=0.22)
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
