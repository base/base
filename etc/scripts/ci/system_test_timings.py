#!/usr/bin/env python3
"""Summarize per-test durations from nextest logs of the CI System Tests job.

Reads one or more saved job logs (for example from `depot ci logs <attempt-id>`), takes every
passing test's duration, and prints a Markdown report with the min, median and max across the
logs, slowest first. Pass several logs from the same configuration so one noisy run does not
decide the ranking.

    depot ci logs <attempt-id> --output-file run1.txt
    python3 etc/scripts/ci/system_test_timings.py run1.txt run2.txt run3.txt
"""

import re
import statistics
import sys
from collections import defaultdict

ANSI = re.compile(r"\x1b\[[0-9;]*m")
# `PASS [  12.345s] ( 12/131) <binary id> <test name>`, optionally prefixed by `TRY n ` when a
# retry passed. Integration tests use the binary id `base-system-tests::<binary>`, unit tests
# use `base-system-tests`.
PASS = re.compile(r"(?:TRY (\d+) )?PASS \[\s*([\d.]+)s\]\s+\(\s*\d+/\d+\)\s+(\S+)\s+(\S+)")
SUMMARY = re.compile(r"Summary \[\s*([\d.]+)s\]")
# Tests below this are unit-style and not worth listing individually.
LISTED_MIN_SECONDS = 1.0


def parse(path):
    """Returns ({test id: (seconds, retried)}, total seconds) for one log."""
    tests, total = {}, None
    with open(path, encoding="utf-8", errors="replace") as log:
        for line in log:
            line = ANSI.sub("", line)
            if match := SUMMARY.search(line):
                total = float(match[1])
            elif match := PASS.search(line):
                retry, seconds, binary, name = match.groups()
                test_id = name if binary == "base-system-tests" else f"{binary.split('::')[-1]}::{name}"
                tests[test_id] = (float(seconds), retry is not None)
    return tests, total


def main(paths):
    if not paths:
        sys.exit(__doc__)
    runs = [parse(path) for path in paths]
    durations, retried = defaultdict(list), set()
    for tests, _ in runs:
        for test_id, (seconds, was_retry) in tests.items():
            durations[test_id].append(seconds)
            if was_retry:
                retried.add(test_id)

    totals = [total for _, total in runs if total is not None]
    rows = sorted(
        ((statistics.median(v), min(v), max(v), len(v), test_id) for test_id, v in durations.items()),
        reverse=True,
    )
    listed = [row for row in rows if row[0] >= LISTED_MIN_SECONDS]
    unlisted = [row for row in rows if row[0] < LISTED_MIN_SECONDS]

    print("# System test timings\n")
    print(f"Per-test durations from {len(runs)} CI run(s) of the `System Tests` job.\n")
    print("**These are wall-clock times under load, not the cost of a test on its own.** Tests share the")
    print("runner and the shared L1, so a test's time includes waiting on CPU, on the deployment lock and")
    print("on other tests' Docker work. Locally one `upgrade_signal` test took 18-30s alone but 100s+ at")
    print("six threads. To find what a test really costs, run it alone, and compare the total below")
    print("against it before and after a change, not individual rows.\n")
    if totals:
        print(f"Test phase (nextest summary): {', '.join(f'{t:.0f}s' for t in totals)}.")
    print(f"Tests listed: {len(listed)} taking at least {LISTED_MIN_SECONDS:.0f}s; "
          f"{len(unlisted)} faster tests, totalling {sum(r[0] for r in unlisted):.0f}s, are omitted.\n")
    print("| Test | Median (s) | Min (s) | Max (s) | Runs | Share of summed time |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    grand = sum(r[0] for r in rows)
    for median, low, high, count, test_id in listed:
        note = " (retried)" if test_id in retried else ""
        print(f"| `{test_id}`{note} | {median:.1f} | {low:.1f} | {high:.1f} | {count} | {100 * median / grand:.1f}% |")
    print(f"\nSum of medians: {grand:.0f}s across {len(rows)} tests. Nextest runs several at once, so this")
    print("is larger than the test phase above.")


if __name__ == "__main__":
    main(sys.argv[1:])
