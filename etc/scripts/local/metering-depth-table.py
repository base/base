#!/usr/bin/env python3
"""Generate the storage-trie depth table for per-contract resource-metering caps.

The resource-metering schedule's `contractSlots.trieDepth` key assigns contracts a storage-trie
depth class; the builder compiles its `nodeBudget` into one changed-slot cap per class. This
script measures each candidate contract's depth over RPC and writes that `trieDepth` object.

    etc/scripts/local/metering-depth-table.py --rpc <url> --block <number> --node-budget 2100 \
        --addresses candidates.txt --previous current-trie-depth.json \
        --probe-log probes.jsonl --out trie-depth.json

Depth probe: one `eth_getProof` per contract at --block with six fixed pseudo-random storage keys.
For each key it counts the branch (17-item) nodes on the exclusion-proof path. In a keccak-keyed
trie with N leaves a random path meets about log16(N) branches, so the median of the six is a size
proxy, not the depth of any particular slot. The median is rounded half to even (a median of 4.5
is class 4, 5.5 is class 6), matching the analysis the default budget came from. Contracts with
empty storage and classes below --min-depth are left out; unlisted contracts get only the flat
`changedSlotLimit`, if the schedule sets one. With the 2,100-node budget, classes 3 and below have
no cap, so --min-depth defaults to 4.

Inputs:
    --addresses   files with one address per line (blank lines and `#` comments ignored).
    --previous    an earlier `trieDepth` JSON; its contracts are re-probed and class changes are
                  reported on stderr. A contract whose probe fails keeps its previous class.
    --from-probe-log  rebuild the table from an earlier --probe-log without RPC calls.

Refresh procedure (weekly, or after a large state change):
    1. Pick candidates: the previous table plus the storage owners with the most changed slots over
       a recent window (for example the top 20,000 owners by changed slots over the last 7 days).
    2. Pin --block to a recent finalized block so the run is reproducible, and keep --probe-log.
    3. Run with --previous, review the class changes printed on stderr, and commit the new table to
       the schedule's config repository together with the block number used.
    4. The builder loads the schedule once at startup, so the new table applies after a restart.

Checks: python3 etc/scripts/local/test_metering_depth_table.py
"""

import argparse
import hashlib
import json
import statistics
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor

PROBE_KEYS = ["0x" + hashlib.sha256(f"depth-probe-{i}".encode()).hexdigest() for i in range(6)]
EMPTY_STORAGE_ROOT = "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
BRANCH_NODE_ITEMS = 17
MAX_TRIE_DEPTH = 64


def rlp_list_items(node_hex):
    """Returns the number of top-level items in an RLP-encoded list, or 0 for a non-list."""
    data = bytes.fromhex(node_hex[2:] if node_hex.startswith("0x") else node_hex)
    prefix = data[0]
    if prefix < 0xC0:
        return 0
    if prefix < 0xF8:
        start, length = 1, prefix - 0xC0
    else:
        length_of_length = prefix - 0xF7
        start = 1 + length_of_length
        length = int.from_bytes(data[1:start], "big")
    index, items = start, 0
    while index < start + length:
        item = data[index]
        if item < 0x80:
            index += 1
        elif item < 0xB8:
            index += 1 + item - 0x80
        elif item < 0xC0:
            length_of_length = item - 0xB7
            index += 1 + length_of_length + int.from_bytes(
                data[index + 1 : index + 1 + length_of_length], "big"
            )
        elif item < 0xF8:
            index += 1 + item - 0xC0
        else:
            length_of_length = item - 0xF7
            index += 1 + length_of_length + int.from_bytes(
                data[index + 1 : index + 1 + length_of_length], "big"
            )
        items += 1
    return items


def probe_record(address, proof):
    """Builds one probe-log record from an `eth_getProof` result."""
    branches = [
        sum(1 for node in storage_proof["proof"] if rlp_list_items(node) == BRANCH_NODE_ITEMS)
        for storage_proof in proof["storageProof"]
    ]
    return {
        "a": address.lower(),
        "br": branches,
        "br_med": statistics.median(branches),
        "acct_len": len(proof["accountProof"]),
        "empty": proof["storageHash"].lower() == EMPTY_STORAGE_ROOT,
    }


def depth_class(record):
    """Returns the record's depth class, or None for empty storage or a failed probe."""
    if "error" in record or record.get("empty"):
        return None
    depth = round(record["br_med"])
    return depth if 1 <= depth <= MAX_TRIE_DEPTH else None


