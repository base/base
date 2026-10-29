# base-common-evm2

Base's execution engine on [EVM2](https://github.com/alloy-rs/evm2), the EVM that Reth uses
in place of revm. It is the node-side counterpart of the revm engine in `base-common-evm`,
which the proof stack keeps using. The two engines must agree byte for byte on every
consensus-relevant result, so this crate is differentially tested against `base-common-evm`.

EVM2 composes an engine from one associated-type family, [`evm2::EvmTypesHost`], plus a
transaction registry and static handler hooks. This crate defines that family for Base as
[`BaseEvmTypes`].

## Implemented

- **Fork schedule**: [`BaseSpecId`] wraps a `BaseUpgrade` and maps it to the EVM2 spec,
  matching the revm mapping.
- **Deposit transactions** (type `0x7e`): the mint is credited and kept whatever the outcome,
  system transactions are rejected (Regolith rules), halts are charged the full gas limit, and
  no L2 fees are charged.
- **Standard transactions** (legacy, EIP-2930, EIP-1559, EIP-7702; EIP-4844 is rejected):
  [`BaseTxHandlerHooks`] charges the L1 data fee and the Isthmus operator fee upfront, rejects
  callers who cannot pay them, and pays the L1 fee, base fee, and operator fee into their
  vaults.
- **Block executor**: [`BaseBlockExecutor`] runs the EIP-4788 and EIP-2935 pre-execution
  calls, checks each transaction against the remaining block gas (reserving the payer's
  worst-case authentication gas for EIP-8130 transactions), meters the Jovian DA footprint into
  `blob_gas_used`, and builds receipts with the running cumulative gas.
- **Transition-block hooks**, applied as [`IrregularStateChange`]s with fork timing from
  [`BaseForkActivations`]: the Canyon create2 deployer, the Denim `BaseTime` predeploy, and the
  Everest EIP-8130 system-account stubs.
- **Precompiles**: Fjord `P256VERIFY`, the Granite and Jovian bn254 pairing input caps, and the
  Isthmus and Jovian BLS12-381 input caps. Azul's EIP-7825 per-transaction gas cap comes from the
  Osaka spec.
- **EIP-8130 primitives**: the envelope variant, the [`NonceManager`] storage layout (read and
  increment), and the intrinsic-gas schedule re-exported from `base-common-eip8130`.

## Pending

- **Stateful Beryl and Cobalt precompiles** (B20 factory, registries, `TxContext`,
  `NonceManager`). These will share one engine-neutral implementation with `base-common-evm`.
- **EIP-8130 execution**: [`BaseBlockExecutor`] rejects EIP-8130 transactions with
  [`Eip8130ExecutionUnsupported`].
- **Reth bridge**: wiring [`BaseBlockExecutor`] into Reth's block-executor and EVM-config traits.

## Parity tests

The `tests/*_parity.rs` harnesses run the same input through both engines and compare the
results across the execution fork ladder (`BaseUpgrade::EXECUTION_VARIANTS`):

- `deposit_parity`: deposit outcome, gas, created address, and post-state, Regolith onward
  (Bedrock deposit rules are not implemented because Base launched with Regolith active).
- `standard_fee_parity`: gas used and every fee recipient's balance for a standard transaction.
- `block_parity`: per-transaction success and cumulative gas for a block of a deposit followed
  by standard transactions.
- `transition_parity`: the state each transition-block hook installs.
