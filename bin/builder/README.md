# `base-builder`

The block builder for the Base Stack.

Under the hood, `base-builder` runs [base-builder-core](../../crates/builder/core/).

Restore a snapshot before starting on an existing chain. Starting at block zero requires
`--allow-genesis-start`, which is only for launching a brand-new chain.
