#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=baseup/baseup
source "$SCRIPT_DIR/baseup"

TEST_ARCH=""

uname() {
    case "$1" in
        -m) printf '%s\n' "$TEST_ARCH" ;;
        -s) printf '%s\n' "Linux" ;;
        *) return 1 ;;
    esac
}

assert_arch() {
    local input="$1"
    local expected="$2"
    local actual

    TEST_ARCH="$input"
    actual="$(detect_arch)"
    if [[ "$actual" != "$expected" ]]; then
        printf 'detect_arch %s: expected %s, got %s\n' "$input" "$expected" "$actual" >&2
        exit 1
    fi
}

for arch in x86_64 x64 amd64; do
    assert_arch "$arch" amd64
done

for arch in arm64 aarch64; do
    assert_arch "$arch" arm64
done

TEST_ARCH="riscv64"
if output="$(detect_arch 2>&1)"; then
    printf 'detect_arch riscv64: expected failure, got success\n' >&2
    exit 1
fi

if [[ "$output" != *"Unsupported architecture: riscv64"* ]]; then
    printf 'detect_arch riscv64: missing unsupported-architecture error: %s\n' "$output" >&2
    exit 1
fi

printf 'baseup architecture tests passed\n'
