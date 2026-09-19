import io
import json
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import mock_open, patch

from publish import (
    MARKER,
    MAX_PAGES,
    ValidationError,
    comment_body,
    main,
    manifest_ids,
    publish,
    validate_result,
)

SHA = "a" * 40
RUN = "depot-run/attempt-2"


def result(checks=None):
    if checks is None:
        checks = [
            {
                "id": "identity",
                "kind": "chain_id",
                "status": "passed",
                "duration_ms": 1,
                "expected": {"chain": 8453},
                "observed": {"chain": 8453},
                "message": "matched",
                "next_step": "none",
                "samples": 1,
                "rpc_errors": 0,
                "evidence": ["evidence/id.json"],
            }
        ]
    return {
        "schema_version": 1,
        "run_id": RUN,
        "tested_sha": SHA,
        "started_at_unix_ms": 1,
        "scenarios": [
            {
                "id": "smoke",
                "status": "passed",
                "duration_ms": 2,
                "config": {},
                "stages": [],
                "checks": checks,
                "samples": [],
                "forks": [],
                "diagnostics": [],
                "reproduction": "base-acceptance run smoke.toml",
            }
        ],
    }


class FakeApi:
    def __init__(self, responses):
        self.responses = list(responses)
        self.calls = []

    def request(self, method, path, body=None, absolute=False):
        self.calls.append((method, path, body))
        return self.responses.pop(0)


class ValidationTests(unittest.TestCase):
    def test_cleanup_error_overrides_forged_pass_and_explains_failure(self):
        item = result()
        item["scenarios"][0]["stages"] = [
            {
                "id": "cleanup",
                "status": "error",
                "duration_ms": 40,
                "message": "owned resources could not be removed",
            }
        ]
        validated = validate_result(item, {"smoke": ["identity"]}, RUN, SHA)
        body = comment_body(validated, "https://github.com/o/r/pull/1/checks", {})
        self.assertIn("Not passed", body)
        self.assertIn("owned resources could not be removed", body)
        self.assertEqual(validated["scenarios"][0]["checks"][0]["status"], "passed")

    def test_long_fork_observation_is_publishable(self):
        item = result()
        item["scenarios"][0]["samples"] = [
            {
                "endpoint": "builder",
                "elapsed_ms": i * 1000,
                "number": i,
                "timestamp": 100 + i,
                "hash": "0x" + "a" * 64,
            }
            for i in range(600)
        ]
        self.assertEqual(
            validate_result(item, {"smoke": ["identity"]}, RUN, SHA)["scenarios"][0][
                "status"
            ],
            "passed",
        )

    def test_result_breakdown_and_marker(self):
        body = comment_body(
            result(),
            "https://github.com/o/r/pull/1/checks",
            {"run_id": RUN, "attempt": 2, "started_at": "2026-01-01T00:00:00Z"},
        )
        self.assertTrue(body.startswith(MARKER))
        self.assertIn("<details>", body)
        self.assertNotIn("<details open>", body)
        self.assertIn("Expected", body)
        self.assertIn("Observed", body)
        self.assertIn("Reproduction", body)
        self.assertIn("| 1 | 0 | 0 | 0 | 0 |", body)

    def test_failed_scenario_details_are_open(self):
        item = result()
        item["scenarios"][0]["status"] = "failed"
        item["scenarios"][0]["checks"][0]["status"] = "failed"
        body = comment_body(
            item,
            "https://github.com/o/r/pull/1/checks",
            {"run_id": RUN, "attempt": 2, "started_at": "2026-01-01T00:00:00Z"},
        )
        self.assertIn("<details open><summary>smoke — failed</summary>", body)

    def test_hostile_input_is_escaped_without_links_mentions_or_scripts(self):
        item = result()
        check = item["scenarios"][0]["checks"][0]
        check.update(
            message="<script>|`x`\n@team https://evil.invalid/x",
            next_step="[click](https://bad.invalid)",
        )
        body = comment_body(
            item,
            "https://github.com/o/r/pull/1/checks",
            {"run_id": RUN, "attempt": 1, "started_at": "x"},
        )
        self.assertNotIn("<script>", body)
        self.assertNotIn("https://evil", body)
        self.assertNotIn("@team", body)
        self.assertIn("&lt;script&gt;", body)
        self.assertIn("＠team", body)

    def test_empty_missing_and_duplicate_records_rejected(self):
        expected = {"smoke": ["identity"]}
        for bad in ({**result(), "scenarios": []}, result([])):
            with self.assertRaises(ValidationError):
                validate_result(bad, expected, RUN, SHA)
        bad = result()
        bad["scenarios"][0]["checks"].append(dict(bad["scenarios"][0]["checks"][0]))
        with self.assertRaises(ValidationError):
            validate_result(bad, expected, RUN, SHA)

    def test_manifest_ids_and_evidence_paths_are_validated(self):
        manifest = {
            "schema_version": 1,
            "run_id": RUN,
            "tested_sha": SHA,
            "scenarios": [{"id": "smoke", "checks": ["identity"]}],
        }
        self.assertEqual(manifest_ids(manifest, RUN, SHA), {"smoke": ["identity"]})
        bad = result()
        bad["scenarios"][0]["checks"][0]["evidence"] = ["../secret"]
        with self.assertRaises(ValidationError):
            validate_result(bad, {"smoke": ["identity"]}, RUN, SHA)


