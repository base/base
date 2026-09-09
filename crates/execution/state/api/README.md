# `base-execution-state-api`

Storage access contracts shared by persistent providers, trie computation, sync, and execution.
Database and memory-state types live below these interfaces; provider implementations live above them.

The default `std` profile includes asynchronous subscriptions. `db-api` adds database-backed
provider contracts and storage settings. Allocation-only read interfaces support `no_std`.

```sh
cargo test -p base-execution-state-api
cargo check -p base-execution-state-api --no-default-features --target riscv32imac-unknown-none-elf
```
