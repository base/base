"""Tests for check-snapshot-manifests.py."""

from __future__ import annotations

import importlib.util
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("check-snapshot-manifests.py")
SPEC = importlib.util.spec_from_file_location("snapshot_manifests", SCRIPT)
assert SPEC and SPEC.loader
snapshot = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snapshot)


class SnapshotManifestTests(unittest.TestCase):
    def entry(self, chain_id, block, url=None):
        return {
            "chainId": chain_id,
            "block": block,
            "metadataUrl": url
            or f"https://snapshots.example/{chain_id}/{block}/manifest.json",
        }

    def manifest(self, chain_id, block):
        return {"chain_id": str(chain_id), "block": block, "components": {"state": {}}}

    def test_selects_latest_from_unordered_numeric_strings(self):
        listing = [
            self.entry("8453", "101"),
            self.entry(8453, 9),
            self.entry("8453", "20"),
            self.entry(84532, 900),
            self.entry(8453, "invalid"),
            None,
        ]
        self.assertEqual(
            snapshot.select_latest(listing, 8453),
            (101, "https://snapshots.example/8453/101/manifest.json"),
        )

    def test_numeric_matches_unsigned_chain_and_block_fields(self):
        for value in [True, False, -1, 1.5, " 12", "1_2", "²", "-1", 2**64]:
            with self.subTest(value=value):
                self.assertIsNone(snapshot.numeric(value))
        self.assertEqual(snapshot.numeric("+12"), 12)
        self.assertEqual(snapshot.numeric(str(2**64 - 1)), 2**64 - 1)

    def test_skips_legacy_nonmodular_entries(self):
        listing = [
            self.entry(8453, 20, "https://example/archive.tar.zst"),
            self.entry(8453, 10),
        ]
        self.assertEqual(snapshot.select_latest(listing, 8453)[0], 10)

    def test_no_matching_chain(self):
        self.assertIsNone(snapshot.select_latest([self.entry(84532, 1)], 8453))

    def test_malformed_listing_is_clear_api_failure(self):
        errors = snapshot.check(lambda _url, _limit: {"entries": []})
        self.assertEqual(
            errors, ["snapshot API failure: snapshot listing is not a JSON array"]
        )

    def test_validates_manifest_shape_identity_and_components(self):
        bad = [[], {"chain_id": 1, "block": 2, "components": {}}]
        with self.assertRaisesRegex(TypeError, "JSON object"):
            snapshot.validate_manifest(bad[0], 1, 2)
        with self.assertRaisesRegex(ValueError, "nonempty"):
            snapshot.validate_manifest(bad[1], 1, 2)
        with self.assertRaisesRegex(ValueError, "chain_id"):
            snapshot.validate_manifest(self.manifest(2, 3), 1, 3)
        with self.assertRaisesRegex(ValueError, "block"):
            snapshot.validate_manifest(self.manifest(1, 3), 1, 4)

    def test_request_failure_is_isolated_and_no_archives_are_requested(self):
        listing = [
            self.entry(chain, index + 10)
            for index, chain in enumerate([8453, 84532, 763360])
        ]
        manifests = {
            entry["metadataUrl"]: self.manifest(entry["chainId"], entry["block"])
            for entry in listing
        }
        failed_url = listing[1]["metadataUrl"]
        calls = []

        def fetch(url, limit):
            calls.append((url, limit))
            if url == snapshot.API_URL:
                return listing
            if url == failed_url:
                raise snapshot.FetchError("unreachable")
            return manifests[url]

        errors = snapshot.check(fetch)
        self.assertEqual(errors, ["sepolia (84532): unreachable"])
        self.assertEqual(
            [url for url, _limit in calls],
            [snapshot.API_URL] + [e["metadataUrl"] for e in listing],
        )

    def test_checks_all_published_chains_without_fetching_components(self):
        listing = [self.entry(8453, 12), self.entry(84532, 34), self.entry(763360, 56)]
        calls = []

        def fetch(url, _limit):
            calls.append(url)
            if url == snapshot.API_URL:
                return listing
            entry = next(item for item in listing if item["metadataUrl"] == url)
            manifest = self.manifest(entry["chainId"], entry["block"])
            manifest["components"]["state"] = {"url": "https://example/state.tar.zst"}
            return manifest

        self.assertEqual(snapshot.check(fetch), [])
        self.assertEqual(
            calls, [snapshot.API_URL] + [e["metadataUrl"] for e in listing]
        )

    def test_reports_every_missing_chain(self):
        self.assertEqual(
            snapshot.check(lambda *_: []),
            [
                "mainnet (8453): no modular snapshot manifest found",
                "sepolia (84532): no modular snapshot manifest found",
                "zeronet (763360): no modular snapshot manifest found",
            ],
        )

    def test_non_https_manifest_is_rejected_without_request(self):
        listing = [self.entry(chain, 1) for chain in snapshot.CHAINS]
        listing[0]["metadataUrl"] = "http://example/manifest.json"
        calls = []

        def fetch(url, _limit):
            calls.append(url)
            if url == snapshot.API_URL:
                return listing
            entry = next(item for item in listing if item["metadataUrl"] == url)
            return self.manifest(entry["chainId"], entry["block"])

        errors = snapshot.check(fetch)
        self.assertIn("not HTTPS", errors[0])
        self.assertNotIn(listing[0]["metadataUrl"], calls)

    def test_fetch_requires_successful_bounded_json_response(self):
        cases = [
            (0, b'{"ok":true}\n200', 11, None),
            (0, b'{"ok":true}\n200', 10, "exceeds"),
            (0, b"{}\n302", 20, "HTTP 302"),
            (22, b"{}\n404", 20, "HTTP 404"),
            (0, b"not json\n200", 20, "invalid JSON"),
        ]
        for code, body, limit, error in cases:
            with self.subTest(code=code, body=body, limit=limit):
                result = subprocess.CompletedProcess([], code, body, b"")
                with patch.object(snapshot.subprocess, "run", return_value=result):
                    if error:
                        with self.assertRaisesRegex(snapshot.FetchError, error):
                            snapshot.fetch_json("https://example/manifest.json", limit)
                    else:
                        self.assertEqual(
                            snapshot.fetch_json("https://example/manifest.json", limit),
                            {"ok": True},
                        )

    def test_request_deadline_is_reported_as_api_failure(self):
        with patch.object(
            snapshot.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired("curl", 45),
        ):
            errors = snapshot.check()
        self.assertEqual(len(errors), 1)
        self.assertIn("snapshot API failure", errors[0])
        self.assertIn("timed out", errors[0])


if __name__ == "__main__":
    unittest.main()
