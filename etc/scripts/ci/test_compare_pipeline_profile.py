"""Behavioral checks for the performance acceptance gate; fixtures are synthetic, not benchmark evidence."""
import json
import pathlib
import tempfile
import unittest

from compare_pipeline_profile import ACTIVE, WALL, WORKLOADS, compare


class PipelineComparisonTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.baseline = pathlib.Path(self.directory.name) / "baseline"
        self.candidate = pathlib.Path(self.directory.name) / "candidate"
        self.baseline.mkdir()
        self.candidate.mkdir()
        for repetition in range(1, 6):
            for workload in WORKLOADS:
                name = f"{repetition:02}-{workload}"
                for directory, active in ((self.baseline, 0.1), (self.candidate, 0.07)):
                    sample = {"transactions": 1025, "gas_used": 21000,
                              "driver_wall_seconds": 1.0, "published_flashblocks": 6,
                              "observations": {ACTIVE: 1, WALL: 1},
                              "timings": {ACTIVE: [active], WALL: [1.0]}}
                    (directory / f"{name}.json").write_text(json.dumps([sample] * 10))
                    (directory / f"{name}.log").write_text("100000 maximum resident set size\n")

    def change_candidate(self, field, value):
        for path in self.candidate.glob("[0-9][0-9]-*.json"):
            samples = json.loads(path.read_text())
            for sample in samples:
                if field == ACTIVE:
                    sample["timings"][ACTIVE] = [value]
                else:
                    sample[field] = value
            path.write_text(json.dumps(samples))

    def test_accepts_repeated_verified_improvement(self):
        result = compare(self.baseline, self.candidate)
        self.assertTrue(result["passed"])
        self.assertAlmostEqual(result["reduction_percent"], 30)
        self.assertGreaterEqual(result["paired_95_percent_ci"][0], 15)

    def test_rejects_improvement_below_target(self):
        self.change_candidate(ACTIVE, 0.09)
        self.assertFalse(compare(self.baseline, self.candidate)["passed"])

    def test_rejects_inclusion_and_publication_loss(self):
        for field, value in (("transactions", 1024), ("published_flashblocks", 5)):
            with self.subTest(field=field):
                self.change_candidate(field, value)
                with self.assertRaises(ValueError):
                    compare(self.baseline, self.candidate)
                self.change_candidate(field, 1025 if field == "transactions" else 6)

    def test_rejects_changed_execution_work(self):
        self.change_candidate("gas_used", 21001)
        with self.assertRaises(ValueError):
            compare(self.baseline, self.candidate)

    def test_rejects_invalid_timings(self):
        for value in (float("nan"), float("inf"), -1, 0, 2):
            with self.subTest(value=value):
                self.change_candidate(ACTIVE, value)
                with self.assertRaises(ValueError):
                    compare(self.baseline, self.candidate)

    def test_rejects_wall_latency_regression(self):
        self.change_candidate("driver_wall_seconds", 1.06)
        result = compare(self.baseline, self.candidate)
        self.assertFalse(result["passed"])
        self.assertTrue(any("driver_p95_ms" in failure for failure in result["failures"]))

    def test_rejects_insufficient_repetitions(self):
        for directory in (self.baseline, self.candidate):
            for path in directory.glob("05-*.json"):
                path.unlink()
        with self.assertRaises(ValueError):
            compare(self.baseline, self.candidate)


if __name__ == "__main__":
    unittest.main()
