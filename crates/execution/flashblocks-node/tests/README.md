## Flashblocks RPC Integration Tests

The suites under this directory exercise `base-flashblocks-node` by linking against its library
build with the `test-utils` feature enabled, not the crate's `cfg(test)` build.
Running them from `tests/` ensures:

1. **Shared library types:** integration tests use the library's exported types and `base_test_utils`
   for shared test helpers, without a separate `cfg(test)` build of the library.
2. **Public API coverage:** integration tests only touch exported items, which makes it easy to
   detect when a public surface change would break downstream users.

If you are adding new flashblocks tests that need access to private internals, keep those as unit
tests under `src/`. All cases that can be expressed via the public API should live here.
