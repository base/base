#!/bin/sh
set -eu

# Cargo's musl host build scripts must dlopen libclang; target binaries stay static.
compiler=$1
shift
for argument do
    case "$argument" in
        build_script_*) exec "$compiler" "$@" -C target-feature=-crt-static ;;
    esac
done
exec "$compiler" "$@"
