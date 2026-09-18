#!/usr/bin/env python3
"""Retain diagnostics, then remove only this acceptance run's labelled resources.

Run after nextest exits, including a killed test process. This is not a daemon:
SIGKILL of the runner itself cannot trigger cleanup. Never prune by name or remove
volumes/images. Run the mocked Docker tests with `cleanup.py test`.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ARTIFACTS = "org.base.glamsterdam.artifacts"
COMPONENT = "org.base.glamsterdam.component"
NETWORK = "org.base.glamsterdam.network"
COMPONENTS = {"setup", "reth", "beacon", "validator"}
COMMAND_TIMEOUT = 30


def docker(*args: str, **kwargs) -> subprocess.CompletedProcess:
    """Bound every Docker operation, including diagnostic collection."""
    return subprocess.run(
        ["docker", *args], timeout=COMMAND_TIMEOUT, check=True,
        **({"stdout": subprocess.PIPE, "stderr": subprocess.PIPE} | kwargs),
    )


def resource_id(value: str) -> str:
    """Accept only full immutable IDs, never names or command options."""
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value):
        raise ValueError(f"invalid Docker resource ID: {value!r}")
    return value


def cleanup(artifacts: Path) -> dict:
    """Fail closed on uncertain ownership or incomplete diagnostics."""
    artifacts = artifacts.resolve(strict=True)
    owner = str((artifacts / "runtime").resolve())
    report = {
        "schema_version": 1, "artifacts_label": owner, "status": "failed",
        "containers": [], "networks": [], "errors": [],
    }
    try:
        ids = sorted(set(docker(
            "ps", "-aq", "--no-trunc", "--filter", f"label={ARTIFACTS}={owner}",
        ).stdout.decode().split()))
        networks = {}
        # Collect all diagnostics before the first destructive command. Separate
        # paths preserve Rust snapshots even when the fallback finds survivors.
        for candidate in ids:
            cid = resource_id(candidate)
            raw = docker("inspect", "--type", "container", cid).stdout
            info, = json.loads(raw)
            labels = info["Config"].get("Labels") or {}
            component = labels.get(COMPONENT)
            if info["Id"] != cid or labels.get(ARTIFACTS) != owner:
                raise ValueError(f"container ownership mismatch: {cid}")
            if component not in COMPONENTS:
                raise ValueError(f"invalid component label on {cid}: {component!r}")
            expected_network = labels.get(NETWORK, "")
            prefix = "glamsterdam-setup" if component == "setup" else "glamsterdam-l1"
            if not re.fullmatch(prefix + r"-[a-z0-9]{8}", expected_network):
                raise ValueError(f"invalid owned network label on {cid}")
            attached = info["NetworkSettings"]["Networks"]
            if set(attached) != {expected_network}:
                raise ValueError(f"unexpected network attachments on {cid}")
            nid = resource_id(attached[expected_network]["NetworkID"])
            if nid in networks and networks[nid] != expected_network:
                raise ValueError(f"conflicting network identity: {nid}")
            networks[nid] = expected_network
            directory = artifacts / "cleanup"
            directory.mkdir(exist_ok=True)
            inspect_path = directory / f"{cid}.inspect.json"
            log_path = directory / f"{cid}.log"
            # Never replace a prior fallback capture either.
            with inspect_path.open("xb") as output:
                output.write(raw)
            with log_path.open("xb") as output:
                docker("logs", "--timestamps", cid, stdout=output, stderr=subprocess.STDOUT)
            report["containers"].append({
                "id": cid, "component": component, "removed": False,
                "inspect": str(inspect_path), "logs": str(log_path),
            })
        for nid, name in sorted(networks.items()):
            raw = docker("network", "inspect", nid).stdout
            info, = json.loads(raw)
            with (artifacts / "cleanup" / f"{nid}.network.json").open("xb") as output:
                output.write(raw)
            if info["Id"] != nid or info["Name"] != name:
                raise ValueError(f"network identity mismatch: {nid}")
            if not set(info.get("Containers", {})).issubset(ids):
                raise ValueError(f"network has unrelated attachments: {nid}")
            report["networks"].append({"id": nid, "name": name, "removed": False})
        for item in report["containers"]:
            # Force is limited to the inspected full container ID. No -v: bind
            # mounts and volumes belong to diagnostics/caller data, not cleanup.
            docker("rm", "--force", item["id"])
            item["removed"] = True
        for item in report["networks"]:
            # No force: a concurrently attached unrelated container must survive.
            docker("network", "rm", item["id"])
            item["removed"] = True
        report["status"] = "passed"
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        report["errors"].append(str(error))
    (artifacts / "cleanup-report.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


class CleanupTests(unittest.TestCase):
    """Mock subprocess to exercise ownership, ordering and failure behavior."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.artifacts = Path(self.temporary.name).resolve()
        self.cid = "a" * 64
        self.other = "b" * 64
        self.nid = "c" * 64
        self.name = "glamsterdam-l1-12345678"
        self.ids = [self.cid]
        self.info = {
            "Id": self.cid,
            "Config": {"Labels": {
                ARTIFACTS: str(self.artifacts / "runtime"), COMPONENT: "reth", NETWORK: self.name,
            }},
            "NetworkSettings": {"Networks": {self.name: {"NetworkID": self.nid}}},
        }
        self.infos = {self.cid: self.info}
        self.network = {"Id": self.nid, "Name": self.name, "Containers": {self.cid: {}}}
        self.calls = []
        self.fail = None
        self.mock = patch("subprocess.run", side_effect=self.run_docker).start()
        self.addCleanup(patch.stopall)

    def run_docker(self, command, **kwargs):
        self.calls.append(command[1:])
        self.assertEqual(kwargs["timeout"], COMMAND_TIMEOUT)
        self.assertTrue(kwargs["check"])
        args = command[1:]
        if args[:len(self.fail or [])] == self.fail:
            raise subprocess.TimeoutExpired(command, COMMAND_TIMEOUT)
        if args[0] == "ps":
            self.assertEqual(args, ["ps", "-aq", "--no-trunc", "--filter",
                                   f"label={ARTIFACTS}={self.artifacts / 'runtime'}"])
            result = "\n".join(self.ids).encode()
        elif args[0] == "inspect":
            result = json.dumps([self.infos[args[-1]]]).encode()
        elif args[0] == "logs":
            kwargs["stdout"].write(b"complete stdout and stderr\n")
            result = b""
        elif args[:2] == ["network", "inspect"]:
            result = json.dumps([self.network]).encode()
        else:
            # Destruction is permitted only after all diagnostic files exist.
            self.assertTrue((self.artifacts / "cleanup" / f"{self.cid}.inspect.json").is_file())
            self.assertEqual((self.artifacts / "cleanup" / f"{self.cid}.log").read_bytes(),
                             b"complete stdout and stderr\n")
            self.assertTrue((self.artifacts / "cleanup" / f"{self.nid}.network.json").is_file())
            result = b""
        return subprocess.CompletedProcess(command, 0, stdout=result, stderr=b"")

    def assert_no_removal(self):
        self.assertFalse(any(args[0] == "rm" or args[:2] == ["network", "rm"]
                             for args in self.calls))

    def test_diagnostics_before_exact_owned_id_removal(self):
        report = cleanup(self.artifacts)
        self.assertEqual(report["status"], "passed")
        self.assertEqual(self.calls[-2:], [["rm", "--force", self.cid], ["network", "rm", self.nid]])
        self.assertTrue(all(item["removed"] for item in report["containers"] + report["networks"]))
        self.assertEqual(json.loads((self.artifacts / "cleanup-report.json").read_text()), report)

    def test_all_diagnostics_precede_removal_and_shared_network_is_removed_once(self):
        self.ids.append(self.other)
        self.infos[self.other] = json.loads(json.dumps(self.info))
        self.infos[self.other]["Id"] = self.other
        self.infos[self.other]["Config"]["Labels"][COMPONENT] = "beacon"
        self.network["Containers"][self.other] = {}
        self.assertEqual(cleanup(self.artifacts)["status"], "passed")
        first_remove = self.calls.index(["rm", "--force", self.cid])
        self.assertLess(self.calls.index(["logs", "--timestamps", self.other]), first_remove)
        self.assertEqual(self.calls[-3:], [["rm", "--force", self.cid],
                                          ["rm", "--force", self.other], ["network", "rm", self.nid]])

    def test_invalid_selected_id_fails_before_inspect(self):
        self.ids = ["--all"]
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assertEqual(len(self.calls), 1)

    def test_network_inspect_identity_must_match(self):
        self.network["Name"] = "caller-network"
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_empty_selection_preserves_rust_snapshots(self):
        self.ids = []
        snapshot = self.artifacts / "reth.log"
        snapshot.write_text("Rust snapshot")
        self.assertEqual(cleanup(self.artifacts)["status"], "passed")
        self.assertEqual(snapshot.read_text(), "Rust snapshot")
        self.assertFalse((self.artifacts / "cleanup").exists())
        self.assertEqual(len(self.calls), 1)

    def test_wrong_owner_is_never_removed(self):
        self.info["Config"]["Labels"][ARTIFACTS] += "-other"
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_malformed_component_is_never_used_as_path(self):
        self.info["Config"]["Labels"][COMPONENT] = "../caller-file"
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assertFalse((self.artifacts / "cleanup").exists())
        self.assert_no_removal()

    def test_caller_network_is_never_removed(self):
        self.info["Config"]["Labels"][NETWORK] = "bridge"
        self.info["NetworkSettings"]["Networks"] = {"bridge": {"NetworkID": self.nid}}
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_unexpected_attachment_is_never_removed(self):
        self.info["NetworkSettings"]["Networks"]["caller-network"] = {"NetworkID": "d" * 64}
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_unrelated_network_member_blocks_removal(self):
        self.network["Containers"][self.other] = {}
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_incomplete_diagnostics_block_removal(self):
        self.fail = ["logs"]
        self.assertEqual(cleanup(self.artifacts)["status"], "failed")
        self.assert_no_removal()

    def test_network_removal_failure_fails_gate(self):
        self.fail = ["network", "rm"]
        report = cleanup(self.artifacts)
        self.assertEqual(report["status"], "failed")
        self.assertTrue(report["containers"][0]["removed"])
        self.assertFalse(report["networks"][0]["removed"])


