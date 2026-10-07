#!/usr/bin/env python3
"""Exercise benchmark comment posting with scripted GitHub CLI responses."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("post_pr_comment.sh")
MARKER = "<!-- iai-bench-results -->"


class PostPrCommentTests(unittest.TestCase):
    def run_script(self, responses):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "responses.json").write_text(json.dumps(responses))
            gh = root / "gh"
            gh.write_text(
                f"#!{sys.executable}\n"
                "import json, os, pathlib, sys\n"
                "root = pathlib.Path(os.environ['FAKE_GH_ROOT'])\n"
                "log = root / 'calls.jsonl'\n"
                "calls = log.read_text().splitlines() if log.exists() else []\n"
                "with log.open('a') as output:\n"
                "    output.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "responses = json.loads((root / 'responses.json').read_text())\n"
                "if len(calls) >= len(responses):\n"
                "    sys.exit('unexpected extra API request')\n"
                "response = responses[len(calls)]\n"
                "sys.stdout.write(response.get('stdout', ''))\n"
                "sys.stderr.write(response.get('stderr', ''))\n"
                "sys.exit(response.get('code', 0))\n"
            )
            gh.chmod(0o755)
            sleep = root / "sleep"
            sleep.write_text("#!/bin/sh\nexit 0\n")
            sleep.chmod(0o755)
            body = root / "comment body.md"
            body.write_text(f"{MARKER}\nBenchmarks: `example`\n")
            environment = {
                **os.environ,
                "PATH": f"{root}:{os.environ['PATH']}",
                "FAKE_GH_ROOT": str(root),
                "GH_TOKEN": "test-token",
                "REPO": "base/base",
                "PR_NUMBER": "123",
            }
            result = subprocess.run(
                ["bash", str(SCRIPT), str(body)],
                env=environment,
                text=True,
                capture_output=True,
                timeout=10,
            )
            calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
            return result, calls

    def test_creates_comment_when_lookup_is_empty_array(self):
        result, calls = self.run_script([{"stdout": "[]"}, {"stdout": '{"id":42}'}])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[1][:4], ["api", "repos/base/base/issues/123/comments", "-X", "POST"])
        self.assertEqual(calls[1][4], "-F")
        self.assertTrue(calls[1][5].startswith("body=@"))
        self.assertTrue(calls[1][5].endswith("comment body.md"))

    def test_updates_first_matching_comment(self):
        comments = [
            {"id": 1, "body": None},
            {"id": 2, "body": "unrelated"},
            {"id": 3, "body": MARKER + " old"},
            {"id": 4, "body": MARKER + " another"},
        ]
        result, calls = self.run_script([
            {"stdout": json.dumps(comments)}, {"stdout": '{"id":3}'},
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[1][:4], ["api", "repos/base/base/issues/comments/3", "-X", "PATCH"])

    def test_retries_empty_and_truncated_lookup_before_writing(self):
        result, calls = self.run_script([
            {"stdout": ""}, {"stdout": '[{"id":'},
            {"stdout": "[]"}, {"stdout": '{"id":42}'},
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 4)
        self.assertTrue(all(call == ["api", "repos/base/base/issues/123/comments"] for call in calls[:3]))
        self.assertIn("attempt 2/3", result.stderr)

    def test_retries_failed_lookup_request(self):
        result, calls = self.run_script([
            {"code": 1, "stderr": "HTTP 502"},
            {"stdout": "[]"}, {"stdout": '{"id":42}'},
        ])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 3)
        self.assertIn("HTTP 502", result.stderr)

    def test_exhausted_invalid_lookup_never_writes(self):
        for response in ["", "[", "{}", "[] []", '[{"id":"unsafe","body":"x"}]']:
            with self.subTest(response=response):
                result, calls = self.run_script([{"stdout": response}] * 3)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 3)
                self.assertTrue(all("-X" not in call for call in calls))
                self.assertIn("No comment was written", result.stderr)
                self.assertIn("GH_TOKEN permissions", result.stderr)

    def test_invalid_write_response_is_not_retried(self):
        for response in ["", '{"id":', "[]", '{}', '{"id":42} {"id":43}']:
            with self.subTest(response=response):
                result, calls = self.run_script([{"stdout": "[]"}, {"stdout": response}])
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(calls), 2)
                self.assertIn("comment may have been written", result.stderr)
                self.assertIn("Check the PR", result.stderr)

    def test_failed_write_is_not_retried(self):
        result, calls = self.run_script([
            {"stdout": "[]"}, {"code": 1, "stderr": "connection reset"},
        ])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls), 2)
        self.assertIn("write outcome is unknown", result.stderr)
        self.assertIn("connection reset", result.stderr)


if __name__ == "__main__":
    unittest.main()
