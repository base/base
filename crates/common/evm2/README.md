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
- **L1 fee parameters**: [`L1Block`] reads them from the `L1Block` predeploy into the block
  environment extension once per block, at the block's first non-deposit transaction (after the
  L1-info deposit has run), like the revm reference's `L1BlockInfo` cache. An EVM built with
  [`BaseEvmExt::l1_fee_params_block`] already set to the block number uses the extension as given.
- **Block executor**: [`BaseBlockExecutor`] runs the EIP-4788 and EIP-2935 pre-execution
  calls, checks each transaction against the remaining block gas (reserving the payer's
  worst-case authentication gas for EIP-8130 transactions), meters the Jovian DA footprint into
  `blob_gas_used`, and builds `BaseReceipt`s with the running cumulative gas (post-Regolith
  deposit receipts carry the depositor's pre-execution nonce, and from Canyon the receipt
  version). Every state change is committed through a [`BaseBlockState`]; the default
  [`BlockStateAccumulator`](evm2::BlockStateAccumulator) collects the block delta, and the Reth
  bridge in `base-execution-evm` supplies one that builds Reth's bundle state and streams state
  updates. Transactions can be executed and committed in one step or two
  ([`BaseExecutedTx`]).
- **Transition-block hooks**, applied as [`IrregularStateChange`]s with fork timing from
  [`BaseForkActivations`]: the Canyon create2 deployer, the Denim `BaseTime` predeploy, and the
  Everest EIP-8130 system-account stubs.
- **Precompiles**: Fjord `P256VERIFY`, the Granite and Jovian bn254 pairing input caps, and the
  Isthmus and Jovian BLS12-381 input caps, all selected by `BaseEvmTypes::precompiles`. Azul's
  EIP-7825 per-transaction gas cap comes from the Osaka spec.
- **Reth traits** (feature `reth`): `FromRecoveredTx`/`FromTxWithEncoded` from
  `BaseTransactionSigned`, `TransactionEnvMut`, and `BlockTransactionResult` for
  [`BaseExecutedTx`]. Simulation overrides of the gas limit or access list keep the transaction's
  original EIP-2718 bytes, so the L1 data fee is priced over the transaction as signed.
- **EIP-8130 primitives**: the envelope variant, the [`NonceManager`] storage layout (read and
  increment), and the intrinsic-gas schedule re-exported from `base-common-eip8130`.

## Pending

- **Stateful Beryl and Cobalt precompiles** (B20 factory, registries, `TxContext`,
  `NonceManager`). These will share one engine-neutral implementation with `base-common-evm` and
  be installed by `BaseEvmTypes::precompiles`, the single precompile-set seam both this crate
  and the Reth bridge build EVMs through.
- **EIP-8130 execution**: [`BaseBlockExecutor`] rejects EIP-8130 transactions with
  [`Eip8130ExecutionUnsupported`].

## Parity tests

The `tests/*_parity.rs` harnesses run the same input through both engines and compare the
results across the execution fork ladder (`BaseUpgrade::EXECUTION_VARIANTS`):

- `deposit_parity`: deposit outcome, gas, created address, and post-state, Regolith onward
  (Bedrock deposit rules are not implemented because Base launched with Regolith active).
- `standard_fee_parity`: gas used and every fee recipient's balance for a standard transaction.
- `block_parity`: per-transaction success and cumulative gas for a block of a deposit followed
  by standard transactions.
- `transition_parity`: the state each transition-block hook installs.
- `l1_block_parity`: the L1 fee parameters read from the `L1Block` predeploy storage.
