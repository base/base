"""CLI regressions for the PCR0 gate and its privileged comment boundary."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

SCRIPT = Path(__file__).with_name("nitro_pcr0.py")
EXPECTED = "a" * 96
ACTUAL = "b" * 96
SHA = "462a94be183c784b1f0f6d22267eb4852a1e4ffd"
REPO = "base/base"
HEAD_REPO = {
    "id": 1414128345,
    "full_name": "zerozebrazone/base",
}
HEAD_BRANCH = "fix/basectl-l1-block-retry"
FORK_PULLS = (
    "repos/base/base/pulls?state=open&"
    "head=zerozebrazone%3Afix%2Fbasectl-l1-block-retry&per_page=100"
)


class NitroPcr0Tests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.expected = self.root / "pcr0.txt"
        self.measurement = self.root / "measurement.json"
        self.result = self.root / "result.json"
        self.expected.write_text(EXPECTED + "\n")
        self.measurement.write_text(json.dumps({"Measurements": {"PCR0": EXPECTED}}))

    def check(self):
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "check",
                "--expected",
                str(self.expected),
                "--measurement",
                str(self.measurement),
                "--result",
                str(self.result),
                "--head-sha",
                SHA,
                "--run-id",
                "55",
                "--run-attempt",
                "1",
            ],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_match_accepts_only_valid_sha384(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(self.result.read_text())["actual"], EXPECTED)

    def test_mismatch_exits_nonzero_and_preserves_both_values(self):
        self.measurement.write_text(json.dumps({"Measurements": {"PCR0": ACTUAL}}))
        result = self.check()
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(self.result.read_text())
        self.assertEqual((report["expected"], report["actual"]), (EXPECTED, ACTUAL))
        self.assertIn("PCR0 mismatch", result.stderr)

    def test_missing_files_fail_closed(self):
        for path in (self.expected, self.measurement):
            with self.subTest(path=path.name):
                content = path.read_text()
                path.unlink()
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("PCR0 validation failed", result.stderr)
                self.assertFalse(self.result.exists())
                path.write_text(content)

    def test_malformed_expected_fails_closed(self):
        for value in ("", "a" * 95, "g" * 96, EXPECTED + "\n" + EXPECTED):
            with self.subTest(value=value):
                self.expected.write_text(value)
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("expected PCR0", result.stderr)
                self.assertFalse(self.result.exists())

    def test_malformed_measurement_fails_closed(self):
        for value in (
            "",
            "not json",
            "null",
            "[]",
            "{}",
            '{"Measurements": {}}',
            '{"Measurements": {"PCR0": null}}',
            '{"Measurements": {"PCR0": "abc"}}',
            json.dumps({"Measurements": {"PCR0": "g" * 96}}),
            '{"Measurements":{"PCR0":"' + EXPECTED + '","PCR0":"' + ACTUAL + '"}}',
        ):
            with self.subTest(value=value):
                self.measurement.write_text(value)
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("PCR0 validation failed", result.stderr)
                self.assertFalse(self.result.exists())

    def test_nonstandard_json_constants_fail_closed(self):
        for constant in ("NaN", "Infinity", "-Infinity"):
            with self.subTest(constant=constant):
                self.measurement.write_text(
                    f'{{"Measurements": {{"PCR0": "{EXPECTED}"}}, "other": {constant}}}'
                )
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertFalse(self.result.exists())

    def comment_fixture(self, *, associated=True):
        self.root.joinpath("posted.json").unlink(missing_ok=True)
        run = {
            "id": 55,
            "run_attempt": 1,
            "event": "pull_request",
            "status": "completed",
            "conclusion": "failure",
            "head_sha": SHA,
            "head_branch": HEAD_BRANCH,
            "head_repository": HEAD_REPO,
            "path": ".github/workflows/nitro-pcr0.yml",
            "pull_requests": [{"number": 5731}] if associated else [],
        }
        pr = {
            "number": 5731,
            "state": "open",
            "head": {"sha": SHA, "ref": HEAD_BRANCH, "repo": dict(HEAD_REPO)},
            "base": {"repo": {"full_name": REPO}},
        }
        self.event = self.root / "event.json"
        self.event.write_text(
            json.dumps({"workflow_run": run, "repository": {"full_name": REPO}})
        )
        self.report = {
            "version": 1,
            "run_id": 55,
            "run_attempt": 1,
            "head_sha": SHA,
            "expected": EXPECTED,
            "actual": ACTUAL,
        }
        self.responses = {
            "repos/base/base/actions/runs/55": run,
            "repos/base/base/pulls/5731": pr,
            # GitHub returns no base commit association for this open fork PR.
            "repos/base/base/commits/" + SHA + "/pulls?per_page=100": [],
            FORK_PULLS: [pr],
            "repos/base/base/actions/runs/55/artifacts?per_page=100": {
                "artifacts": [
                    {
                        "id": 88,
                        "name": "nitro-pcr0-1",
                        "expired": False,
                        "size_in_bytes": 1000,
                    }
                ]
            },
        }
        # gh is an external API client; this double keeps privileged-path tests offline.
        fake = self.root / "gh"
        fake.write_text(
            "#!"
            + sys.executable
            + "\n"
            + """
