#!/bin/sh
set -eu
env_file=$(realpath "$1")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for variant in first second; do
    mkdir "$work/$variant"
    printf '%s\n' "pub fn source_path() -> &'static str { file!() }" > "$work/$variant/fixture.rs"
    export NITRO_SOURCE_ROOT="$work/$variant"
    # shellcheck source=etc/docker/nitro-enclave/build-env.sh
    . "$env_file"
    # Cargo splits target-specific RUSTFLAGS on whitespace as well.
    # shellcheck disable=SC2086
    rustc --crate-name fixture --crate-type lib --emit=obj \
        "$work/$variant/fixture.rs" -o "$work/$variant/fixture.o" \
        $CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS
done
cmp "$work/first/fixture.o" "$work/second/fixture.o"
echo "Rust objects match across source-root paths"
