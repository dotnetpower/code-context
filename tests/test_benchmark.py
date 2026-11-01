import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("benchmark", ROOT / "benchmarks/benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
    def test_percentile_interpolates_instead_of_rounding_tail(self):
        self.assertEqual(benchmark.percentile([4, 1, 3, 2], 0.5), 2.5)
        self.assertAlmostEqual(benchmark.percentile([1, 2, 3, 4], 0.95), 3.85)

    def test_cluster_bootstrap_preserves_paired_session_ratio(self):
        records = [
            {"session": session, "kind": kind, "disabled_ms": session + 1,
             "enabled_ms": (session + 1) / 2}
            for session in range(3) for kind in ("files", "grep", "context", "read")
        ]
        first = benchmark.summarize(records, 3, bootstrap=100)
        self.assertEqual(first, benchmark.summarize(records, 3, bootstrap=100))
        for kind, summary in first.items():
            self.assertEqual(summary["speedup"]["ratio"], 2)
            self.assertEqual(summary["speedup"]["ci95"], [2, 2], kind)
            self.assertEqual(summary["pairs"], 12 if kind == "all" else 3)

    def test_known_read_includes_whole_file_hash_and_bounded_lines(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "sample.py").write_bytes(b"one\r\ntwo\r\nthree\r\n")
            result = benchmark.read_range(root, "sample.py", 2, 20)
            self.assertEqual(result["lines"], ["two", "three"])
            self.assertEqual((result["start"], result["end"]), (2, 3))
            self.assertEqual(len(result["sha256"]), 64)

    def test_published_results_match_task_counts_and_summaries(self):
        path = ROOT / "benchmarks/results/aks-store-demo/results.json"
        if not path.exists():
            self.skipTest("Published measurements not yet generated")
        report = json.loads(path.read_text())
        self.assertEqual(len(report["records"]),
                         report["sessions"] * report["rounds"] * len(report["tasks"]))
        self.assertEqual(report["summary"], benchmark.summarize(report["records"], report["sessions"]))
        self.assertEqual(len(report["cold"]), report["sessions"])
        for record in report["records"]:
            self.assertGreater(record["disabled_ms"], 0)
            self.assertGreater(record["enabled_ms"], 0)

    def test_public_documentation_contains_no_local_machine_paths(self):
        documents = [ROOT / name for name in ("README.md", "DESIGN.md", "REPORT.md")]
        documents += list((ROOT / "docs").rglob("*.md"))
        documents += [ROOT / "vscode-extension/README.md"]
        documents += [ROOT / "vscode-extension/CHANGELOG.md"]
        documents += list((ROOT / "vscode-extension/docs").rglob("*.md"))
        for path in documents:
            text = path.read_text()
            self.assertNotRegex(text, r"(?:/home/|/Users/|[A-Z]:\\Users\\)[^<\s/\\]+")
            self.assertNotIn(str(Path.home()), text)

    def test_readmes_are_short_and_claims_match_published_measurement(self):
        report = json.loads((ROOT / "benchmarks/results/aks-store-demo/results.json").read_text())
        summary = report["summary"]["all"]
        for path in (ROOT / "README.md", ROOT / "vscode-extension/README.md"):
            text = path.read_text()
            self.assertLessEqual(len(text.splitlines()), 100)
            for value in (summary["disabled_ms"]["mean"], summary["enabled_ms"]["mean"]):
                self.assertIn(f"{value:.3f}", text)
            for value in (summary["speedup"]["ratio"], *summary["speedup"]["ci95"]):
                self.assertIn(f"{value:.2f}", text)
            self.assertIn("slower", text)
            self.assertIn("not", text)

    def test_relative_document_links_and_images_exist(self):
        def md_slug(heading: str) -> str:
            text = re.sub(r"^[#=]+\s*|\s*#+$", "", heading.strip())
            text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
            slug = "".join(
                ch.lower() if ch.isalnum() else ("-" if ch in " \t-" else ("_" if ch == "_" else ""))
                for ch in text
            )
            return slug.strip("-")

        documents = list(ROOT.glob("*.md")) + list((ROOT / "docs").rglob("*.md"))
        documents += [ROOT / "vscode-extension/README.md"]
        documents += list((ROOT / "vscode-extension/docs").rglob("*.md"))
        for path in documents:
            for raw_target in re.findall(r"\]\(([^)\s]+)\)", path.read_text()):
                if "://" in raw_target or raw_target.startswith("#"):
                    continue
                parts = raw_target.split("#", 1)
                target_path = (path.parent / parts[0]).resolve()
                self.assertTrue(target_path.exists(), f"{path.relative_to(ROOT)} -> {raw_target}")
                if len(parts) == 2 and parts[1] and target_path.suffix.lower() == ".md":
                    anchor = parts[1].lower()
                    headings = [
                        md_slug(line)
                        for line in target_path.read_text().splitlines()
                        if re.match(r"^\s{0,3}#{1,6}\s+", line)
                    ]
                    collapsed = [re.sub(r"-+", "-", h) for h in headings]
                    self.assertTrue(
                        anchor in headings or re.sub(r"-+", "-", anchor) in collapsed,
                        f"{path.relative_to(ROOT)} -> {raw_target} missing heading anchor #{anchor}",
                    )

    def test_installer_blocks_legacy_publisher_before_dependency_install(self):
        with tempfile.TemporaryDirectory() as directory:
            bin_path = Path(directory)
            code = bin_path / "code"
            node = bin_path / "node"
            npm = bin_path / "npm"
            node.write_text("#!/bin/sh\nexit 98\n")
            node.chmod(0o700)
            npm.write_text("#!/bin/sh\nexit 99\n")
            npm.chmod(0o700)
            env = {**os.environ, "PATH": str(bin_path) + os.pathsep + os.environ["PATH"]}
            for publisher, expected in (("moonchoi", 1), ("dotnetpower", 99)):
                with self.subTest(publisher=publisher):
                    code.write_text(f"#!/bin/sh\nprintf '%s\\n' '{publisher}.code-context-native'\n")
                    code.chmod(0o700)
                    result = subprocess.run(
                        ["bash", str(ROOT / "scripts/install-vscode-extension.sh")],
                        env=env, capture_output=True, text=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, expected, result.stderr)
                    if expected == 1:
                        self.assertIn("Remove the previous publisher extension", result.stderr)


if __name__ == "__main__":
    unittest.main()
