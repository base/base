#!/usr/bin/env python3
"""Measure forward-only Dowse hint learning on block-aligned execution traces."""

import argparse
import hashlib
import json
import resource
import subprocess
import tempfile
import time
from collections import defaultdict
from pathlib import Path


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline-traces", type=Path, required=True)
    parser.add_argument("--holdout-traces", type=Path, required=True)
    parser.add_argument("--static-hints", type=Path, required=True)
    parser.add_argument("--dowse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--refresh-blocks", type=int, default=50)
    parser.add_argument(
        "--windows",
        default="500,0",
        help="comma-separated training windows; 0 means cumulative",
    )
    parser.add_argument("--fixed-slot-min-frequency", type=float, default=0.4)
    parser.add_argument("--min-confidence-bps", type=int, default=2000)
    parser.add_argument("--max-storage-slots", type=int, default=256)
    return parser.parse_args()


def load_traces(path):
    traces = []
    with path.open() as source:
        for line in source:
            trace = json.loads(line)
            trace["_block"] = int(trace["_block"])
            traces.append(trace)
    return traces


def group_by_block(traces):
    blocks = defaultdict(list)
    for trace in traces:
        blocks[trace["_block"]].append(trace)
    return {block: blocks[block] for block in sorted(blocks)}


def normalized(value):
    return value.lower()


def unwrap_item(item, confidence=1.0):
    while item["kind"] == "Scored":
        score = item["confidence"]
        confidence = min(confidence, score if isinstance(score, (int, float)) else 0.0)
        item = item["item"]
    return item, max(0.0, min(1.0, confidence))


def resolve_expression(expression, calldata, caller):
    kind = expression["type"]
    if kind == "Concrete":
        return bytes.fromhex(expression["value"][2:])
    if kind == "CalldataWord":
        offset = expression["offset"]
        return calldata[offset : offset + 32] if len(calldata) >= offset + 32 else None
    if kind == "Caller":
        return bytes.fromhex(caller[2:]).rjust(32, b"\0")
    if kind == "Keccak256":
        inputs = [resolve_expression(value, calldata, caller) for value in expression["inputs"]]
        if any(value is None for value in inputs):
            return None
        return hashlib.new("keccak-256", b"".join(inputs)).digest()
    if kind == "Add":
        left = resolve_expression(expression["left"], calldata, caller)
        right = resolve_expression(expression["right"], calldata, caller)
        if left is None or right is None:
            return None
        value = (int.from_bytes(left, "big") + int.from_bytes(right, "big")) % (1 << 256)
        return value.to_bytes(32, "big")
    if kind == "SLoad":
        return None
    raise ValueError(f"unsupported expression: {kind}")


def lookup_items(hints, trace):
    address = normalized(trace["address"])
    code_hash = hints["code_hashes"].get(address)
    if code_hash is None:
        return None
    entries = hints["entries"].get(code_hash, {})
    calldata = bytes.fromhex(trace["calldata"][2:])
    selector = "0x" + calldata[:4].hex() if len(calldata) >= 4 else None
    items = entries.get(selector) if selector is not None else None
    return entries.get("*") if items is None else items


def storage_plan(hints, trace, min_confidence, max_storage_slots):
    items = lookup_items(hints, trace)
    if items is None:
        return None

    target = normalized(trace["address"])
    caller = normalized(trace["caller"])
    calldata = bytes.fromhex(trace["calldata"][2:])
    targets = {}
    for wrapped_item in items:
        item, confidence = unwrap_item(wrapped_item)
        if item["kind"] == "Storage":
            address = target
        elif item["kind"] == "ExternalStorage":
            address = normalized(item["address"])
        else:
            continue
        slot = resolve_expression(item["slot"], calldata, caller)
        if slot is None:
            continue
        key = (address, "0x" + slot.hex())
        targets[key] = max(targets.get(key, 0.0), confidence)

    ranked = sorted(targets.items(), key=lambda value: (-value[1], value[0]))[:max_storage_slots]
    return {target for target, confidence in ranked if confidence >= min_confidence}


def score_block(hints, traces, min_confidence, max_storage_slots):
    actual = {
        (normalized(address), normalized(slot))
        for trace in traces
        for address, slot in trace["storage_accesses"]
    }
    predicted = set()
    matched_transactions = 0
    planned_transactions = 0
    for trace in traces:
        plan = storage_plan(hints, trace, min_confidence, max_storage_slots)
        if plan is None:
            continue
        matched_transactions += 1
        if plan:
            planned_transactions += 1
            predicted.update(plan)
    return {
        "actual": len(actual),
        "predicted": len(predicted),
        "hits": len(actual & predicted),
        "misses": len(predicted - actual),
        "uncovered": len(actual - predicted),
        "matchedTransactions": matched_transactions,
        "plannedTransactions": planned_transactions,
    }


def add_score(total, score):
    for key, value in score.items():
        total[key] = total.get(key, 0) + value


def finish_score(score):
    predicted = score["hits"] + score["misses"]
    actual = score["hits"] + score["uncovered"]
    return {
        **score,
        "precision": score["hits"] / predicted if predicted else 0.0,
        "recall": score["hits"] / actual if actual else 0.0,
    }


