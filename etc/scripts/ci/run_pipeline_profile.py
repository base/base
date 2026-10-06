#!/usr/bin/env python3
"""Run isolated production-pipeline benchmark repetitions and preserve raw artifacts."""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import subprocess
import sys

WORKLOADS = ("transfer-legacy", "transfer-azul", "storage-legacy", "storage-azul")


def run_one(binary, label, output_dir, repetition, workload, load_class):
    name = f"{repetition + 1:02}-{workload}"
    output = output_dir.resolve() / f"{name}.json"
    env = dict(os.environ, PIPELINE_WORKLOAD=workload, PIPELINE_OUTPUT=str(output), PIPELINE_LOAD_CLASS=load_class)
    command = [str(binary), "profile_pipeline", "--ignored", "--nocapture", "--test-threads=1"]
    if sys.platform == "darwin":
        command = ["/usr/bin/time", "-l", *command]
    print(f"{label}: {name}", flush=True)
    with (output_dir / f"{name}.log").open("w") as log:
        subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=180, check=True)
    log_text = (output_dir / f"{name}.log").read_text()
    if any(error in log_text for error in ("Persistence service failed", "Termination failed", "panicked at", "critical task exited")):
        raise RuntimeError(f"node task failed during {name}; inspect the preserved log")
    samples = json.loads(output.read_text())
    active = [v for sample in samples for v in sample["timings"]["base_builder_active_block_build_duration"]]
    print(f"  mean active pipeline: {1000 * sum(active) / len(active):.3f} ms/block", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--label", required=True)
    parser.add_argument("--comparison-binary", type=pathlib.Path)
    parser.add_argument("--comparison-label", default="candidate")
    parser.add_argument("--baseline-commit")
    parser.add_argument("--comparison-commit")
    parser.add_argument("--load-class", choices=("reference", "normal", "stress"), default="reference")
    args = parser.parse_args()
    binary = args.binary.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    current_commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    entries = [(binary, args.label, args.output / args.label if args.comparison_binary else args.output, args.baseline_commit or current_commit)]
    if args.comparison_binary:
        entries.append((args.comparison_binary.resolve(), args.comparison_label, args.output / args.comparison_label, args.comparison_commit or current_commit))
    for executable, label, output_dir, commit in entries:
        if output_dir != args.output:
            output_dir.mkdir(exist_ok=False)
        manifest = {
            "label": label, "binary": str(executable),
            "binary_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
            "host": platform.platform(), "commit": commit,
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            "workloads": WORKLOADS, "repetitions": args.repetitions,
            "load_class": args.load_class,
            "comparison_order": "baseline/candidate on odd pairs, candidate/baseline on even pairs; paired per workload" if args.comparison_binary else "single cohort",
            "contract_sha256": hashlib.sha256(pathlib.Path("etc/benchmarks/block-building-contract.json").read_bytes()).hexdigest(),
        }
        (output_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for repetition in range(args.repetitions):
        for workload in WORKLOADS:
            order = entries if repetition % 2 == 0 else list(reversed(entries))
            for executable, label, output_dir, _ in order:
                run_one(executable, label, output_dir, repetition, workload, args.load_class)


if __name__ == "__main__":
    main()
