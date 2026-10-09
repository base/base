#!/usr/bin/env python3
"""Summarize a samply / Firefox Profiler profile without third-party dependencies.

Reports, over all samples (or only samples whose stack contains ``--root``):
  * top-N functions by self time,
  * top-N functions by inclusive time,
  * bucket totals (serialization, allocation, hashing, EVM, ...): self (each
    sample's leaf frame goes to the first matching bucket; sums to 100% with
    "other") and inclusive (sample counted if any frame on its stack matches).

Percentages are relative to the selected samples. Frames are the outermost
symbol at each address; inlined callees are attributed to their caller.
Addresses with no symbol are shown as ``<library>+0x<addr>``.

Symbolication: record with ``samply record --save-only --unstable-presymbolicate
-o prof.json.gz -- <cmd>``. That writes ``prof.json.syms.json`` next to the
profile; this script loads it automatically (or pass ``--syms``) and resolves
the hex addresses samply leaves in the string table. Profiles symbolicated in
the Firefox Profiler UI and re-downloaded also work without a sidecar.

Usage:
  summarize_profile.py prof.json.gz [--root execute_best_transactions] [-n 15]
                                    [--thread NAME] [--syms prof.json.syms.json]
"""

import argparse
import bisect
import gzip
import json
import os
import re
import sys
from collections import Counter

BUCKETS = [
    ("serialization", r"serde|core::fmt|alloc::fmt|::fmt::|Display|Debug>::fmt|to_string|write_fmt|itoa|ryu|hex::|format_escaped|FromIterator<char>"),
    ("allocation", r"malloc|free\b|realloc|calloc|memmove|memcpy|memset|bzero|_platform_mem|libsystem_malloc|jemalloc|_rjem_|je_|rtree_|zone_size|tcache|arena_|alloc::alloc|RawVec|__rust_alloc|__rust_dealloc|__rust_realloc|drop_in_place"),
    ("hashing", r"keccak|sha2|sha256|Sha256|hash_slow|Hasher|::hash\b|Hash for|hashbrown|foldhash|ahash|siphash|SipHash|fxhash"),
    ("metrics", r"metrics|prometheus|Histogram|Counter|Gauge|quanta"),
    ("tracing", r"tracing|Span|Subscriber|EnvFilter|callsite"),
    ("locks", r"Mutex|RwLock|parking_lot|pthread_mutex|psynch|_os_unfair_lock|futex|Condvar|semaphore|crossbeam|mpsc|lock_api|OnceLock"),
    ("evm", r"revm|interpreter|Interpreter|alloy_evm|base_common_evm|precompile"),
    ("state_access", r"State<|CacheDB|InMemoryDB|BundleState|CacheState|TransitionState|Database>::|load_account|storage_ref|code_by_hash|journal"),
]

def open_profile(path):
    opener = gzip.open if path.endswith(".gz") else open
    with opener(path, "rt") as f:
        return json.load(f)


class Symbols:
    """Address -> name lookup from a samply ``.syms.json`` sidecar."""

    def __init__(self, path):
        self.libs = {}
        if not path or not os.path.exists(path):
            return
        with open(path) as f:
            data = json.load(f)
        strings = data.get("string_table", [])
        for entry in data.get("data", []):
            rows = sorted(
                (s["rva"], s["rva"] + s.get("size", 0), strings[s["symbol"]])
                for s in entry.get("symbol_table", [])
            )
            keys = [entry.get("debug_name"), entry.get("code_id"), entry.get("debug_id")]
            for key in filter(None, keys):
                self.libs[key] = ([r[0] for r in rows], rows)

    def lookup(self, lib, address):
        if lib is None or address is None:
            return None
        for key in (lib.get("debugName"), lib.get("codeId"), lib.get("breakpadId"), lib.get("name")):
            table = self.libs.get(key) if key else None
            if table:
                starts, rows = table
                i = bisect.bisect_right(starts, address) - 1
                if i >= 0 and (rows[i][1] == rows[i][0] or address < rows[i][1]):
                    return rows[i][2]
        return None


def thread_strings(profile, thread):
    shared = profile.get("shared", {})
    return thread.get("stringArray") or shared.get("stringArray") or thread.get("stringTable", {}).get("_array", [])


def build_func_names(profile, thread, symbols):
    """Returns a per-frame display name list."""
    strings = thread_strings(profile, thread)
    libs = profile.get("libs", [])
    frames = thread["frameTable"]
    funcs = thread["funcTable"]
    resources = thread.get("resourceTable", {})
    names = []
    hex_re = re.compile(r"^0x[0-9a-fA-F]+$")
    for i in range(frames["length"]):
        func = frames["func"][i]
        name = strings[funcs["name"][func]]
        if hex_re.match(name) or not name:
            lib = None
            res = funcs.get("resource", [None] * (func + 1))[func]
            if res is not None and res >= 0 and resources:
                lib_index = resources["lib"][res]
                lib = libs[lib_index] if lib_index is not None else None
            address = frames.get("address", [None] * (i + 1))[i]
            resolved = symbols.lookup(lib, address)
            if resolved is None and hex_re.match(name):
                resolved = symbols.lookup(lib, int(name, 16))
            if resolved:
                name = resolved
            elif lib is not None:
                name = f"{lib.get('name', '?')}+{name}"
        names.append(name)
    return names


def stacks_to_frames(thread):
    stacks = thread["stackTable"]
    prefix = stacks["prefix"]
    frame = stacks["frame"]
    return prefix, frame


