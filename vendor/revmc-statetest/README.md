# revmc-statetest

[Ethereum state test][tests] runner for revmc.

Runs the official [GeneralStateTests][tests] against three execution modes:

The runner is vendored from revm's `revme` with revmc-specific extensions
for compilation, custom handler integration, and diagnostic diffing between
interpreter and compiled execution.

## Usage

This crate is not published and is used internally by `revmc` (state test integration tests)
and `revmc-cli` (the `statetest` and `statetest-diff` subcommands).

See the [main README](/README.md#testing) for instructions on running state tests.

[tests]: https://github.com/ethereum/tests
