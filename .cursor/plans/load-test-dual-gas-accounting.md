# Load-test dual gas accounting

- [x] Trace gas-limit and calibrated-gas accounting through pacing and tracking.
- [x] Carry estimated execution gas with signed and accepted transactions.
- [x] Pace GPS and mempool depth using estimated gas while retaining gas limits for signing.
- [x] Update metrics and tests to distinguish offered estimated gas from reserved gas.
- [x] Run load-test unit tests and strict Clippy.

## Accepted mempool depth

- [x] Exclude locally queued transactions from mempool depth.
- [x] Retain queued transactions in sender and aggregate capacity limits.
- [x] Count RPC-bound cycles only when a refill offered transactions.
- [x] Run load-test unit tests and strict Clippy.
