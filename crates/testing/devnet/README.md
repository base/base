# Base devnet test support

`base-testing-devnet` provides in-process Base node and builder fixtures for
integration tests. `test_utils` contains the execution harness and direct engine
client; `builder_test_utils` provides block drivers, transaction builders, pool
observers, and optional external reference-node validation.

Production node assembly and builder configuration live in `base-node-service`.
RPC and payload definitions remain in their execution crates.