class RunnerTests(unittest.TestCase):
    """Exercise the runner's post-nextest failure path without client builds."""

    def test_status_precedence_and_cleanup_after_nextest(self):
        tail = Path(__file__).with_name("run.sh").read_text().split("# Preserve failures", 1)[1]
        tail = "# Preserve failures" + tail
        for run, clean, validate, expected in [(0, 0, 0, 0), (7, 1, 1, 7), (0, 1, 0, 1), (0, 0, 9, 9)]:
            with self.subTest(run=run, cleanup=clean, validation=validate):
                with tempfile.TemporaryDirectory() as temporary:
                    script = '''set -euo pipefail
BASE_GLAMSTERDAM_ARTIFACTS="$1"
mode=calldata
mock_nextest() { echo nextest >> "$1/order"; return "$2"; }
python3() {
  case "$1" in
    */cleanup.py) echo cleanup >> "$BASE_GLAMSTERDAM_ARTIFACTS/order"; return CLEAN_STATUS ;;
    */validate-results.py) echo validate >> "$BASE_GLAMSTERDAM_ARTIFACTS/order"; return VALIDATE_STATUS ;;
    *) return 99 ;;
  esac
}
nextest=(mock_nextest "$1" RUN_STATUS)
'''.replace("RUN_STATUS", str(run)).replace("CLEAN_STATUS", str(clean)).replace("VALIDATE_STATUS", str(validate))
                    result = subprocess.run(["bash", "-c", script + tail, "test", temporary],
                                            capture_output=True, timeout=10)
                    self.assertEqual(result.returncode, expected, result.stderr.decode())
                    self.assertEqual((Path(temporary) / "order").read_text(), "nextest\ncleanup\nvalidate\n")


if __name__ == "__main__":
    if sys.argv[1:] == ["test"]:
        unittest.main(argv=[sys.argv[0]])
    elif len(sys.argv) == 2:
        result = cleanup(Path(sys.argv[1]))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result["status"] == "passed" else 1)
    else:
        sys.exit(f"usage: {sys.argv[0]} ARTIFACT_DIRECTORY | test")
