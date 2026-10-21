#!/usr/bin/env python3
"""Measure historical Base execution payloads with and without EIP-7928 BALs.

    BASE_RPC_URL=https://... python3 etc/scripts/local/bal-bandwidth.py --days 3

Requires eth_getBlockByNumber, debug_getRawBlock and debug_getRawBlockAccessList.
Reth's BAL RPC replays uncached blocks, including pre/post execution system work;
the endpoint needs historical state for the requested period. No trace heuristic
or EIP-2930 transaction access list is used as a substitute for a BAL.

Reports SSZ payload bytes and raw Snappy-compressed payload bytes (not JSON).
The proposed BAL payload appends one variable-length RLP BAL field: 4-byte SSZ
offset plus BAL bytes. This isolates BAL overhead: no SLOTNUM field, signature,
parent-beacon-root envelope, libp2p framing, or gossip replication is counted.
Compression is per payload, with the BAL compressed together with the payload.
These are historical execution measurements, not a replay under future gas rules.

Snappy requires either the system libsnappy library or the python-snappy package.
Use --no-compression to measure uncompressed bytes using only Python's stdlib.
CSV rows are flushed incrementally; summary.json is written only on success.
Use --save-raw to also retain the RPC data as one gzip JSON file per block.
"""

from __future__ import annotations

import argparse
from array import array
from collections import deque
from concurrent.futures import ThreadPoolExecutor
import csv
import ctypes
import ctypes.util
import gzip
import itertools
import json
import math
import os
from pathlib import Path
import sys
import time
import urllib.error
import urllib.request


class Rpc:
    def __init__(self, url, timeout=120, retries=3):
        self.url, self.timeout, self.retries = url, timeout, retries
        self.ids = itertools.count(1)

    def call(self, method, params):
        request_id = next(self.ids)
        request = urllib.request.Request(
            self.url,
            json.dumps(dict(jsonrpc="2.0", id=request_id, method=method, params=params)).encode(),
            {"Content-Type": "application/json"},
        )
        for attempt in range(self.retries + 1):
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    body = json.load(response)
                if body.get("id") != request_id:
                    raise RuntimeError(f"{method}: mismatched RPC response ID")
                if "error" in body:
                    # Avoid printing provider error bodies, which may echo credentials.
                    code = body["error"].get("code")
                    raise RuntimeError(f"{method}: RPC error {code}; check method/state availability")
                if body.get("result") is None:
                    raise RuntimeError(f"{method}: no data returned")
                return body["result"]
            except (urllib.error.URLError, TimeoutError) as error:
                retryable = not isinstance(error, urllib.error.HTTPError) or (
                    error.code == 429 or error.code >= 500
                )
                if not retryable or attempt == self.retries:
                    raise RuntimeError(f"{method}: HTTP/transport failure ({type(error).__name__})") from None
                time.sleep(min(2**attempt, 8))

    def block(self, number):
        return self.call("eth_getBlockByNumber", [hex(number) if isinstance(number, int) else number, False])


def unhex(value):
    if not isinstance(value, str) or not value.startswith("0x"):
        raise ValueError("Expected RPC hex bytes")
    return bytes.fromhex(value[2:])


def quantity(value):
    return int(value, 16)


def rlp_decode(data):
    """Decode raw block RLP, retaining typed transaction byte strings."""
    def item(pos, limit):
        if pos >= limit:
            raise ValueError("Truncated RLP")
        prefix = data[pos]
        if prefix < 0x80:
            return data[pos:pos + 1], pos + 1
        is_list = prefix >= 0xC0
        short = 0xC0 if is_list else 0x80
        long = short + 55
        if prefix <= long:
            start, size = pos + 1, prefix - short
        else:
            length_size = prefix - long
            start = pos + 1 + length_size
            if start > limit:
                raise ValueError("Truncated RLP length")
            size = int.from_bytes(data[pos + 1:start], "big")
        end = start + size
        if end > limit:
            raise ValueError("Truncated RLP data")
        if not is_list:
            return data[start:end], end
        values = []
        while start < end:
            value, start = item(start, end)
            values.append(value)
        return values, end

    value, end = item(0, len(data))
    if end != len(data):
        raise ValueError("Trailing RLP data")
    return value


