#!/usr/bin/env python3
"""Fail closed on missing, skipped, retried, or incomplete acceptance results.

This checks artifact completeness, not Ethereum consensus. Rust scenarios own the
observable assertions. Run the colocated tests with `validate-results.py test`.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

CASES = {
    "calldata": "glamsterdam::glamsterdam_calldata",
    "blob": "glamsterdam::glamsterdam_blob",
}
REQUIRED_FILES = (
    "provenance.json", "source-before-build.json", "client-pins.json", "run.log",
    "build-commands.log", "test-command.log", "nextest.toml", "cargo-metadata.json",
    "nextest-binaries.json", "cleanup-report.json",
    "rpc-diagnostics.json", "reth.log", "reth.json", "beacon.log", "beacon.json",
    "validator.log", "validator.json", "configs/l1-genesis.json",
    "configs/beacon.yaml", "configs/l2-genesis.json", "configs/rollup.json",
)


def validate_junit(path: Path, case: str) -> None:
    """Require exactly one successful execution of the selected case."""
    root = ET.parse(path).getroot()
    if root.tag not in {"testsuites", "testsuite"}:
        raise ValueError("JUnit root must be testsuites or testsuite")
    cases = list(root.iter("testcase"))
    expected = ("base-system-tests::acceptance", case)
    identities = [(item.get("classname"), item.get("name")) for item in cases]
    if identities != [expected]:
        raise ValueError(f"expected exactly one execution of {expected}; found {identities}")
    forbidden = {"skipped", "failure", "error", "flakyFailure", "flakyError", "rerunFailure", "rerunError"}
    for element in root.iter():
        if element.tag in forbidden:
            raise ValueError(f"JUnit contains {element.tag}; acceptance permits no skips or retries")
        if element.tag in {"testsuite", "testsuites"}:
            for attribute in ("failures", "errors", "skipped", "disabled"):
                if int(element.get(attribute, "0")) != 0:
                    raise ValueError(f"JUnit {attribute} count must be zero")
            if "tests" in element.attrib and int(element.get("tests")) != 1:
                raise ValueError("JUnit must report exactly one test")


def validate_evidence(path: Path, case: str) -> None:
    """Require successful scenario evidence and all four assertion groups."""
    evidence = json.loads(path.read_text())
    if (
        not isinstance(evidence, dict)
        or type(evidence.get("schema_version")) is not int
        or evidence["schema_version"] != 1
    ):
        raise ValueError("unsupported evidence schema_version (expected 1)")
    if evidence.get("case") != case or evidence.get("status") != "passed":
        raise ValueError(f"evidence must record a passed {case}")
    for name in ("activation", "l2_rules"):
        if not isinstance(evidence.get(name), dict) or not evidence[name]:
            raise ValueError(f"missing {name} evidence")
    for name in ("transfers", "batches", "safe_blocks"):
        group = evidence.get(name)
        if not isinstance(group, dict):
            raise ValueError(f"missing {name} evidence")
        for phase in ("pre", "post"):
            if not isinstance(group.get(phase), dict) or not group[phase]:
                raise ValueError(f"missing {name}.{phase} evidence")


def validate(directory: Path, mode: str) -> None:
    """Validate a single DA case's machine-readable results."""
    case = CASES[mode]
    validate_junit(directory / "test-results.xml", case)
    validate_evidence(directory / "evidence.json", case)
    for name in REQUIRED_FILES:
        if not (directory / name).is_file() or (directory / name).stat().st_size == 0:
            raise ValueError(f"missing or empty required artifact: {name}")
    # An empty patch is correct for a clean committed source tree.
    if not (directory / "source.patch").is_file():
        raise ValueError("missing source.patch")
    cleanup = json.loads((directory / "cleanup-report.json").read_text())
    if (
        not isinstance(cleanup, dict)
        or type(cleanup.get("schema_version")) is not int
        or cleanup["schema_version"] != 1
        or cleanup.get("status") != "passed"
    ):
        raise ValueError("cleanup did not complete successfully")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=CASES)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    try:
        validate(args.directory, args.mode)
    except (OSError, ValueError, ET.ParseError) as error:
        print(f"invalid acceptance results: {error}", file=sys.stderr)
        return 1
    print(f"complete acceptance results: {CASES[args.mode]}")
    return 0