def pick_threads(profile, wanted):
    threads = profile["threads"]
    if wanted:
        threads = [t for t in threads if wanted in t.get("name", "")]
    return [t for t in threads if t["samples"]["length"]]


def summarize(profile, root, thread_filter, symbols):
    self_counts = Counter()
    incl_counts = Counter()
    bucket_counts = Counter()
    bucket_incl = Counter()
    bucket_res = [(name, re.compile(rx)) for name, rx in BUCKETS]
    total = 0
    all_samples = 0
    for thread in pick_threads(profile, thread_filter):
        names = build_func_names(profile, thread, symbols)
        prefix, frame = stacks_to_frames(thread)
        samples = thread["samples"]
        weights = samples.get("weight") or [1] * samples["length"]
        cache = {}
        for stack, weight in zip(samples["stack"], weights):
            if stack is None:
                continue
            weight = weight or 1
            all_samples += weight
            if stack not in cache:
                chain = []
                s = stack
                while s is not None:
                    chain.append(names[frame[s]])
                    s = prefix[s]
                cache[stack] = chain  # leaf first
            chain = cache[stack]
            if root:
                idx = next((i for i, n in enumerate(chain) if root in n), None)
                if idx is None:
                    continue
                chain = chain[: idx + 1]
            total += weight
            leaf = chain[0]
            self_counts[leaf] += weight
            for name in set(chain):
                incl_counts[name] += weight
            for bucket, rx in bucket_res:
                if rx.search(leaf):
                    bucket_counts[bucket] += weight
                    break
            else:
                bucket_counts["other"] += weight
            for bucket, rx in bucket_res:
                if any(rx.search(name) for name in chain):
                    bucket_incl[bucket] += weight
    return total, all_samples, self_counts, incl_counts, bucket_counts, bucket_incl


def callers(profile, target, root, thread_filter, symbols, depth):
    """Counts the `depth` frames above each frame matching `target`, as caller paths.

    With `root`, only samples whose stack also contains `root` are counted."""
    paths = Counter()
    total = 0
    for thread in pick_threads(profile, thread_filter):
        names = build_func_names(profile, thread, symbols)
        prefix, frame = stacks_to_frames(thread)
        samples = thread["samples"]
        weights = samples.get("weight") or [1] * samples["length"]
        for stack, weight in zip(samples["stack"], weights):
            if stack is None:
                continue
            chain = []
            s = stack
            while s is not None:
                chain.append(names[frame[s]])
                s = prefix[s]
            # Outermost match, so recursion does not split one call site into several paths.
            idx = next((i for i in range(len(chain) - 1, -1, -1) if target in chain[i]), None)
            if idx is None or (root and not any(root in n for n in chain)):
                continue
            weight = weight or 1
            total += weight
            paths[tuple(chain[idx + 1 : idx + 1 + depth])] += weight
    return total, paths


def short(name, width=140):
    return name if len(name) <= width else name[: width - 3] + "..."


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("profile")
    parser.add_argument("--root", help="only count samples whose stack contains a frame matching this substring")
    parser.add_argument("-n", "--top", type=int, default=15)
    parser.add_argument("--thread", help="only include threads whose name contains this substring")
    parser.add_argument("--callers", help="report caller paths of frames matching this substring")
    parser.add_argument("--depth", type=int, default=4, help="caller path depth for --callers")
    parser.add_argument("--syms", help="samply .syms.json sidecar (default: <profile minus .gz>.syms.json)")
    args = parser.parse_args()

    syms_path = args.syms
    if syms_path is None:
        base = args.profile[:-3] if args.profile.endswith(".gz") else args.profile
        syms_path = re.sub(r"\.json$", "", base) + ".json.syms.json"
    symbols = Symbols(syms_path)
    profile = open_profile(args.profile)
    if args.callers:
        total, paths = callers(profile, args.callers, args.root, args.thread, symbols, args.depth)
        if total == 0:
            sys.exit(f"no samples matched (callers={args.callers!r})")
        print(f"samples containing '{args.callers}': {total}\n")
        print(f"Top {args.top} caller paths (innermost caller first)")
        for path, count in paths.most_common(args.top):
            print(f"  {100 * count / total:6.2f}%  " + "\n           <- ".join(short(n, 120) for n in path))
        return
    total, all_samples, self_c, incl_c, buckets, buckets_incl = summarize(profile, args.root, args.thread, symbols)
    if total == 0:
        sys.exit(f"no samples matched (root={args.root!r}, thread={args.thread!r})")

    scope = f"under '{args.root}'" if args.root else "all samples"
    print(f"samples {scope}: {total} of {all_samples} ({100 * total / all_samples:.1f}%)")
    print(f"symbols: {'loaded ' + syms_path if symbols.libs else 'none (names from profile string table)'}\n")
    print(f"Top {args.top} self time ({scope})")
    for name, count in self_c.most_common(args.top):
        print(f"  {100 * count / total:6.2f}%  {short(name)}")
    print(f"\nTop {args.top} inclusive time ({scope})")
    for name, count in incl_c.most_common(args.top):
        print(f"  {100 * count / total:6.2f}%  {short(name)}")
    print(f"\nBuckets ({scope})")
    print("    self%   incl%  bucket   (self: leaf frame, first match wins; incl: any frame matches)")
    for bucket, _ in BUCKETS + [("other", None)]:
        incl = f"{100 * buckets_incl[bucket] / total:6.2f}%" if bucket != "other" else "      -"
        print(f"  {100 * buckets[bucket] / total:6.2f}% {incl}  {bucket}")


if __name__ == "__main__":
    main()
