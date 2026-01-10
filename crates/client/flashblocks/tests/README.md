## Flashblocks RPC Integration Tests

The suites under this directory exercise `base-flashblocks-rpc` the same way external
consumers do — by linking against the published library instead of the crate's `cfg(test)` build.
Running them from `tests/` ensures:

1. **Single compilation unit:** both the tests and `base_reth_test_utils` depend on the standard
   library build, so types like `Flashblock` stay aligned (no duplicate definitions across lib/test
   targets and no `E0308` mismatches on channels).
2. **Public API coverage:** integration tests only touch exported items, which makes it easy to
   detect when a public surface change would break downstream users.

If you are adding new flashblocks tests that need access to private internals, keep those as unit
tests under `src/`. All cases that can be expressed via the public API should live here.
