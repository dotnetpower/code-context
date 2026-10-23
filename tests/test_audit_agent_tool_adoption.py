import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts" / "audit_agent_tool_adoption.py"
SPEC = importlib.util.spec_from_file_location("audit_agent_tool_adoption", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def event(timestamp, channel, action):
    return json.dumps(
        {
            "_ahpLog": {"ts": timestamp},
            "params": {"channel": channel, "action": action},
        }
    )


class AdoptionAuditTests(unittest.TestCase):
    def test_expected_cache_misses_are_separate_from_empty_results_and_actual_load_failures(self):
        calls = [
            {"tool": "code-context-code_read", "success": False,
             "error": {"message": "MCP server 'code-context': CACHE_MISS root=/repo state=loading"}},
            {"tool": "cctxNativeContext", "success": False,
             "error": "CACHE_LOAD_FAILED root=/repo: source exceeds configured limit"},
            {"tool": "cctxNativeRead", "success": False, "error": "access denied"},
            {"tool": "code-context-code_read", "success": True, "useful": True,
             "error": "CACHE_MISS root=source-text-is-not-an-error"},
            {"tool": "view", "success": False, "error": "CACHE_MISS root=not-cctx"},
        ]
        counts = MODULE.outcome_counts(calls)
        self.assertEqual(counts["expected_cache_misses"], 1)
        self.assertEqual(counts["cache_load_failures"], 1)
        self.assertEqual(counts["useful_results"], 1)
        self.assertEqual(counts["empty_results"], 0)
        self.assertEqual(sum(call["success"] is False for call in calls), 4)

    def test_filters_by_event_time_scopes_call_ids_and_handles_reordered_files(self):
        now = 1_800_000_000.0
        recent = "2027-01-15T08:00:00+00:00"
        stale = "2027-01-14T00:00:00+00:00"
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "trace.jsonl"
            trace.write_text(
                "\n".join(
                    [
                        event(
                            stale,
                            "ahp-chat:stale",
                            {
                                "type": "chat/toolCallStart",
                                "toolCallId": "same",
                                "toolName": "rg",
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:first",
                            {
                                "type": "chat/toolCallComplete",
                                "toolCallId": "same",
                                "result": {"success": True},
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:first",
                            {
                                "type": "chat/toolCallStart",
                                "toolCallId": "same",
                                "toolName": "code-context-code_context",
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:second",
                            {
                                "type": "chat/toolCallStart",
                                "toolCallId": "same",
                                "toolName": "view",
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:second",
                            {
                                "type": "chat/toolCallComplete",
                                "toolCallId": "same",
                                "result": {"success": False, "error": "denied"},
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:incomplete",
                            {
                                "type": "chat/toolCallStart",
                                "toolCallId": "missing-success",
                                "toolName": "code-context-code_read",
                            },
                        ),
                        event(
                            recent,
                            "ahp-chat:incomplete",
                            {
                                "type": "chat/toolCallComplete",
                                "toolCallId": "missing-success",
                                "result": {},
                            },
                        ),
                    ]
                )
            )
            report = MODULE.build_report([trace], hours=24, now=now)
        self.assertEqual(report["schema"], 5)
        self.assertEqual(report["chats_with_events"], 3)
        self.assertEqual(report["tool_calls"], 3)
        self.assertEqual(
            report["groups"]["mcp"]["code-context-code_context"]["successes"], 1
        )
        self.assertEqual(
            report["groups"]["mcp"]["code-context-code_read"]["incomplete"], 1
        )
        self.assertEqual(
            report["groups"]["builtin_search_read"]["view"]["failures"], 1
        )
        self.assertNotIn("rg", report["groups"]["builtin_search_read"])
        self.assertEqual(report["group_totals"]["mcp"]["sessions"], 2)
        self.assertEqual(report["group_totals"]["mcp"]["successes"], 1)

    def test_rejects_non_positive_hours(self):
        with self.assertRaisesRegex(Exception, "greater than zero"):
            MODULE.positive_hours("0")

    def test_results_distinguish_success_from_empty_and_unknown(self):
        for payload, expected in [
            ({"total_matches": 0}, False),
            ({"search": {"total_matches": 2}}, True),
            ({"content": [{"type": "text", "text": '{"paths":["a.rs"],"total_matches":1}'}]}, True),
            ({"content": "matches=0 truncated=false"}, False),
            ({"content": "files=2 truncated=true\na.rs"}, True),
            ({"content": '[{"lines":["source"]}]'}, True),
            ({"content": "omitted"}, None),
            ({"success": True}, None),
            ({"total_matches": True}, None),
        ]:
            with self.subTest(payload=payload):
                self.assertIs(MODULE.result_usefulness(payload), expected)

    def test_session_events_measure_actual_calls_and_nonempty_results(self):
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / "events.jsonl"
            records = []
            for number, (tool, payload) in enumerate([
                ("cctxNativeContext", '{"search":{"total_matches":0}}'),
                ("code-context-code_files", '{"paths":["a.rs"],"total_matches":1}'),
                ("cctxNativeRead", "omitted"),
                ("view", "text"),
                ("tool_search_tool", "catalog"),
                ("code-context-code_workspaces", "[]"),
            ]):
                records.extend([
                    {"type": "tool.execution_start", "timestamp": "2027-01-15T08:00:00Z",
                     "data": {"toolName": tool, "toolCallId": str(number)}},
                    {"type": "tool.execution_complete", "timestamp": "2027-01-15T08:00:01Z",
                     "data": {"toolCallId": str(number), "success": True,
                              "result": {"content": payload}}},
                ])
            trace.write_text("\n".join(map(json.dumps, records)))
            report = MODULE.build_report([trace], 24, now=1_800_000_010)
        self.assertEqual(report["lookup_adoption"], {
            "lookup_calls": 4, "code_context_calls": 3, "selection_percent": 75.0,
            "useful_results": 1, "empty_results": 1, "unmeasured_successes": 1,
            "expected_cache_misses": 0, "cache_load_failures": 0,
        })
        self.assertFalse(report["catalog_exposure"]["verified"])
        self.assertEqual(report["group_totals"]["mcp"]["empty_results"], 0)

    def test_eligibility_never_counts_legitimate_fallback_or_unknown_as_eligible(self):
        now = 1_800_000_010
        coverage = {
            "generated_at_unix": now,
            "roots": [{"registration": {"root": "/repo", "scopes": ["src"], "globs": ["*.ts"]},
                       "files": ["src/a.ts"]}],
        }
        fixtures = [
            ("view", {"path": "/repo/src/a.ts"}, "eligible"),
            ("cctxNativeRead", {"ranges": [{"path": "src/a.ts"}]}, "eligible"),
            ("view", {"path": "/repo/README.md"}, "legitimate_fallback"),
            ("view", {"path": "/repo/.github/test.ts"}, "legitimate_fallback"),
            ("view", {"path": "/tmp/output.txt"}, "legitimate_fallback"),
            ("view", {"path": "/repo.worktrees/one/src/a.ts"}, "legitimate_fallback"),
            ("rg", {"paths": "/repo/src"}, "unknown"),
            ("bash", {"command": "rg needle src"}, "unknown"),
            ("view", {"path": "relative.ts"}, "legitimate_fallback"),
            ("view", {"path": "/repo/../private.ts"}, "unknown"),
        ]
        for tool, arguments, expected in fixtures:
            with self.subTest(tool=tool, arguments=arguments):
                verdict = MODULE.request_eligibility(tool, arguments, "/repo", coverage, now)
                self.assertEqual(verdict["classification"], expected)
        stale = {**coverage, "generated_at_unix": now - 601}
        self.assertEqual(MODULE.request_eligibility("view", {"path": "/repo/src/a.ts"},
                         "/repo", stale, now)["classification"], "unknown")
        preparable = {**coverage, "preparation_candidates": {"roots": [
            {"root": "/repo.worktrees/one", "parent": "/repo"},
        ]}}
        verdict = MODULE.request_eligibility("view", {"path": "/repo.worktrees/one/src/a.ts"},
                                             "/repo", preparable, now)
        self.assertEqual(verdict, {"classification": "unknown",
                                  "reason": "authorized_worktree_requires_preparation"})
        incomplete = {**coverage, "preparation_candidates": {"roots": [], "truncated": True}}
        verdict = MODULE.request_eligibility("view", {"path": "/unseen/worktree/a.ts"},
                                             "/repo", incomplete, now)
        self.assertEqual(verdict["classification"], "unknown")

    def test_first_twenty_eligible_calls_are_scoped_per_session(self):
        calls = []
        for index in range(25):
            calls.append({
                "tool": "cctxNativeRead" if index < 18 else "view",
                "eligibility": {"classification": "eligible", "reason": "admitted_file"},
                "started_at": index, "success": True, "useful": True,
            })
        calls.append({"tool": "view", "eligibility": {"classification": "legitimate_fallback", "reason": "hidden"},
                      "started_at": -1, "success": True})
        sample = MODULE.session_adoption(calls, 20)["first_eligible_sample"]
        self.assertEqual(sample["selection_percent"], 90)
        self.assertEqual(sample["count"], 20)
        self.assertEqual(sample["avoidable_builtin_calls"], 2)

    def test_channel_less_calls_are_isolated_by_trace_file(self):
        now = 1_800_000_000.0
        timestamp = "2027-01-15T08:00:00+00:00"
        with tempfile.TemporaryDirectory() as directory:
            paths = []
            for index, tool in enumerate(("code-context-code_grep", "view")):
                trace = Path(directory) / f"trace-{index}.jsonl"
                trace.write_text(
                    "\n".join(
                        [
                            event(
                                timestamp,
                                "",
                                {
                                    "type": "chat/toolCallStart",
                                    "toolCallId": "same",
                                    "toolName": tool,
                                },
                            ),
                            event(
                                timestamp,
                                "",
                                {
                                    "type": "chat/toolCallComplete",
                                    "toolCallId": "same",
                                    "result": {"success": True},
                                },
                            ),
                        ]
                    )
                )
                paths.append(trace)
            report = MODULE.build_report(paths, hours=24, now=now)
        self.assertEqual(report["tool_calls"], 2)
        self.assertEqual(report["group_totals"]["mcp"]["calls"], 1)
        self.assertEqual(report["group_totals"]["builtin_search_read"]["calls"], 1)

    def test_malformed_event_envelopes_are_ignored(self):
        self.assertIsNone(MODULE.event_timestamp({"_ahpLog": "invalid"}))

    def test_atomic_write_replaces_complete_report(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            output.write_text("old")
            MODULE.atomic_write(output, '{"schema": 2}')
            self.assertEqual(output.read_text(), '{"schema": 2}')


if __name__ == "__main__":
    unittest.main()
