#!/usr/bin/env python3
"""Unit tests for the no-dependency idle CPU profiler."""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "scripts"))
import idle_cpu_profile as monitor


class CpuProfileTests(unittest.TestCase):
    def test_stat_handles_parenthesized_commands_and_field_offsets(self):
        fields = ["S"] + ["0"] * 22
        fields[11], fields[12], fields[19] = "202", "98", "98765"
        parsed = monitor.parse_stat("441 (anvil (nested) worker) " + " ".join(fields))
        self.assertEqual(parsed["cpu_ticks"], 300)
        self.assertEqual(parsed["starttime_ticks"], 98765)

    def test_stat_rejects_truncated_input(self):
        with self.assertRaises(ValueError):
            monitor.parse_stat("441 (anvil) S 0 0")

    def test_context_switch_counters(self):
        self.assertEqual(monitor.parse_context_switches(
            "voluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t9\n"), 129)
        with self.assertRaises(ValueError):
            monitor.parse_context_switches("voluntary_ctxt_switches: 12")

    def test_summary_reports_one_cpu_and_common_thread_switches(self):
        a = {"cpu_ticks": 200, "starttime_ticks": 123, "threads": {"1": 10, "2": 20}}
        b = {"cpu_ticks": 260, "starttime_ticks": 123, "threads": {"1": 30, "3": 40}}
        stats_a = {"rendered_frames": 17, "render_attempts": 18}
        stats_b = {"rendered_frames": 18, "render_attempts": 20}
        result = monitor.summarize(441, a, b, 20.0, 100, stats_a, stats_b)
        self.assertEqual(result["cpu_percent_of_one_vcpu"], 3.0)
        self.assertEqual(result["context_switches_common_threads"], 20)
        self.assertTrue(result["thread_churn_detected"])
        self.assertEqual(result["render_counter_deltas"]["rendered_frames"], 1)

    def test_summary_rejects_pid_reuse(self):
        a = {"cpu_ticks": 100, "starttime_ticks": 123, "threads": {"1": 10}}
        b = {"cpu_ticks": 200, "starttime_ticks": 124, "threads": {"1": 20}}
        with self.assertRaisesRegex(RuntimeError, "PID was reused"):
            monitor.summarize(441, a, b, 10, 100)


if __name__ == "__main__":
    unittest.main()
