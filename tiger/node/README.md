# base-tiger

The `tiger` binary and the node code behind it: today the differential fuzz adapter
(`tiger diff-adapter`, see `etc/tools/diff-fuzz/PROTOCOL.md`) and the stateless witness state it
executes blocks over. The Engine API, JSON-RPC and chain storage land here next.

## Edge coverage

With `BASE_DIFF_COVERAGE=1` the adapter adds a `coverage` map of `tiger:edge:<index>` features to
build-mode responses. The counters exist only in an instrumented build:

```sh
cd tiger
SANCOV_CRATES="base_tiger_evm base_tiger evm2" \
RUSTC_WRAPPER=$PWD/../etc/tools/diff-adapter/sancov-rustc.sh CARGO_TARGET_DIR=target/sancov \
  cargo build --release -p base-tiger --features edge-coverage
```
