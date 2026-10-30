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
  EIP-7825 per-transaction gas cap comes from the Osaka spec. `BaseEvmTypes::native_precompiles`
  layers the stateful native precompiles on top, gated like the revm `BasePrecompiles`: the B-20
  factory and tokens, the policy registry, and the activation registry from Beryl, and the
  EIP-8130 `TxContext` and `NonceManager` from Cobalt. They read `tx.origin` from
  [`BaseEvmExt::tx_origin`], which every transaction handler sets.
- **EIP-8130 transactions** (type `0x79`, Everest onward): [`Eip8130Handler`] executes them
  through the engine-neutral pipeline in `base-execution-eip8130` (shared with the revm
  reference): sender/payer and account-change authorization, account creation, configuration
  changes and delegation, the 2D nonce manager and the nonce-free replay ring with the
  millisecond validity window, the EIP-8130 intrinsic gas schedule, a worst-case payer pre-charge
  (gas, L1 data fee, operator fee), phased calls that commit or revert in order, and settlement
  (refund cap, EIP-7623 floor, vault credits). The receipt is an `Eip8130Receipt` carrying
  `committedPhases`, reported through [`BaseTxResultExt::eip8130_committed_phases`]. The envelope
  ([`Eip8130Tx`]) carries the EIP-2718 bytes that price intrinsic gas, the L1 fee, and the DA
  footprint.
- **EIP-8130 simulation** for RPC: an envelope built with `BaseTxEnvelope::eip8130_simulation`
  runs the calls from the recovered signer without verifying signatures, touching the nonce, or
  settling fees, and rolls all state back while keeping the logs.
  [`Eip8130SimulationMode::Call`] reports one run's metered gas (`eth_call`);
  [`Eip8130SimulationMode::Estimate`] reports the smallest gas limit at which the calls succeed
  (`eth_estimateGas`). Execute it with `Evm::call_tx`.
- **Reth traits** (feature `reth`): `FromRecoveredTx`/`FromTxWithEncoded` from
  `BaseTransactionSigned`, `TransactionEnvMut`, and `BlockTransactionResult` for
  [`BaseExecutedTx`]. Simulation overrides of the gas limit or access list keep the transaction's
  original EIP-2718 bytes, so the L1 data fee is priced over the transaction as signed.
- **EIP-8130 primitives**: the [`NonceManager`] storage layout (untracked reads and increment)
  and the intrinsic-gas schedule re-exported from `base-common-eip8130`.

## Pending

- **EIP-8130 tracing**: an inspector sees each EIP-8130 call as its own top-level frame, and
  estimation probes are inspected too. The revm reference wraps the calls in one synthetic root
  frame and hides the probes.

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
- `native_precompile_parity`: calls to the stateful native precompiles, Azul onward.
- `eip8130_parity`: signed EIP-8130 transactions (self-pay, phased reverts and halts, payer
  sponsorship, create, delegation, configuration change, policy gate, 2D and nonce-free nonces,
  the validity window, refunds, rejections, and simulation), comparing inclusion or rejection,
  status, gas, output, logs, committed phases, and every touched account and slot; plus a block
  mixing deposit, standard, and EIP-8130 transactions compared receipt by receipt, with the
  DA footprint and post-block state.
