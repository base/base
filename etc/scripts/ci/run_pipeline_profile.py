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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--label", required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {
        "label": args.label,
        "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "host": platform.platform(),
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "workloads": WORKLOADS,
        "repetitions": args.repetitions,
    }
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    for repetition in range(args.repetitions):
        for workload in WORKLOADS:
            name = f"{repetition + 1:02}-{workload}"
            output = args.output.resolve() / f"{name}.json"
            env = dict(os.environ, PIPELINE_WORKLOAD=workload, PIPELINE_OUTPUT=str(output))
            command = [str(binary), "profile_pipeline", "--ignored", "--nocapture", "--test-threads=1"]
            if sys.platform == "darwin":
                command = ["/usr/bin/time", "-l", *command]
            print(f"{args.label}: {name}", flush=True)
            with (args.output / f"{name}.log").open("w") as log:
                subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, timeout=180, check=True)
            samples = json.loads(output.read_text())
            active = [v for sample in samples for v in sample["timings"]["base_builder_active_block_build_duration"]]
            print(f"  mean active pipeline: {1000 * sum(active) / len(active):.3f} ms/block", flush=True)


if __name__ == "__main__":
    main()
