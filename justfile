default:
    @just --list

check: check-format check-clippy test

fix: fix-format fix-clippy

test:
    cargo test --workspace --all-features

check-format:
    cargo fmt --all -- --check

fix-format:
    cargo fmt --all

check-clippy:
    cargo clippy --all-targets -- -D warnings

fix-clippy:
    cargo clippy --all-targets --fix --allow-dirty --allow-staged

build:
    cargo build --release --bin base-reth-node

build-maxperf:
    cargo build --profile maxperf --bin base-reth-node --features jemalloc

clean:
    cargo clean

watch-test:
    cargo watch -x test

watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-targets -- -D warnings" -x test

lint: check-format check-clippy

doc:
    cargo doc --workspace --all-features --no-deps --open
