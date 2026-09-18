#!/usr/bin/env python3
"""Render a single PR comment from a fresh-devnet base-bench workload suite."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

MARKER = "<!-- fresh-devnet-bench-results -->"


def markdown_cell(value: object) -> str:
    """Format untrusted JSON text safely inside a one-line Markdown table cell."""
    return str(value).replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")


def load_json(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as file:
        value = json.load(file)
    if not isinstance(value, dict):
        raise ValueError(f"expected a JSON object in {path}")
    return value


def number(value: object, precision: int = 0) -> str:
    if not isinstance(value, (int, float)):
        return "—"
    return f"{value:,.{precision}f}"


def duration(value: object) -> str:
    if not isinstance(value, dict):
        return "—"
    seconds = value.get("secs")
    nanoseconds = value.get("nanos")
    if not isinstance(seconds, int) or not isinstance(nanoseconds, int):
        return "—"
    milliseconds = seconds * 1_000 + nanoseconds / 1_000_000
    if milliseconds < 1:
        return f"{milliseconds * 1_000:.0f} µs"
    return f"{milliseconds:.1f} ms"


def summary_values(results_dir: Path, output_dir: str) -> tuple[str, str, str, str]:
    summary_path = results_dir / output_dir / "load-test-result.json"
    if not summary_path.is_file():
        return "—", "—", "—", "—"
    try:
        summary = load_json(summary_path)
    except (OSError, ValueError, json.JSONDecodeError):
        return "—", "—", "—", "—"
    throughput = summary.get("throughput", {})
    block_latency = summary.get("block_latency", {})
    if not isinstance(throughput, dict) or not isinstance(block_latency, dict):
        return "—", "—", "—", "—"
    return (
        number(throughput.get("total_confirmed")),
        number(throughput.get("tps"), 1),
        number(throughput.get("gps"), 0),
        duration(block_latency.get("p50")),
    )


def render_comment(results_dir: Path, run_url: str, commit: str) -> str:
    suite_path = results_dir / "suite-results.json"
    if not suite_path.is_file():
        return "\n".join(
            [
                MARKER,
                "## Fresh-devnet `base-bench` results",
                "",
                f"Commit `{markdown_cell(commit)}` · [workflow run]({run_url})",
                "",
                "❌ The suite did not produce a results manifest. The build or devnet setup failed "
                "before the workloads could begin; see the workflow run for details.",
                "",
            ]
        )
    suite = load_json(suite_path)
    runs = suite.get("runs")
    if not isinstance(runs, list):
        raise ValueError("suite-results.json is missing its runs array")

    lines = [
        MARKER,
        "## Fresh-devnet `base-bench` results",
        "",
        f"Commit `{markdown_cell(commit)}` · [workflow run]({run_url})",
        "",
        "Every workload ran against its own newly initialized, empty Base devnet. "
        "B-20 workloads enable Beryl at genesis. These are advisory measurements, not merge gates.",
        "",
        "| Workload | Payload | Status | Confirmed | TPS | Gas/s | p50 block latency |",
        "| --- | --- | --- | ---: | ---: | ---: | ---: |",
    ]
    failures: list[tuple[str, str]] = []
    for run in runs:
        if not isinstance(run, dict):
            raise ValueError("suite-results.json contains a non-object run")
        workload = markdown_cell(run.get("workload", "unknown"))
        payload = markdown_cell(run.get("transaction_payload", "unknown"))
        output_dir = run.get("output_dir")
        if not isinstance(output_dir, str):
            output_dir = ""
        confirmed, tps, gps, latency = summary_values(results_dir, output_dir)
        success = run.get("success") is True
        status = "✅ passed" if success else "❌ failed"
        lines.append(
            f"| `{workload}` | `{payload}` | {status} | {confirmed} | {tps} | {gps} | {latency} |"
        )
        error = run.get("error")
        if not success and isinstance(error, str) and error:
            failures.append((workload, error))

    lines.extend(
        [
            "",
            "Artifacts: `fresh-devnet-benchmark-results` contains the raw load-test sidecars; "
            "`fresh-devnet-benchmark-visualizer` contains a static [base/benchmark](https://github.com/base/benchmark) report.",
        ]
    )
    if failures:
        lines.extend(["", "<details><summary>Failed workload details</summary>", ""])
        for workload, error in failures:
            lines.append(f"- `{workload}`: `{markdown_cell(error)[:1_000]}`")
        lines.extend(["", "</details>"])
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--run-url", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render_comment(args.results_dir, args.run_url, args.commit), encoding="utf-8")


if __name__ == "__main__":
    main()
