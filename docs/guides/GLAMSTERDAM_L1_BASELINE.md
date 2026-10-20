# Glamsterdam L1-parameter test baseline

This guide describes the Amsterdam execution spec inherited by Everest on the block-access-list stack. It records the pinned upstream gas schedule for test networks, not a Base parameter proposal or a claim of full Ethereum protocol equivalence.

## Activation

The BAL base already maps **Denim**, Everest, and Zenith to `SpecId::AMSTERDAM` (`crates/common/chains/src/upgrade.rs:60`). Everest does not newly enable Amsterdam. Preserve that fork history: before Denim the EVM remains Osaka; immediately before and after Everest it remains Amsterdam when Denim has already activated.

For a custom geth genesis passed to the execution node with `--chain /path/to/genesis.json`, set the following fields inside the existing `config` object (merge into a complete genesis, not a standalone genesis):

```json
{
  "blockTime": 2,
  "base": {
    "denim": 1900000000,
    "everest": 1900000000
  }
}
```

These example activation timestamps are 2030-03-17 17:46:40 UTC / 12:46:40 EST. Configure earlier prerequisites and the consensus/rollup schedule consistently. `config.blockTime` must be positive for Denim's timestamp schedule. `config.base.denim` also schedules the Ethereum Amsterdam alias; setting only Everest is insufficient for the BAL/header gates. No production schedule is changed by this baseline. See `BaseChainSpec::from(Genesis)` in `crates/execution/chainspec/src/spec.rs:190` and the existing `parse_base_upgrades` test.

## Pinned implementation inventory

The stack pins revm **43.0.3** and Base Reth **base-v2.7.0.1**, commit `de52d8204b08e52b4dc719d7b4e59b6c7ef93b2c`. Paths below are relative to the named upstream Cargo package. These are implementation snapshots, not a promise that future revisions of the Amsterdam meta-EIP have the same contents.

| EIP | Upstream implementation and gate | Base status |
| --- | --- | --- |
| 7928, block access lists | Reth support integrated by the BAL stack | Already wired at Denim; payload budget, commitment validation and synchronous payload envelope retained |
| 8037, state gas | `revm-context-43.0.3/src/cfg.rs:233`; `revm-context-interface-43.0.2/src/cfg/gas_params.rs:315`; `revm-primitives-43.0.0/src/eip8037.rs:23` | Mainnet gas table and `enable_amsterdam_eip8037=true`; CPSB **1530**, no dynamic Base formula override; account 120 bytes = 183600 state gas, slot 64 bytes = 97920, code byte = 1530 |
| 8038, state access | `revm-context-interface-43.0.2/src/cfg/gas_params.rs:351`; `revm-primitives-43.0.0/src/eip8038.rs` | Already selected by Amsterdam: account read 3000/write 9000, storage read 2100/write 10000, warm access 100; CALL value 11300 |
| 2780, intrinsic gas | `revm-context-43.0.3/src/cfg.rs:233`; gas table `gas_params.rs:334` | `enable_amsterdam_eip2780=true`; upstream base 12000. Base has its own caller fee/nonce handler; this inventory does not establish full runtime-charge parity |
| 7976, calldata floor | `revm-context-interface-43.0.2/src/cfg/gas_params.rs:337` | Already selected: **64 gas/byte** for both zero and nonzero bytes, not the separate Base 128 proposal |
| 7981, access-list floor | Same gas table, `gas_params.rs:345` | Already selected; access-list data floor at upstream 64 gas/byte |
| 7708, ETH transfer logs | `revm-context-43.0.3/src/journal/inner.rs`; `cfg.rs:151` | Amsterdam journal support, disable flag false; Base deposit/fee paths remain custom, not asserted equivalent to all L1 transfer paths |
| 8246, delayed SELFDESTRUCT clearing | `revm-context-43.0.3/src/cfg.rs:155` | Disable flag false; upstream default retained |
| 7825, transaction cap | `revm-context-43.0.3/src/cfg.rs:451` | Already inherited from Azul; Base's explicit `MAX_TX_GAS_LIMIT_OSAKA` override preserved; deposits retain their validation exemption |
| 7954, contract size | `revm-context-43.0.3/src/cfg.rs:464,479` | Amsterdam defaults: code 65536 bytes, initcode 131072 bytes |
| 8024, extended stack operations | `revm-interpreter-43.0.2/src/instructions/stack.rs:63,80,97` | Amsterdam-gated DUPN/SWAPN/EXCHANGE; no additional enable flag |
| 7843, slot number | `revm-interpreter-43.0.2/src/instructions/block_info.rs:83` | Header slot already wired to L2 block number by BAL base; **gap:** Base EVM environment currently sets `slot_num=0`, so opcode/header parity is not established |
| 7904, compute cost analysis | Informational in [EIP-7773](https://eips.ethereum.org/EIPS/eip-7773); no independent activation switch | Not a separate consensus activation |
| 7778, block gas without refunds | `revm-context-interface-43.0.2/src/result.rs:271` exposes pre-refund `block_regular_gas_used()` | **Integration gap:** Base payload accounting still accumulates `tx_gas_used()`; full L1 block-accounting parity is not established |
| 7997, deterministic factory | Chain-state requirement at canonical address `0x4e59b44847b379578588920cA78FbF26c0B4956C`, already referenced by `Eip8130Contracts::CREATE2_FACTORY` (`crates/common/consensus/src/transaction/eip8130/addresses.rs:36-41`); not an execution-client implementation gap | A custom genesis may need the factory in its `alloc` if tests exercise it; this PR does not provision it |
| 7688, 7732, 8045, 8061, 8282 | Ethereum consensus-layer changes in the Amsterdam meta-EIP | Outside Base EL spec selection; no L1 consensus behavior imported |

No additional Amsterdam enable switch was found disabled in Base's production `BaseEvmEnvBuilder::cfg_env`: it calls `with_spec_and_mainnet_gas_params`, enabling 8037/2780 and retaining upstream defaults for 7708/8246. Optional simulation bypass features do not themselves disable consensus checks. This baseline does not implement unverified or missing meta-EIP items.

## Builder caveats and validation scope

`--builder.max_gas_per_txn` compares `result.tx_gas_used()` in `crates/builder/core/src/flashblocks/context.rs:1333-1349`. It is a builder admission policy, not a new Amsterdam regular-gas-only cap. State-heavy transactions can therefore meet a different builder-policy bound from the EVM regular-gas cap. This baseline does not redesign that policy. Payload cumulative accounting also uses `tx_gas_used()`; BAL budgeting remains inherited from the stack.

The focused environment regression covers pre-Denim, Denim, pre-Everest and Everest, the upstream configuration flags, code-deposit state gas, the gas-table zero/nonzero calldata floor, CALL-value pricing, code/initcode limits, and the retained Azul cap. The calldata check omits recipient/value decomposition and is not a full runtime transaction-floor test. It does not prove full block-building, deposit, AA, slot-opcode or transfer-log conformance. No devnet, node or load test is part of this baseline.

Run the focused check with:

```sh
cargo test -p base-execution-evm everest_inherits_amsterdam_l1_parameters -- --test-threads=1
```