def rlp_encode(value):
    is_list = isinstance(value, list)
    data = b"".join(map(rlp_encode, value)) if is_list else value
    if not is_list and len(data) == 1 and data[0] < 0x80:
        return data
    base = 0xC0 if is_list else 0x80
    if len(data) <= 55:
        return bytes([base + len(data)]) + data
    size = len(data).to_bytes((len(data).bit_length() + 7) // 8, "big")
    return bytes([base + 55 + len(size)]) + size + data


def ssz_container(fields):
    """Each field is (encoded bytes, is_variable); SSZ offsets are little endian."""
    offset = sum(4 if variable else len(data) for data, variable in fields)
    fixed, dynamic = [], []
    for data, variable in fields:
        if variable:
            fixed.append(offset.to_bytes(4, "little"))
            dynamic.append(data)
            offset += len(data)
        else:
            fixed.append(data)
    return b"".join(fixed + dynamic)


def payload_fields(block, raw_block):
    """Reconstruct Base V1/V2/V3/V4 SSZ from header JSON and canonical tx bytes."""
    decoded = rlp_decode(raw_block)
    if not isinstance(decoded, list) or len(decoded) < 3 or not isinstance(decoded[1], list):
        raise ValueError("Invalid raw block")
    transactions = [rlp_encode(tx) if isinstance(tx, list) else tx for tx in decoded[1]]
    if len(transactions) != len(block["transactions"]):
        raise ValueError("RPC block/transaction count mismatch")
    fields = []

    def fixed(name, width, integer=False):
        value = (quantity(block[name]).to_bytes(width, "little") if integer else unhex(block[name]))
        if len(value) != width:
            raise ValueError(f"Wrong width for {name}")
        fields.append((value, False))

    for name, width in [("parentHash", 32), ("miner", 20), ("stateRoot", 32),
                        ("receiptsRoot", 32), ("logsBloom", 256), ("mixHash", 32)]:
        fixed(name, width)
    for name in ["number", "gasLimit", "gasUsed", "timestamp"]:
        fixed(name, 8, True)
    fields.append((unhex(block["extraData"]), True))
    fixed("baseFeePerGas", 32, True)
    fixed("hash", 32)
    fields.append((ssz_container([(tx, True) for tx in transactions]), True))
    if block.get("withdrawals") is not None:
        if block["withdrawals"]:
            raise ValueError("Base payloads require an empty withdrawals list")
        fields.append((b"", True))
    if block.get("blobGasUsed") is not None:
        if block.get("withdrawals") is None:
            raise ValueError("V3 payload missing withdrawals")
        fixed("blobGasUsed", 8, True)
        fixed("excessBlobGas", 8, True)
    return fields


EMPTY_WITHDRAWALS_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"


def execution_fields(block, raw_block, version):
    fields = payload_fields(block, raw_block)
    # Base sets requestsHash starting at Isthmus, including when the message
    # passer storage root is empty. The non-empty root also identifies V4 on
    # providers that omit requestsHash. Overrides support custom test forks.
    v4 = version == "v4" or (version == "auto" and (
        block.get("requestsHash") is not None or
        block.get("withdrawalsRoot") not in (None, EMPTY_WITHDRAWALS_ROOT)
    ))
    if version == "v3" and block.get("blobGasUsed") is None:
        raise ValueError("Base V3 requires blob-gas fields")
    if v4:
        if block.get("blobGasUsed") is None:
            raise ValueError("Base V4 requires V3 blob-gas fields")
        root = unhex(block["withdrawalsRoot"])
        if len(root) != 32:
            raise ValueError("Wrong withdrawalsRoot width")
        fields.append((root, False))
    return fields


class Snappy:
    """Raw Snappy, matching Base gossip's compression (no framed stream)."""
    def __init__(self):
        try:
            import snappy
            self.compress = snappy.compress
            return
        except ImportError:
            pass
        name = ctypes.util.find_library("snappy")
        if not name:
            raise RuntimeError("Install libsnappy or python-snappy, or pass --no-compression")
        self.lib = ctypes.CDLL(name)
        self.lib.snappy_max_compressed_length.argtypes = [ctypes.c_size_t]
        self.lib.snappy_max_compressed_length.restype = ctypes.c_size_t
        self.lib.snappy_compress.argtypes = [ctypes.c_char_p, ctypes.c_size_t,
                                            ctypes.c_void_p, ctypes.POINTER(ctypes.c_size_t)]
        self.lib.snappy_compress.restype = ctypes.c_int

    def compress(self, data):
        capacity = ctypes.c_size_t(self.lib.snappy_max_compressed_length(len(data)))
        output = ctypes.create_string_buffer(capacity.value)
        if self.lib.snappy_compress(data, len(data), output, ctypes.byref(capacity)) != 0:
            raise RuntimeError("Snappy compression failed")
        return output.raw[:capacity.value]


def find_start(rpc, end, cutoff):
    low, high = 0, quantity(end["number"])
    while low < high:
        mid = (low + high) // 2
        if quantity(rpc.block(mid)["timestamp"]) < cutoff:
            low = mid + 1
        else:
            high = mid
    return low


def measure(rpc, number, version, compressor, raw_dir=None):
    block = rpc.block(number)
    if quantity(block["number"]) != number:
        raise ValueError("RPC returned the wrong block number")
    raw = rpc.call("debug_getRawBlock", [block["hash"]])
    bal_hex = rpc.call("debug_getRawBlockAccessList", [block["hash"]])
    bal = unhex(bal_hex)
    if not bal or bal[0] < 0xC0:
        raise ValueError("BAL is missing or is not an RLP list")
    # Reject truncated data, including an absent BAL represented by 0x80.
    rlp_decode(bal)
    fields = execution_fields(block, unhex(raw), version)
    payload = ssz_container(fields)
    combined = ssz_container(fields + [(bal, True)])
    row = dict(block_number=number, block_hash=block["hash"], parent_hash=block["parentHash"],
               timestamp=quantity(block["timestamp"]), transactions=len(block["transactions"]),
               payload_bytes=len(payload), bal_bytes=len(bal), payload_with_bal_bytes=len(combined))
    if compressor:
        row["payload_snappy_bytes"] = len(compressor.compress(payload))
        row["payload_with_bal_snappy_bytes"] = len(compressor.compress(combined))
    if raw_dir:
        with gzip.open(raw_dir / f"{number}.json.gz", "wt", encoding="utf-8") as output:
            json.dump(dict(block=block, raw_block=raw, bal=bal_hex), output)
    return row


def ordered_measurements(rpc, numbers, args, compressor, raw_dir):
    """Bound outstanding RPC work and emit rows in canonical block order."""
    numbers = iter(numbers)
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        pending = deque()

        def submit():
            number = next(numbers, None)
            if number is not None:
                pending.append(pool.submit(measure, rpc, number, args.payload_version, compressor, raw_dir))

        for _ in range(args.workers * 2):
            submit()
        while pending:
            yield pending.popleft().result()
            submit()


def statistics(values):
    ordered = sorted(values)
    return dict(total=sum(values), mean=sum(values) / len(values), min=ordered[0],
                p50=ordered[math.ceil(len(values) * 0.50) - 1],
                p95=ordered[math.ceil(len(values) * 0.95) - 1],
                p99=ordered[math.ceil(len(values) * 0.99) - 1], max=ordered[-1])


def collect(args, rpc, compressor):
    end = rpc.block(args.end_block)
    cutoff = quantity(end["timestamp"]) - args.days * 86400
    start = find_start(rpc, end, cutoff)
    stop = quantity(end["number"])
    metadata = dict(chain_id=rpc.call("eth_chainId", []), days=args.days, start_block=start,
                    end_block=stop, end_hash=end["hash"], cutoff_timestamp=cutoff,
                    end_timestamp=quantity(end["timestamp"]), payload_version=args.payload_version,
                    model="Base SSZ payload; append RLP BAL as one variable field (+4-byte offset)",
                    compression="raw Snappy per payload" if compressor else None,
                    excludes=["signatures", "parent beacon root envelope", "libp2p framing",
                              "gossip replication", "other future payload fields"])
    args.out.mkdir(parents=True, exist_ok=True)
    # Never mix a partial run with a new window or overwrite collected results.
    with (args.out / "metadata.json").open("x") as output:
        json.dump(metadata, output, indent=2)
    raw_dir = args.out / "raw" if args.save_raw else None
    if raw_dir:
        raw_dir.mkdir()
    metrics = {}
    previous, first_timestamp, transactions, count = None, None, 0, 0
    with (args.out / "blocks.csv").open("x", newline="") as output:
        writer = None
        for row in ordered_measurements(rpc, range(start, stop + 1), args, compressor, raw_dir):
            if previous is not None and row["parent_hash"] != previous:
                raise RuntimeError("Chain changed during collection; rerun into a new output directory")
            previous = row["block_hash"]
            if first_timestamp is None:
                first_timestamp = row["timestamp"]
            if writer is None:
                writer = csv.DictWriter(output, fieldnames=list(row))
                writer.writeheader()
            writer.writerow(row)
            output.flush()
            for key, value in row.items():
                if key.endswith("_bytes"):
                    metrics.setdefault(key, array("Q")).append(value)
            count += 1
            transactions += row["transactions"]
            if count == 1 or count % 100 == 0 or row["block_number"] == stop:
                print(f"Collected {count}/{stop - start + 1} blocks", file=sys.stderr)
    if previous != end["hash"] or rpc.block(stop)["hash"] != end["hash"]:
        raise RuntimeError("Pinned end block changed; rerun into a new output directory")
    summary = dict(metadata, blocks=count, transactions=transactions,
                   first_timestamp=first_timestamp,
                   sizes={key: statistics(values) for key, values in metrics.items()})
    for suffix in ["bytes"] + (["snappy_bytes"] if compressor else []):
        baseline = summary["sizes"][f"payload_{suffix}"]["total"]
        combined = summary["sizes"][f"payload_with_bal_{suffix}"]["total"]
        summary[f"increase_{suffix}"] = combined - baseline
        summary[f"increase_{suffix}_percent"] = (combined / baseline - 1) * 100
    with (args.out / "summary.json").open("x") as output:
        json.dump(summary, output, indent=2)
        output.write("\n")
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--rpc-url", default=os.environ.get("BASE_RPC_URL"), help="RPC URL (or BASE_RPC_URL)")
    parser.add_argument("--days", type=float, required=True, help="Lookback from the pinned end block's timestamp")
    parser.add_argument("--end-block", default="latest", help="Block tag or number (default: latest)")
    parser.add_argument("--workers", type=int, default=4, help="Concurrent block fetch/replay jobs (default: 4)")
    parser.add_argument("--timeout", type=float, default=120, help="Timeout per RPC in seconds")
    parser.add_argument("--out", type=Path, default=Path("bal-bandwidth"), help="New result directory")
    parser.add_argument("--payload-version", choices=["auto", "v3", "v4"], default="auto",
                        help="Auto detects Base Isthmus from requestsHash/withdrawalsRoot; override for custom forks")
    parser.add_argument("--save-raw", action="store_true", help="Retain raw block/BAL RPC data in gzip JSON files")
    parser.add_argument("--no-compression", action="store_true", help="Skip Snappy measurements")
    args = parser.parse_args()
    if not args.rpc_url:
        parser.error("provide --rpc-url or BASE_RPC_URL")
    if not math.isfinite(args.days) or args.days <= 0 or args.workers < 1:
        parser.error("--days and --workers must be positive")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be positive")
    if args.end_block.isdigit() or args.end_block.startswith("0x"):
        args.end_block = int(args.end_block, 0) if args.end_block.startswith("0x") else int(args.end_block)
    try:
        summary = collect(args, Rpc(args.rpc_url, args.timeout), None if args.no_compression else Snappy())
    except (RuntimeError, ValueError, OSError, KeyError) as error:
        # OSError can include an endpoint URL; don't print arbitrary transport details.
        message = str(error) if isinstance(error, (RuntimeError, ValueError)) else type(error).__name__
        print(f"Collection failed: {message}. No complete summary was produced.", file=sys.stderr)
        return 1
    print(f"{summary['blocks']:,} blocks, {summary['transactions']:,} transactions")
    print("Metric                         Total bytes     Mean/block     p95/block     p99/block")
    for key, values in summary["sizes"].items():
        print(f"{key:30} {values['total']:12,} {values['mean']:14,.1f} {values['p95']:13,} {values['p99']:13,}")
    for suffix in ["bytes"] + ([] if args.no_compression else ["snappy_bytes"]):
        print(f"Added {suffix}: {summary[f'increase_{suffix}']:,} "
              f"(+{summary[f'increase_{suffix}_percent']:.2f}%)")
    print(f"Results: {args.out.resolve()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
