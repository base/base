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
    cargo build --release

clean:
    cargo clean

watch-test:
    cargo watch -x test

watch-check:
    cargo watch -x "fmt --all -- --check" -x "clippy --all-targets -- -D warnings" -x test

mempool-db:
    docker run -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres -d

mempool-db-migrate:
    sea-orm-cli migrate refresh --database-url postgres://postgres:postgres@localhost:5432/postgres --migration-dir ./crates/mempool-tracer/migration

mempool-db-entities:
    sea-orm-cli generate entity --database-url postgres://postgres:postgres@localhost:5432/postgres -o ./crates/mempool-tracer/entity/src/ -l --expanded-format