class ResultValidationTests(unittest.TestCase):
    """Exercise false-green reports and valid reports through the validator."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.junit = self.directory / "test-results.xml"
        self.evidence = self.directory / "evidence.json"
        self.write_junit()
        # Values are deliberately opaque: consensus correctness belongs to Rust.
        self.record = {
            "schema_version": 1,
            "case": CASES["calldata"],
            "status": "passed",
            "activation": {"headers": ["pre", "post"]},
            "l2_rules": {"amsterdam_configured": False},
            **{
                name: {"pre": {"hash": "pre"}, "post": {"hash": "post"}}
                for name in ("transfers", "batches", "safe_blocks")
            },
        }
        self.write_evidence()
        for name in REQUIRED_FILES:
            path = self.directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("artifact content is checked by its producer")
        (self.directory / "source.patch").touch()
        (self.directory / "cleanup-report.json").write_text(
            json.dumps({"schema_version": 1, "status": "passed"})
        )

    def write_junit(self, body: str = "", count: int = 1, name: str | None = None) -> None:
        case = name or CASES["calldata"]
        cases = "".join(
            f'<testcase classname="base-system-tests::acceptance" name="{case}">{body}</testcase>'
            for _ in range(count)
        )
        self.junit.write_text(
            f'<testsuites tests="{count}"><testsuite tests="{count}">'
            f'{cases}</testsuite></testsuites>'
        )

    def write_evidence(self) -> None:
        self.evidence.write_text(json.dumps(self.record))

    def test_complete_results(self) -> None:
        validate(self.directory, "calldata")

    def test_blob_results_are_independently_selectable(self) -> None:
        self.write_junit(name=CASES["blob"])
        self.record["case"] = CASES["blob"]
        self.write_evidence()
        validate(self.directory, "blob")
        with self.assertRaises(ValueError):
            validate(self.directory, "calldata")

    def test_missing_results(self) -> None:
        for path in (self.junit, self.evidence):
            with self.subTest(path=path):
                saved = path.read_bytes()
                path.unlink()
                with self.assertRaises(OSError):
                    validate(self.directory, "calldata")
                path.write_bytes(saved)

    def test_missing_or_empty_artifacts(self) -> None:
        for name in (*REQUIRED_FILES, "source.patch"):
            path = self.directory / name
            saved = path.read_bytes()
            with self.subTest(name=name, missing=True):
                path.unlink()
                with self.assertRaisesRegex(ValueError, "missing"):
                    validate(self.directory, "calldata")
            if name != "source.patch":
                with self.subTest(name=name, empty=True):
                    path.touch()
                    with self.assertRaisesRegex(ValueError, "empty"):
                        validate(self.directory, "calldata")
            path.write_bytes(saved)

    def test_failed_or_malformed_cleanup_is_not_a_pass(self) -> None:
        for report in (None, {}, {"schema_version": 1, "status": "failed"},
                       {"schema_version": True, "status": "passed"}):
            with self.subTest(report=report):
                (self.directory / "cleanup-report.json").write_text(json.dumps(report))
                with self.assertRaisesRegex(ValueError, "cleanup"):
                    validate(self.directory, "calldata")

    def test_zero_or_duplicate_cases(self) -> None:
        for count in (0, 2):
            with self.subTest(count=count):
                self.write_junit(count=count)
                with self.assertRaises(ValueError):
                    validate(self.directory, "calldata")

    def test_wrong_case(self) -> None:
        self.write_junit(name="smoke_test")
        with self.assertRaises(ValueError):
            validate(self.directory, "calldata")

    def test_wrong_binary(self) -> None:
        self.junit.write_text(
            self.junit.read_text().replace("base-system-tests::acceptance", "other::acceptance")
        )
        with self.assertRaises(ValueError):
            validate(self.directory, "calldata")

    def test_cli_fails_on_missing_results(self) -> None:
        result = subprocess.run(
            [sys.executable, __file__, "blob", str(self.directory / "missing")],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("invalid acceptance results", result.stderr)

    def test_skips_failures_and_retries(self) -> None:
        for tag in ("skipped", "failure", "error", "flakyFailure", "flakyError", "rerunFailure", "rerunError"):
            with self.subTest(tag=tag):
                self.write_junit(body=f"<{tag}/>")
                with self.assertRaises(ValueError):
                    validate(self.directory, "calldata")

    def test_inconsistent_summary(self) -> None:
        for attribute in ("tests", "failures", "errors", "skipped", "disabled"):
            with self.subTest(attribute=attribute):
                self.junit.write_text(
                    f'<testsuite {attribute}="2"><testcase '
                    f'classname="base-system-tests::acceptance" '
                    f'name="{CASES["calldata"]}"/></testsuite>'
                )
                with self.assertRaises(ValueError):
                    validate(self.directory, "calldata")

    def test_missing_evidence_groups(self) -> None:
        for name in ("activation", "transfers", "batches", "safe_blocks", "l2_rules"):
            with self.subTest(name=name):
                saved = self.record.pop(name)
                self.write_evidence()
                with self.assertRaises(ValueError):
                    validate(self.directory, "calldata")
                self.record[name] = saved

    def test_missing_pre_or_post_evidence(self) -> None:
        for name in ("transfers", "batches", "safe_blocks"):
            for phase in ("pre", "post"):
                with self.subTest(name=name, phase=phase):
                    saved = self.record[name].pop(phase)
                    self.write_evidence()
                    with self.assertRaises(ValueError):
                        validate(self.directory, "calldata")
                    self.record[name][phase] = saved

    def test_incomplete_or_wrong_evidence(self) -> None:
        for key, value in (
            ("status", "failed"), ("case", CASES["blob"]),
            ("schema_version", 2), ("schema_version", True),
        ):
            with self.subTest(key=key):
                saved = self.record[key]
                self.record[key] = value
                self.write_evidence()
                with self.assertRaises(ValueError):
                    validate(self.directory, "calldata")
                self.record[key] = saved

    def test_malformed_results(self) -> None:
        self.junit.write_text("not XML")
        with self.assertRaises(ET.ParseError):
            validate(self.directory, "calldata")
        self.write_junit()
        self.evidence.write_text("not JSON")
        with self.assertRaises(ValueError):
            validate(self.directory, "calldata")


if __name__ == "__main__":
    if sys.argv[1:] == ["test"]:
        unittest.main(argv=[sys.argv[0]])
    else:
        sys.exit(main())
