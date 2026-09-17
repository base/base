#!/usr/bin/env python3
"""Smoke-check published Base manifests, never snapshot archives.

Run with `just check-snapshot-manifests` (Python 3.10+ and curl required).
This checks discovery and manifest identity, not successful snapshot restoration.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from collections.abc import Callable
from typing import Any
from urllib.parse import urlparse

API_URL = "https://chain.base.org/api/snapshots"
CHAINS = {8453: "mainnet", 84532: "sepolia", 763360: "zeronet"}
API_LIMIT = 1024 * 1024
MANIFEST_LIMIT = 16 * 1024 * 1024


class FetchError(RuntimeError):
    """A bounded HTTP request or JSON decode failed."""


def fetch_json(url: str, limit: int) -> Any:
    """Fetch one JSON endpoint without following redirects."""
    command = [
        "curl",
        "--disable",
        "--silent",
        "--show-error",
        "--fail",
        "--proto",
        "=https",
        "--globoff",
        "--connect-timeout",
        "10",
        "--max-time",
        "30",
        "--max-filesize",
        str(limit),
        "--retry",
        "2",
        "--retry-delay",
        "1",
        "--retry-max-time",
        "40",
        "--retry-connrefused",
        "--write-out",
        "\n%{http_code}",
        "--url",
        url,
    ]
    try:
        result = subprocess.run(command, capture_output=True, check=False, timeout=45)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise FetchError(str(exc)) from exc
    body, separator, status = result.stdout.rpartition(b"\n")
    if result.returncode or not separator or status != b"200":
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        code = status.decode("ascii", errors="replace") if separator else "unknown"
        raise FetchError(
            detail or f"curl failed (HTTP {code}, exit {result.returncode})"
        )
    if len(body) > limit:
        raise FetchError(f"response exceeds {limit} bytes")
    try:
        return json.loads(body)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise FetchError(f"invalid JSON: {exc}") from exc


def numeric(value: Any) -> int | None:
    """Read an unsigned integer represented as a JSON number or string."""
    if isinstance(value, bool):
        return None
    if isinstance(value, str):
        if not re.fullmatch(r"\+?[0-9]+", value):
            return None
        try:
            value = int(value)
        except ValueError:
            return None
    if isinstance(value, int) and 0 <= value <= 2**64 - 1:
        return value
    return None


def select_latest(listing: Any, chain_id: int) -> tuple[int, str] | None:
    """Select the highest-block modular manifest, matching node discovery."""
    if not isinstance(listing, list):
        raise TypeError("snapshot listing is not a JSON array")
    candidates: list[tuple[int, str]] = []
    for entry in listing:
        if not isinstance(entry, dict) or numeric(entry.get("chainId")) != chain_id:
            continue
        url = entry.get("metadataUrl")
        block = numeric(entry.get("block"))
        if isinstance(url, str) and url.endswith("manifest.json") and block is not None:
            candidates.append((block, url))
    return max(candidates, default=None, key=lambda candidate: candidate[0])


def validate_manifest(manifest: Any, chain_id: int, block: int) -> None:
    """Validate discovery identity and the presence of modular components."""
    if not isinstance(manifest, dict):
        raise TypeError("manifest is not a JSON object")
    if numeric(manifest.get("chain_id")) != chain_id:
        raise ValueError(f"manifest chain_id does not match {chain_id}")
    if numeric(manifest.get("block")) != block:
        raise ValueError(f"manifest block does not match discovered block {block}")
    components = manifest.get("components")
    if not isinstance(components, dict) or not components:
        raise ValueError("manifest components is not a nonempty object")


def check(fetch: Callable[[str, int], Any] = fetch_json) -> list[str]:
    """Run all checks and return aggregated errors."""
    try:
        listing = fetch(API_URL, API_LIMIT)
        if not isinstance(listing, list):
            raise TypeError("snapshot listing is not a JSON array")
    except (FetchError, TypeError) as exc:
        return [f"snapshot API failure: {exc}"]

    errors: list[str] = []
    for chain_id, name in CHAINS.items():
        selected = select_latest(listing, chain_id)
        if selected is None:
            errors.append(f"{name} ({chain_id}): no modular snapshot manifest found")
            continue
        block, url = selected
        print(f"{name} ({chain_id}): block {block}, manifest {url}")
        try:
            parsed = urlparse(url)
            if parsed.scheme != "https" or not parsed.hostname:
                raise ValueError("manifest URL is not HTTPS with a valid host")
            validate_manifest(fetch(url, MANIFEST_LIMIT), chain_id, block)
        except (FetchError, TypeError, ValueError) as exc:
            errors.append(f"{name} ({chain_id}): {exc}")
    return errors


def main() -> int:
    """CLI entrypoint."""
    errors = check()
    for error in errors:
        print(f"ERROR: {error}", file=sys.stderr)
    if errors:
        return 1
    print("All published Base snapshot manifests are reachable and valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
