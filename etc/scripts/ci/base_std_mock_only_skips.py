#!/usr/bin/env python3
"""Count the skipped base-std fork tests that only run against base-std's Solidity mocks.

base-std skips a test in fork mode by opening it with `vm.skip(livePrecompiles)` when it can
only run against the mocks, for example because it writes precompile storage with `vm.store`.
Those skips are expected on every fork, so the fork-test summary reports them apart from other
skips, such as env-gated tests, which should stay visible.

Usage: base_std_mock_only_skips.py <forge-output> <base-std-dir>
Prints the number of `[SKIP]` results in the forge output whose test is mock-only.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# A test function whose first statement, after any line comments, is `vm.skip(livePrecompiles)`
# with or without a reason.
_MOCK_ONLY = re.compile(
    r"function\s+(\w+)\s*\([^)]*\)[^{;]*\{(?:\s*//[^\n]*)*\s*vm\.skip\(\s*livePrecompiles\s*[,)]"
)
# A skipped forge result, with or without a skip reason: `[SKIP] name(...)` or `[SKIP: why] name(...)`.
_SKIPPED = re.compile(r"^\[SKIP[^\]]*\]\s+(\w+)\(", re.MULTILINE)


def main() -> None:
    output, base_std = Path(sys.argv[1]), Path(sys.argv[2])
    mock_only = {
        name
        for source in (base_std / "test").rglob("*.sol")
        for name in _MOCK_ONLY.findall(source.read_text())
    }
    skipped = _SKIPPED.findall(output.read_text())
    print(sum(name in mock_only for name in skipped))


if __name__ == "__main__":
    main()
