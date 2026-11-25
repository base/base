#!/usr/bin/env bash
# Builds a profile-guided `tiger` trained on `tiger bench replay` (see README.md, "Optimized
# builds").
#
#   ./pgo.sh --datadir <base mainnet datadir> --blocks <blocks.bin> [--count N] [--chain base]
#            [--profile maxperf]
#
# 1. builds tiger with `-Cprofile-generate` into target/pgo-generate;
# 2. replays --count blocks (default 500) after the datadir's tip with that build, which writes
#    raw profiles into target/pgo-data;
# 3. merges them with the llvm-profdata of rustup's llvm-tools component, which matches rustc's
#    LLVM (`rustup component add llvm-tools` installs it);
# 4. rebuilds with `-Cprofile-use` into target/pgo-use.
#
# The result is target/pgo-use/<host>/<profile>/tiger. --profile is the cargo profile of both
# builds: maxperf (fat LTO, one codegen unit; the default) or release. The replay never writes
# the datadir. Profile data stays under target/ and is never committed; `cargo build --release`
# stays the plain build. `--target <host>` keeps the flags off build scripts and proc macros.
# C dependencies built through the cc crate (libmdbx, secp256k1) get the flags too, but only a
# clang with rustc's LLVM version reads the profile; others warn and build them without it.
set -euo pipefail
cd "$(dirname "$0")"

chain=base datadir="" blocks="" count=500 profile=maxperf
while [ $# -gt 0 ]; do
    case $1 in
        --datadir) datadir=$2; shift 2 ;;
        --blocks) blocks=$2; shift 2 ;;
        --count) count=$2; shift 2 ;;
        --chain) chain=$2; shift 2 ;;
        --profile) profile=$2; shift 2 ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done
if [ -z "$datadir" ] || [ -z "$blocks" ]; then
    echo "usage: $0 --datadir <dir> --blocks <file> [--count N] [--chain base] [--profile maxperf|release]" >&2
    exit 2
fi
datadir=$(realpath "$datadir") blocks=$(realpath "$blocks")

host=$(rustc -vV | sed -n 's/^host: //p')
profdata="$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata"
if [ ! -x "$profdata" ]; then
    echo "$profdata is missing: run \`rustup component add llvm-tools\`" >&2
    exit 1
fi
data=$PWD/target/pgo-data
rm -rf "$data"
mkdir -p "$data"

echo "pgo: building the instrumented binary ($profile profile)" >&2
RUSTFLAGS="-Cprofile-generate=$data" \
    cargo build --profile "$profile" -p base-tiger --bin tiger --target "$host" --target-dir target/pgo-generate

echo "pgo: training on $count blocks after the tip of $datadir" >&2
"target/pgo-generate/$host/$profile/tiger" bench replay --chain "$chain" --datadir "$datadir" --blocks "$blocks" --count "$count"

"$profdata" merge -o "$data/merged.profdata" "$data"/*.profraw
# Cargo does not track the profile's contents, only the flag: name the profile by its hash so a
# new profile rebuilds every crate instead of reusing ones optimized with an older profile.
profile_data="$data/$(sha256sum "$data/merged.profdata" | cut -c1-16).profdata"
mv "$data/merged.profdata" "$profile_data"

echo "pgo: building the optimized binary" >&2
RUSTFLAGS="-Cprofile-use=$profile_data" \
    cargo build --profile "$profile" -p base-tiger --bin tiger --target "$host" --target-dir target/pgo-use

echo "pgo: built target/pgo-use/$host/$profile/tiger" >&2
