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

The stack pins revm **43.0.3**, alloy-evm **0.39.0**, and Base Reth **base-v2.7.0.1**, commit `de52d8204b08e52b4dc719d7b4e59b6c7ef93b2c`. Paths below are relative to the named upstream Cargo package. These are implementation snapshots, not a promise that future revisions of the Amsterdam meta-EIP have the same contents.

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
| 7843, slot number | `revm-interpreter-43.0.2/src/instructions/block_info.rs:83` | Header and EVM slot use the L2 block number from Amsterdam onward; focused opcode execution tests cover imported headers, payload building and Engine payload execution, with the opcode still unavailable before Amsterdam |
| 7904, compute cost analysis | Informational in [EIP-7773](https://eips.ethereum.org/EIPS/eip-7773); no independent activation switch | Not a separate consensus activation |
| 7778, block gas without refunds | `revm-context-interface-43.0.2/src/result.rs:271` exposes pre-refund `block_regular_gas_used()` | Wired with 8037 in the shared executor, basic builder and post-execution validation; header gas uses the maximum of independent regular/state totals, not receipt totals |
| 7997, deterministic factory | Chain-state requirement at canonical address `0x4e59b44847b379578588920cA78FbF26c0B4956C`, already referenced by `Eip8130Contracts::CREATE2_FACTORY` (`crates/common/consensus/src/transaction/eip8130/addresses.rs:36-41`); not an execution-client implementation gap | A custom genesis may need the factory in its `alloc` if tests exercise it; this PR does not provision it |
| 7688, 7732, 8045, 8061, 8282 | Ethereum consensus-layer changes in the Amsterdam meta-EIP | Outside Base EL spec selection; no L1 consensus behavior imported |

No additional Amsterdam enable switch was found disabled in Base's production `BaseEvmEnvBuilder::cfg_env`: it calls `with_spec_and_mainnet_gas_params`, enabling 8037/2780 and retaining upstream defaults for 7708/8246. Optional simulation bypass features do not themselves disable consensus checks. This baseline does not implement unverified or missing meta-EIP items.

## Amsterdam block gas accounting

At Denim and later, the shared executor and basic builder mirror the pinned Ethereum implementation:

- Per transaction, charged gas is `max(total spent - ordinary refund, calldata floor)`. Receipts accumulate this scalar value, including deposits. The header no longer has to equal the last receipt's `cumulativeGasUsed`.
- Block regular gas is `E = max(total spent - final state spend, calldata floor)`, **before ordinary gas refunds** (EIP-7778). Block state gas is `S = final state spend`, after state-reservoir refills. Refill applies only to state created and removed within the same transaction; SELFDESTRUCT does not create such a refill.
- The block header uses `gasUsed = max(sum(E), sum(S))`, not `sum(max(E, S))` or summed receipt gas. Both budgets are bounded by the existing block gas limit.
- Before inclusion, `min(tx.gas, 16777216)` must fit the remaining regular budget, and **the full `tx.gas`** must fit the remaining state budget. Native transactions retain the existing additional payer-auth reservation. Deposits instead reserve their **full declared gas in both budgets**: they bypass the ordinary transaction cap and can report their full limit as regular gas on failure. Base's pre-Regolith deposit admission exemption is unchanged. Deposits and L1-info transactions count the regular/state usage reported by Base execution, without changing minting or fee rules.
- Pre-Amsterdam admission, header gas and receipt semantics remain unchanged. Consensus still enforces `header.gasUsed <= header.gasLimit`; Amsterdam post-execution validation compares the header to execution's dual-budget result.

Sources: alloy-evm `src/eth/block.rs:203-235,254-280,338-350`; Reth `crates/ethereum/payload/src/lib.rs:265-281,377-380,436-441`; revm-context-interface `src/result.rs:263-292`; [EIP-8037](https://eips.ethereum.org/EIPS/eip-8037), [EIP-7778](https://eips.ethereum.org/EIPS/eip-7778), [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825). The pinned Amsterdam implementation applies the EIP-7825 cap to regular gas, rather than rejecting a state-heavy transaction solely because its declared gas exceeds that cap.

**Existing Base fee policy is retained:** Jovian calculates the next base fee using `max(parent.gasUsed, parent DA footprint)`, now receiving the corrected Amsterdam header value. Existing chain/extra-data elasticity, denominator, minimum base fee and block gas-limit controls are unchanged (`crates/execution/chainspec/src/basefee.rs:63-83`). This imports L1 Amsterdam block accounting, not Ethereum's separate fee-policy parameters. No Base-tuned CPSB, state-gas price or new capacity ratio is introduced.

## Builder caveats and validation scope

`--builder.max_gas_per_txn` is unchanged. It is a builder policy, not the upstream EIP-7825 regular-gas cap of **16,777,216**. The basic builder uses Amsterdam's independent block budgets and retains BAL budgeting. Native EIP-8430/type `0x79` pricing is intentionally unchanged for now; its reported regular/state gas is included in those block counters, including the existing conservative payer-auth reservation.

The focused environment regression covers pre-Denim, Denim, pre-Everest and Everest, the upstream configuration flags, code-deposit state gas, the gas-table zero/nonzero calldata floor, CALL-value pricing, code/initcode limits, and the retained Azul cap. The calldata check omits recipient/value decomposition and is not a full runtime transaction-floor test. Additional executor, builder and consensus tests cover Amsterdam block accounting, ordinary storage refunds, state-heavy/mixed blocks, admission boundaries and native transaction accounting. These checks do not establish full deposit, slot-opcode or transfer-log conformance.

Run the focused check with:

```sh
cargo test -p base-execution-evm everest_inherits_amsterdam_l1_parameters -- --test-threads=1
cargo test -p base-common-evm amsterdam_block_gas -- --test-threads=1
cargo test -p base-common-evm eip8130_phase_statuses_survive_execution_on_another_thread -- --test-threads=1
cargo test -p base-execution-payload-builder amsterdam_block_gas -- --test-threads=1
cargo test -p base-execution-consensus amsterdam_block_gas -- --test-threads=1
```

## Local verification

A light local devnet rerun used 120,000,000 block gas, Denim/Everest active at genesis and 200 ms runtime cadence. Existing/new-recipient ETH transfers, ERC-20/WETH transfers, 24 KiB and 64 KiB deployments, four fresh SSTOREs and a real L1 deposit succeeded. All eight sampled user-transaction blocks and the deposit block matched `max(sum(E), sum(S))` in debug traces and were canonically imported by the follower.

For the 64 KiB block, regular gas summed to **4,299,587** and state gas to **100,453,680**. Before this integration its header reported summed receipt gas, **100,857,269**. Afterward its header correctly reports **100,453,680**, while the receipt sum remains **100,857,269** and the deployment's receipt gas remains **100,776,882**. The follower's raw BAL matched the builder's bytes and both header commitments.

The 115M declared-gas deployment was admitted at 120M and rejected after the local L1 SystemConfig lowered the block limit to 40M; the limit was restored to 120M. The existing `--builder.max_gas_per_txn=40000000` flag did not prevent its inclusion by the basic builder; that flag was not changed. Ten consecutive timestamp intervals were 200 ms. This is a smoke test, not a throughput benchmark.

Native `0x79` execution/commit accounting is covered by a unit test; no native transaction was submitted on this devnet. Safe/finalized derivation was not qualified. The local logs retained temporary follower L1-origin lookup resets and L1 beacon/validator timeouts; the tested unsafe canonical imports and BAL checks succeeded. No load test was run, and all task-created devnet containers/network were removed.
