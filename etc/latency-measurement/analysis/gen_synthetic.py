#!/usr/bin/env python3
"""Fabricate realistic multi-region P2P block-latency CSVs.

Produces one CSV per region under --out-dir, matching the schema consumed by
analyze.py. Purely synthetic; lets you exercise the toolkit end-to-end before
any real observer data exists.

Schema (exact column order):
    recv_wallclock_ns,block_number,block_hash,produced_sec,
    produced_millis_part,region,peer_id

Usage:
    python gen_synthetic.py --out-dir data/ --blocks 5000 [--holocene] [--seed 42]
"""

from __future__ import annotations

import csv
import argparse
from pathlib import Path

import numpy as np

# Regions ordered fastest -> slowest one-way latency to the sequencer.
# base_ms is the per-region floor one-way latency in milliseconds.
REGIONS = {
    "us-east": 30.0,
    "us-west": 70.0,
    "eu-central": 110.0,
    "eu-north": 130.0,
    "ap-northeast": 160.0,
    "ap-southeast": 230.0,
}

# Post-Holocene BaseTime sub-second buckets (200ms cadence).
HOLOCENE_MILLIS = [0, 200, 400, 600, 800]

# Genesis-ish base timestamp for produced_sec (arbitrary, just realistic magnitude).
BASE_PRODUCED_SEC = 1_700_000_000

# Fraction of (region, block) observations that are simply missing.
DROP_PROB = 0.01

# libp2p-ish PeerId strings, one representative upstream peer per region.
PEER_IDS = {
    "us-east": "16Uiu2HAmUsEast000000000000000000000000000000000001",
    "us-west": "16Uiu2HAmUsWest000000000000000000000000000000000002",
    "eu-central": "16Uiu2HAmEuCentral0000000000000000000000000000000003",
    "eu-north": "16Uiu2HAmEuNorth00000000000000000000000000000000004",
    "ap-northeast": "16Uiu2HAmApNortheast00000000000000000000000000000005",
    "ap-southeast": "16Uiu2HAmApSoutheast00000000000000000000000000000006",
}


class SyntheticGenerator:
    """Generates synthetic per-region latency CSVs."""

    def __init__(self, out_dir: Path, blocks: int, holocene: bool, seed: int):
        self.out_dir = out_dir
        self.blocks = blocks
        self.holocene = holocene
        self.rng = np.random.default_rng(seed)

    @staticmethod
    def _block_hash(block_number: int) -> str:
        """Deterministic 0x-prefixed 32-byte hash, shared across regions."""
        return "0x" + f"{block_number:064x}"

    def _produced(self, i: int) -> tuple[int, int]:
        """(produced_sec, produced_millis_part) for the i-th block."""
        if self.holocene:
            # 200ms cadence: 5 sub-second slots per second, so seconds advance
            # every 5 blocks and millis cycle through the buckets.
            millis = HOLOCENE_MILLIS[i % len(HOLOCENE_MILLIS)]
            produced_sec = BASE_PRODUCED_SEC + (i // len(HOLOCENE_MILLIS)) * 1
        else:
            # Pre-Holocene: whole-second timestamps, produced_sec increments by 2.
            millis = 0
            produced_sec = BASE_PRODUCED_SEC + i * 2
        return produced_sec, millis

    def _latency_ns(self, base_ms: float) -> int:
        """One-way latency in ns: base + lognormal jitter + heavy P99 tail."""
        # Lognormal jitter with a modest median relative to base latency.
        jitter_ms = self.rng.lognormal(mean=np.log(8.0), sigma=0.6)
        # Heavy tail: ~2% of samples get a big multiplicative spike.
        if self.rng.random() < 0.02:
            jitter_ms += self.rng.lognormal(mean=np.log(120.0), sigma=0.5)
        total_ms = base_ms + jitter_ms
        return int(round(total_ms * 1e6))

    def run(self) -> list[Path]:
        self.out_dir.mkdir(parents=True, exist_ok=True)
        written: list[Path] = []

        # Precompute produced times so all regions agree on each block.
        produced = [self._produced(i) for i in range(self.blocks)]

        for region, base_ms in REGIONS.items():
            path = self.out_dir / f"{region}.csv"
            peer_id = PEER_IDS[region]
            with path.open("w", newline="") as fh:
                writer = csv.writer(fh)
                writer.writerow(
                    [
                        "recv_wallclock_ns",
                        "block_number",
                        "block_hash",
                        "produced_sec",
                        "produced_millis_part",
                        "region",
                        "peer_id",
                    ]
                )
                for i in range(self.blocks):
                    if self.rng.random() < DROP_PROB:
                        continue  # region missed this block
                    produced_sec, millis = produced[i]
                    produced_ms = produced_sec * 1000 + millis
                    recv_ns = int(produced_ms * 1e6) + self._latency_ns(base_ms)
                    writer.writerow(
                        [
                            recv_ns,
                            i,
                            self._block_hash(i),
                            produced_sec,
                            millis,
                            region,
                            peer_id,
                        ]
                    )
            written.append(path)
        return written


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", default="data", help="output directory for per-region CSVs")
    parser.add_argument("--blocks", type=int, default=5000, help="number of blocks to fabricate")
    parser.add_argument(
        "--holocene",
        action="store_true",
        help="cycle produced_millis_part through 0/200/400/600/800 (post-Holocene 200ms cadence)",
    )
    parser.add_argument("--seed", type=int, default=42, help="RNG seed for reproducibility")
    args = parser.parse_args()

    gen = SyntheticGenerator(
        out_dir=Path(args.out_dir),
        blocks=args.blocks,
        holocene=args.holocene,
        seed=args.seed,
    )
    written = gen.run()
    total = sum(1 for _ in open(written[0])) - 1 if written else 0
    print(f"Wrote {len(written)} region CSVs to {args.out_dir}/ ({args.blocks} blocks each)")
    for p in written:
        print(f"  {p}")
    print(f"holocene={args.holocene} seed={args.seed} (~{total} rows in first region after drops)")


if __name__ == "__main__":
    main()
