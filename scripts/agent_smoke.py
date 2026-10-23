#!/usr/bin/env python3
"""Probe fresh Copilot CLI tool adoption using synthetic, non-private source only."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    executable = Path(__file__).resolve().parents[1] / "target/release/cctx"
    report_path = executable.parents[2] / "logs/2026-10-17/agent-smoke.json"
    if report_path.exists():
        raise RuntimeError("refusing to overwrite smoke report")
    with tempfile.TemporaryDirectory(prefix="cctx-agent-") as temporary:
        temporary = Path(temporary)
        root = temporary / "project"
        (root / "src").mkdir(parents=True)
        (root / "src/stock.rs").write_text(
            "pub fn stock_after_sale(available: u32, sold: u32) -> Option<u32> {\n"
            "    available.checked_sub(sold)\n}\n"
            "pub fn restock(available: u32, added: u32) -> Option<u32> {\n"
            "    available.checked_add(added)\n}\n"
        )
        (root / "src/lib.rs").write_text("pub mod stock;\n")
        state = temporary / "state"
        home = temporary / "copilot"
        home.mkdir()
        env = os.environ.copy()
        env["CCTX_HOME"] = str(state)
        env["COPILOT_HOME"] = str(home)
        configuration = {"mcpServers": {"code-context": {"type": "local",
                         "command": str(executable), "args": ["shared-mcp"],
                         "env": {"CCTX_HOME": str(state)}, "tools": ["*"]}}}
        (home / "mcp-config.json").write_text(json.dumps(configuration))
        subprocess.run([str(executable), "--root", str(root), "--scope", "src",
                        "--glob", "*.rs", "register"], env=env, check=True, capture_output=True)
        daemon = subprocess.Popen([str(executable), "daemon"], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            start = time.monotonic()
            while not (state / "daemon.sock").exists():
                if time.monotonic() - start > 10:
                    raise RuntimeError("isolated daemon failed readiness")
                time.sleep(0.01)
            command = ["copilot", "-C", str(root), "-p",
                       "Read only. In this small project, locate the function that prevents "
                       "selling more stock than is available. Give its name, file/line and "
                       "explain exactly how failure is represented. Do not modify files.",
                       "--allow-all-tools", "--disable-builtin-mcps", "--no-custom-instructions",
                       "--no-ask-user", "--output-format", "json", "--no-auto-update",
                       "--log-dir", str(home / "logs")]
            start = time.perf_counter()
            try:
                result = subprocess.run(command, env=env, cwd=root, capture_output=True, timeout=120)
                report = {"exit_code": result.returncode, "wall_seconds": time.perf_counter() - start,
                          "stdout": result.stdout.decode(errors="replace"),
                          "stderr": result.stderr.decode(errors="replace"),
                          "scope": "synthetic code only; fresh CLI, no custom instructions, built-in MCPs disabled",
                          "claim": "single smoke trial, not a statistical selection-rate measurement"}
            except subprocess.TimeoutExpired as error:
                report = {"timeout_seconds": 120, "stdout": (error.stdout or b"").decode(errors="replace"),
                          "stderr": (error.stderr or b"").decode(errors="replace"),
                          "claim": "host adoption not verified due to timeout"}
            report_path.write_text(json.dumps(report, indent=2))
            print(json.dumps({k: v for k, v in report.items() if k not in ["stdout", "stderr"]}, indent=2))
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()


if __name__ == "__main__":
    main()