import json, os, pathlib, sys
root = pathlib.Path(os.environ["FAKE_GH_ROOT"])
endpoint = sys.argv[2]
if "--method" in sys.argv:
    if os.environ.get("FAIL_COMMENT") == "1":
        print("HTTP 403: forbidden", file=sys.stderr)
        sys.exit(1)
    root.joinpath("posted.json").write_text(sys.stdin.read())
    root.joinpath("posted-endpoint.txt").write_text(endpoint)
    print("{}")
elif endpoint.endswith("/zip"):
    sys.stdout.buffer.write(root.joinpath(endpoint.split("/")[-2] + ".zip").read_bytes())
else:
    print(json.dumps(json.loads(root.joinpath("responses.json").read_text())[endpoint]))
"""
        )
        fake.chmod(0o755)

    def comment(self, *, fail=False, entries=None):
        self.root.joinpath("responses.json").write_text(json.dumps(self.responses))
        with zipfile.ZipFile(self.root / "88.zip", "w") as archive:
            for name, content in (
                entries or {"result.json": json.dumps(self.report)}
            ).items():
                archive.writestr(name, content)
        env = dict(
            os.environ,
            PATH=str(self.root) + os.pathsep + os.environ["PATH"],
            FAKE_GH_ROOT=str(self.root),
            FAIL_COMMENT="1" if fail else "0",
        )
        return subprocess.run(
            [sys.executable, str(SCRIPT), "comment", "--event", str(self.event)],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )

    def test_fork_mismatch_comment_uses_api_pr_and_renders_update(self):
        self.comment_fixture(associated=False)
        result = self.comment()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.root.joinpath("posted.json").exists())
        self.assertEqual(
            self.root.joinpath("posted-endpoint.txt").read_text(),
            "repos/base/base/issues/5731/comments",
        )
        body = json.loads(self.root.joinpath("posted.json").read_text())["body"]
        self.assertIn("Committed expected", body)
        self.assertIn(EXPECTED, body)
        self.assertIn("Measured actual", body)
        self.assertIn(ACTUAL, body)
        self.assertIn("etc/docker/nitro-enclave/pcr0.txt", body)
        self.assertIn("/actions/runs/55", body)
        self.assertIn(SHA, body)

    def test_fork_head_query_encodes_branch_characters(self):
        self.comment_fixture(associated=False)
        branch = "feature/a+b&c#d"
        self.responses["repos/base/base/actions/runs/55"]["head_branch"] = branch
        self.responses["repos/base/base/pulls/5731"]["head"]["ref"] = branch
        self.responses[
            "repos/base/base/pulls?state=open&"
            "head=zerozebrazone%3Afeature%2Fa%2Bb%26c%23d&per_page=100"
        ] = self.responses.pop(FORK_PULLS)
        result = self.comment()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.root.joinpath("posted.json").exists())

    def test_pr_identity_must_match_run_for_both_association_paths(self):
        for associated in (True, False):
            for mismatch in ("sha", "ref", "head_repo_name", "head_repo_id", "base"):
                with self.subTest(associated=associated, mismatch=mismatch):
                    self.comment_fixture(associated=associated)
                    pr = self.responses["repos/base/base/pulls/5731"]
                    if mismatch == "sha":
                        pr["head"]["sha"] = "d" * 40
                    elif mismatch == "ref":
                        pr["head"]["ref"] = "other"
                    elif mismatch == "head_repo_name":
                        pr["head"]["repo"]["full_name"] = "other/base"
                    elif mismatch == "head_repo_id":
                        pr["head"]["repo"]["id"] += 1
                    else:
                        pr["base"]["repo"]["full_name"] = "other/base"
                    self.comment()
                    self.assertFalse(self.root.joinpath("posted.json").exists())

    def test_rerun_ignores_older_artifact_and_comments_current_attempt(self):
        self.comment_fixture()
        run = self.responses["repos/base/base/actions/runs/55"]
        run["run_attempt"] = 2
        self.event.write_text(
            json.dumps({"workflow_run": run, "repository": {"full_name": REPO}})
        )
        self.report["run_attempt"] = 2
        self.responses["repos/base/base/actions/runs/55/artifacts?per_page=100"][
            "artifacts"
        ] = [
            {
                "id": 87,
                "name": "nitro-pcr0-1",
                "expired": True,
                "size_in_bytes": 1000,
            },
            {
                "id": 88,
                "name": "nitro-pcr0-2",
                "expired": False,
                "size_in_bytes": 1000,
            },
        ]
        result = self.comment()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.root.joinpath("posted.json").exists())

    def test_comment_api_failure_exits_nonzero(self):
        self.comment_fixture()
        result = self.comment(fail=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("403", result.stderr)
        self.assertFalse(self.root.joinpath("posted.json").exists())

    def test_stale_pr_or_run_attempt_does_not_comment(self):
        for stale in ("head", "attempt", "closed"):
            with self.subTest(stale=stale):
                self.comment_fixture()
                if stale == "head":
                    self.responses["repos/base/base/pulls/5731"]["head"]["sha"] = (
                        "d" * 40
                    )
                elif stale == "attempt":
                    self.responses["repos/base/base/actions/runs/55"]["run_attempt"] = 2
                else:
                    self.responses["repos/base/base/pulls/5731"]["state"] = "closed"
                result = self.comment()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(self.root.joinpath("posted.json").exists())

    def test_unbound_or_malformed_artifacts_never_comment(self):
        for field, value in (
            ("run_id", 56),
            ("run_attempt", 2),
            ("head_sha", "d" * 40),
            ("actual", "`$(touch injected)`"),
            ("expected", ""),
            ("version", True),
            ("extra", "unexpected"),
        ):
            with self.subTest(field=field):
                self.comment_fixture()
                self.report[field] = value
                result = self.comment()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(self.root.joinpath("posted.json").exists())

    def test_zip_paths_and_oversized_data_rejected_without_extraction(self):
        for entries in (
            {"../result.json": "{}"},
            {"result.json": "x" * 16385},
            {"result.json": "{}", "execute.sh": "touch injected"},
        ):
            with self.subTest(entries=list(entries)):
                self.comment_fixture()
                result = self.comment(entries=entries)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(self.root.joinpath("posted.json").exists())
                self.assertFalse(self.root.joinpath("execute.sh").exists())

    def test_ambiguous_fork_association_rejected(self):
        self.comment_fixture(associated=False)
        candidates = self.responses[FORK_PULLS]
        candidates.append(dict(candidates[0], number=8))
        result = self.comment()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse(self.root.joinpath("posted.json").exists())

    def test_matching_artifact_does_not_post_mismatch(self):
        self.comment_fixture()
        self.report["actual"] = EXPECTED
        result = self.comment()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.root.joinpath("posted.json").exists())


if __name__ == "__main__":
    unittest.main()
