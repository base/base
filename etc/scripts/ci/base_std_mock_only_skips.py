#!/usr/bin/env python3
"""Count the skipped base-std fork tests that only run against base-std's Solidity mocks.

base-std skips a test in fork mode by opening it with `vm.skip(livePrecompiles)` when it can
only run against the mocks, for example because it writes precompile storage with `vm.store`.
Those skips are expected on every fork, so the fork-test summary reports them apart from other
skips, such as env-gated tests, which should stay visible.

A skip counts as mock-only only when the function is defined with that guard in the same file
and contract that forge reports it under. Anything else, including a test inherited from
another contract, stays with the other skips.

Usage: base_std_mock_only_skips.py <forge-output> <base-std-dir>
Prints the number of skipped results in the forge output whose test is mock-only.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

_LINE_COMMENT = re.compile(r"//[^\n]*")
_CONTRACT = re.compile(r"\bcontract\s+(\w+)")
# A function, with line comments removed, whose first statement is `vm.skip(livePrecompiles)`
# with or without a reason.
_MOCK_ONLY = re.compile(
    r"function\s+(\w+)\s*\([^)]*\)[^{;]*\{\s*vm\.skip\(\s*livePrecompiles\s*[,)]"
)
# Forge's suite header: `Ran 3 tests for test/unit/Foo.t.sol:FooTest`.
_SUITE = re.compile(r"^Ran \d+ tests? for ([^:\s]+):(\w+)")
# A skipped result, with or without a reason: `[SKIP] name(...)` or `[SKIP: why] name(...)`.
_SKIPPED = re.compile(r"^\[SKIP[^\]]*\]\s+(\w+)\(")


def mock_only_tests(base_std: Path) -> set[tuple[str, str, str]]:
    """Returns `(path, contract, function)` for every mock-only test in base-std."""
    tests = set()
    for source in (base_std / "test").rglob("*.sol"):
        path = source.relative_to(base_std).as_posix()
        text = _LINE_COMMENT.sub("", source.read_text())
        contracts = [(match.start(), match.group(1)) for match in _CONTRACT.finditer(text)]
        for function in _MOCK_ONLY.finditer(text):
            owners = [name for start, name in contracts if start < function.start()]
            if owners:
                tests.add((path, owners[-1], function.group(1)))
    return tests


def main() -> None:
    output, base_std = Path(sys.argv[1]), Path(sys.argv[2])
    if not (base_std / "test").is_dir():
        sys.exit(f"no base-std test directory under {base_std}")
    mock_only = mock_only_tests(base_std)
    suite = None
    count = 0
    for line in output.read_text().splitlines():
        if header := _SUITE.match(line):
            suite = header.groups()
        elif (skipped := _SKIPPED.match(line)) and suite:
            count += (*suite, skipped.group(1)) in mock_only
    print(count)


if __name__ == "__main__":
    main()