def strip_block(trace):
    return {key: value for key, value in trace.items() if key != "_block"}


def infer_hints(binary, traces, fixed_slot_min_frequency, directory, sequence):
    traces_path = directory / f"traces-{sequence}.json"
    hints_path = directory / f"hints-{sequence}.json"
    with traces_path.open("w") as output:
        output.write("[")
        for index, trace in enumerate(traces):
            if index:
                output.write(",")
            json.dump(strip_block(trace), output, separators=(",", ":"))
        output.write("]")

    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.perf_counter()
    subprocess.run(
        [
            str(binary),
            "infer",
            "--traces",
            str(traces_path),
            "--fixed-slot-min-frequency",
            str(fixed_slot_min_frequency),
            "--format",
            "json",
            "--output",
            str(hints_path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    wall_seconds = time.perf_counter() - started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    hints = json.loads(hints_path.read_text())
    traces_path.unlink()
    hints_path.unlink()
    return hints, {
        "traces": len(traces),
        "wallMs": wall_seconds * 1000,
        "userCpuMs": (after.ru_utime - before.ru_utime) * 1000,
        "systemCpuMs": (after.ru_stime - before.ru_stime) * 1000,
        "addresses": len(hints["code_hashes"]),
        "selectors": sum(len(selectors) for selectors in hints["entries"].values()),
        "items": sum(
            len(items)
            for selectors in hints["entries"].values()
            for items in selectors.values()
        ),
    }


def main():
    args = parse_args()
    if args.refresh_blocks <= 0:
        raise ValueError("refresh interval must be positive")
    windows = [int(value) for value in args.windows.split(",")]
    if any(window < 0 for window in windows):
        raise ValueError("training windows must be non-negative")

    baseline = load_traces(args.baseline_traces)
    holdout = load_traces(args.holdout_traces)
    holdout_blocks = group_by_block(holdout)
    blocks = list(holdout_blocks)
    baseline_block_count = len({trace["_block"] for trace in baseline})
    static_hints = json.loads(args.static_hints.read_text())
    min_confidence = args.min_confidence_bps / 10_000

    static_by_block = {}
    static_total = {}
    for block, traces in holdout_blocks.items():
        score = score_block(static_hints, traces, min_confidence, args.max_storage_slots)
        static_by_block[block] = score
        add_score(static_total, score)

    result = {
        "config": {
            "baselineBlocks": [
                min(trace["_block"] for trace in baseline),
                max(trace["_block"] for trace in baseline),
            ],
            "holdoutBlocks": [blocks[0], blocks[-1]],
            "refreshBlocks": args.refresh_blocks,
            "fixedSlotMinFrequency": args.fixed_slot_min_frequency,
            "minConfidenceBps": args.min_confidence_bps,
            "maxStorageSlots": args.max_storage_slots,
        },
        "static": {"total": finish_score(static_total), "blocks": static_by_block},
        "strategies": {},
    }

    with tempfile.TemporaryDirectory(prefix="dowse-online-learning-") as temporary:
        directory = Path(temporary)
        for window in windows:
            strategy = "cumulative" if window == 0 else f"rolling-{window}"
            observed = list(baseline)
            adaptive_hints = static_hints
            adaptive_total = {}
            adaptive_by_block = {}
            refreshes = []
            for offset in range(0, len(blocks), args.refresh_blocks):
                chunk = blocks[offset : offset + args.refresh_blocks]
                if offset > 0 or (window and window < baseline_block_count):
                    training = (
                        observed
                        if window == 0
                        else [
                            trace
                            for trace in observed
                            if trace["_block"] >= chunk[0] - window
                        ]
                    )
                    adaptive_hints, inference = infer_hints(
                        args.dowse_bin,
                        training,
                        args.fixed_slot_min_frequency,
                        directory,
                        f"{strategy}-{offset}",
                    )
                    inference["effectiveFromBlock"] = chunk[0]
                    refreshes.append(inference)

                for block in chunk:
                    score = score_block(
                        adaptive_hints,
                        holdout_blocks[block],
                        min_confidence,
                        args.max_storage_slots,
                    )
                    baseline_score = static_by_block[block]
                    score["hitDeltaVsStatic"] = score["hits"] - baseline_score["hits"]
                    score["predictionDeltaVsStatic"] = score["predicted"] - baseline_score["predicted"]
                    adaptive_by_block[block] = score
                    add_score(adaptive_total, score)
                    observed.extend(holdout_blocks[block])

            total = finish_score(adaptive_total)
            total["recallDeltaVsStatic"] = total["recall"] - result["static"]["total"]["recall"]
            total["precisionDeltaVsStatic"] = total["precision"] - result["static"]["total"]["precision"]
            result["strategies"][strategy] = {
                "total": total,
                "blocks": adaptive_by_block,
                "refreshes": refreshes,
            }

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({
        "static": result["static"]["total"],
        "strategies": {
            name: {
                "total": value["total"],
                "meanRefreshWallMs": sum(item["wallMs"] for item in value["refreshes"])
                / len(value["refreshes"]),
            }
            for name, value in result["strategies"].items()
        },
    }, indent=2))


if __name__ == "__main__":
    main()