def build_table(records, min_depth, node_budget, previous=None):
    """Builds a `trieDepth` object. Failed probes keep their class from `previous`."""
    previous = previous or {}
    classes = {}
    for record in records:
        depth = depth_class(record)
        if "error" in record:
            depth = previous.get(record["a"].lower())
        if depth is not None and depth >= min_depth:
            classes.setdefault(depth, set()).add(record["a"].lower())
    table = {
        "classes": [
            {"depth": depth, "contracts": sorted(classes[depth])}
            for depth in sorted(classes, reverse=True)
        ]
    }
    if node_budget is not None:
        table = {"nodeBudget": node_budget, **table}
    return table


def table_classes(table):
    """Maps each contract in a `trieDepth` object to its depth class."""
    return {
        contract.lower(): entry["depth"]
        for entry in table.get("classes", [])
        for contract in entry["contracts"]
    }


def class_changes(old, new):
    """Returns sorted (address, old_class, new_class) for every contract whose class changed."""
    return sorted(
        (address, old.get(address), new.get(address))
        for address in set(old) | set(new)
        if old.get(address) != new.get(address)
    )


def read_addresses(paths):
    addresses = []
    for path in paths:
        with open(path) as file:
            for line in file:
                line = line.split("#", 1)[0].strip()
                if line:
                    addresses.append(line.lower())
    return addresses


def rpc_call(url, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(url, data=body, headers={"content-type": "application/json"})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                out = json.loads(response.read())
            if "error" in out:
                raise RuntimeError(out["error"])
            return out["result"]
        except Exception:  # noqa: BLE001
            if attempt == 2:
                raise
    raise AssertionError("unreachable")


def probe(url, block, address):
    try:
        return probe_record(address, rpc_call(url, "eth_getProof", [address, PROBE_KEYS, block]))
    except Exception as error:  # noqa: BLE001
        return {"a": address.lower(), "error": str(error)[:200]}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--rpc", help="execution RPC URL that serves eth_getProof")
    parser.add_argument("--block", help="block number to probe at (default: latest)")
    parser.add_argument("--addresses", nargs="*", default=[], help="candidate address files")
    parser.add_argument("--previous", help="earlier trieDepth JSON to re-probe and diff against")
    parser.add_argument("--from-probe-log", help="rebuild from an earlier probe log, without RPC")
    parser.add_argument("--probe-log", help="write one JSON line per probed contract here")
    parser.add_argument("--node-budget", type=int, help="nodeBudget to write into the table")
    parser.add_argument("--min-depth", type=int, default=4, help="smallest class to list")
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--out", help="output path (default: stdout)")
    args = parser.parse_args(argv)

    previous_table = {}
    if args.previous:
        with open(args.previous) as file:
            previous_table = json.load(file)
    previous = table_classes(previous_table)

    if args.from_probe_log:
        with open(args.from_probe_log) as file:
            records = [json.loads(line) for line in file if line.strip()]
    else:
        if not args.rpc:
            parser.error("--rpc is required unless --from-probe-log is given")
        block = hex(int(args.block)) if args.block else "latest"
        candidates = sorted(set(read_addresses(args.addresses)) | set(previous))
        with ThreadPoolExecutor(args.concurrency) as pool:
            records = list(pool.map(lambda address: probe(args.rpc, block, address), candidates))
        if args.probe_log:
            with open(args.probe_log, "w") as file:
                for record in records:
                    file.write(json.dumps(record, separators=(",", ":")) + "\n")

    node_budget = args.node_budget if args.node_budget is not None else previous_table.get("nodeBudget")
    table = build_table(records, args.min_depth, node_budget, previous)
    output = json.dumps(table, indent=2) + "\n"
    if args.out:
        with open(args.out, "w") as file:
            file.write(output)
    else:
        sys.stdout.write(output)

    failed = sum(1 for record in records if "error" in record)
    listed = table_classes(table)
    counts = {entry["depth"]: len(entry["contracts"]) for entry in table["classes"]}
    print(f"probed={len(records)} failed={failed} listed={len(listed)} by_class={counts}", file=sys.stderr)
    if previous_table:
        changes = class_changes(previous, listed)
        print(f"class changes: {len(changes)}", file=sys.stderr)
        for address, old, new in changes:
            print(f"  {address} {old} -> {new}", file=sys.stderr)
    if node_budget is None:
        print("warning: no --node-budget; add nodeBudget before loading the table", file=sys.stderr)


if __name__ == "__main__":
    main()
