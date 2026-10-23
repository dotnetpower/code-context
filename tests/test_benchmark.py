import importlib.util
import json
from pathlib import Path
import re
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

    def test_public_documentation_contains_no_local_account_or_machine_paths(self):
        publisher = json.loads((ROOT / "vscode-extension/package.json").read_text())["publisher"]
        documents = [ROOT / name for name in ("README.md", "DESIGN.md", "REPORT.md")]
        documents += list((ROOT / "docs").rglob("*.md"))
        documents += [ROOT / "vscode-extension/README.md"]
        documents += [ROOT / "vscode-extension/CHANGELOG.md"]
        documents += list((ROOT / "vscode-extension/docs").rglob("*.md"))
        for path in documents:
            text = path.read_text()
            self.assertNotIn(publisher, text, str(path.relative_to(ROOT)))
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
        documents = list(ROOT.glob("*.md")) + list((ROOT / "docs").rglob("*.md"))
        documents += [ROOT / "vscode-extension/README.md"]
        documents += list((ROOT / "vscode-extension/docs").rglob("*.md"))
        for path in documents:
            for target in re.findall(r"\]\(([^)\s]+)\)", path.read_text()):
                if "://" in target or target.startswith("#"):
                    continue
                target = target.split("#")[0]
                self.assertTrue((path.parent / target).exists(), f"{path.relative_to(ROOT)} -> {target}")


if __name__ == "__main__":
    unittest.main()