class PublishTests(unittest.TestCase):
    def args(self, **overrides):
        values = {
            "pr": 1,
            "head": "head",
            "base": "base",
            "run_id": RUN,
            "attempt": 2,
            "started_at": "2026-01-02T00:00:00Z",
            "body": MARKER + "\nbody",
            "bot_login": "depot[bot]",
        }
        values.update(overrides)
        return values

    def test_stale_head_or_base_never_writes(self):
        for current in (
            {"head": {"sha": "new"}, "base": {"sha": "base"}},
            {"head": {"sha": "head"}, "base": {"sha": "new"}},
        ):
            api = FakeApi([current])
            self.assertFalse(publish(api, **self.args()))
            self.assertEqual(len(api.calls), 1)

    def test_rechecks_head_and_base_before_write(self):
        api = FakeApi(
            [
                {"head": {"sha": "head"}, "base": {"sha": "base"}},
                [],
                {"head": {"sha": "new"}, "base": {"sha": "base"}},
            ]
        )
        self.assertFalse(publish(api, **self.args()))
        self.assertEqual(api.calls[-1][0], "GET")

    def test_newer_attempt_and_newer_run_are_not_overwritten(self):
        def owned(meta):
            return [
                {
                    "id": 7,
                    "user": {"login": "depot[bot]", "type": "Bot"},
                    "body": MARKER
                    + "\n<!-- acceptance-results-meta:"
                    + json.dumps(meta)
                    + " -->",
                }
            ]

        current = {"head": {"sha": "head"}, "base": {"sha": "base"}}
        api = FakeApi(
            [
                current,
                owned(
                    {"run_id": RUN, "attempt": 3, "started_at": "2026-01-01T00:00:00Z"}
                ),
            ]
        )
        self.assertFalse(publish(api, **self.args()))
        api = FakeApi(
            [
                current,
                owned(
                    {
                        "run_id": "other",
                        "attempt": 1,
                        "started_at": "2026-01-03T00:00:00Z",
                    }
                ),
            ]
        )
        self.assertFalse(publish(api, **self.args()))

    def test_paginates_updates_exact_owned_bot_and_writes(self):
        page = [
            {
                "id": i,
                "user": {"login": "other[bot]", "type": "Bot"},
                "body": MARKER + " other",
            }
            for i in range(100)
        ]
        meta = {"run_id": RUN, "attempt": 1, "started_at": "2026-01-01T00:00:00Z"}
        owned = {
            "id": 777,
            "user": {"login": "depot[bot]", "type": "Bot"},
            "body": MARKER
            + "\n<!-- acceptance-results-meta:"
            + json.dumps(meta)
            + " -->",
        }
        current = {"head": {"sha": "head"}, "base": {"sha": "base"}}
        api = FakeApi([current, page, [owned], current, {}])
        self.assertTrue(publish(api, **self.args()))
        self.assertEqual(api.calls[-1][:2], ("PATCH", "/issues/comments/777"))

    def test_pagination_cap_fails_instead_of_duplicating(self):
        current = {"head": {"sha": "head"}, "base": {"sha": "base"}}
        page = [{"user": {"type": "User"}, "body": ""}] * 100
        api = FakeApi([current] + [page] * MAX_PAGES)
        with self.assertRaises(ValidationError):
            publish(api, **self.args())
        self.assertFalse(any(call[0] in ("POST", "PATCH") for call in api.calls))

    def test_bounded_body(self):
        item = result()
        item["scenarios"][0]["checks"][0]["message"] = "x" * 2000
        item["scenarios"][0]["checks"] *= 200
        with self.assertRaises(ValidationError):
            comment_body(item, "https://github.com/o/r/pull/1/checks", {})


class CliTests(unittest.TestCase):
    def argv(self, started_at):
        return [
            "publish.py",
            "--event",
            "event.json",
            "--result",
            "result.json",
            "--expected",
            "manifest.json",
            "--run-id",
            RUN,
            "--attempt",
            "2",
            "--started-at",
            started_at,
        ]

    def test_invalid_timestamp_fails_clearly_before_io(self):
        for started_at in (
            "",
            "invalid",
            "2026-02-30T00:00:00Z",
            "2026-01-02T00:00:00",
            "2026-1-2T00:00:00Z",
            "2026-01-02T01:00:00+01:00",
            "2026-01-02T00:00:00.123Z",
        ):
            with (
                self.subTest(started_at=started_at),
                patch("sys.argv", self.argv(started_at)),
                patch("builtins.open") as source,
                patch("publish.Api") as api,
                redirect_stderr(io.StringIO()) as stderr,
                self.assertRaises(SystemExit) as exit_code,
            ):
                main()
            self.assertEqual(exit_code.exception.code, 2)
            self.assertIn("--started-at must be a UTC timestamp", stderr.getvalue())
            self.assertNotIn("Traceback", stderr.getvalue())
            source.assert_not_called()
            api.assert_not_called()

    def test_canonical_timestamp_publishes_with_trusted_ordering(self):
        started_at = "2026-01-02T00:00:00Z"
        current = {"head": {"sha": "head"}, "base": {"sha": "base"}}
        event = {"pull_request": {"number": 1, **current}}
        manifest = {
            "schema_version": 1,
            "run_id": RUN,
            "tested_sha": SHA,
            "scenarios": [{"id": "smoke", "checks": ["identity"]}],
        }
        api = FakeApi([current, [], current, {}])
        with (
            patch("sys.argv", self.argv(started_at)),
            patch("builtins.open", mock_open(read_data=json.dumps(event))),
            patch.dict(
                "os.environ",
                {
                    "GITHUB_REPOSITORY": "base/base",
                    "GITHUB_TOKEN": "test-token",
                    "TESTED_SHA": SHA,
                    "BOT_LOGIN": "depot[bot]",
                },
            ),
            patch("publish.load_json", side_effect=[manifest, result()]),
            patch("publish.Api", return_value=api),
            redirect_stdout(io.StringIO()) as stdout,
        ):
            main()
        self.assertEqual(stdout.getvalue(), "published\n")
        self.assertEqual(api.calls[-1][:2], ("POST", "/issues/1/comments"))
        self.assertIn(f'"started_at":"{started_at}"', api.calls[-1][2]["body"])


if __name__ == "__main__":
    unittest.main()
