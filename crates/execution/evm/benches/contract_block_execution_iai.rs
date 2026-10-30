//! Deterministic instruction-count benchmark for executing a contract-heavy Base block.
//!
//! Counts the same production path as `block_execution_iai.rs`, the per-block execution work
//! every node does on block import and every builder does when it re-executes a block (about
//! 4×10^5 blocks per day after the Denim upgrade): build the EVM from [`BaseEvmConfig`], create
//! the production `BaseBlockExecutor`, apply pre-execution changes, execute each transaction,
//! and finish, over an in-memory database behind `reth_revm`'s `State`.
//!
//! The block (see `fixtures/mainnet_block.rs` and [`CONTRACT_MIX`]) replaces the plain
//! transfers of the mainnet-shaped block with contract calls: 400 ERC-20-style token transfers
//! from distinct callers to 256 holders, and 120 swaps spread over 4 constant-product pools, each
//! reading five storage slots and writing six. Every 10th caller holds no balance, so its call
//! reverts. The workload stresses account and storage loads, `SLOAD`/`SSTORE` gas accounting,
//! journaling and revert, logs, and committing touched storage, rather than value transfers.
//!
//! Building the block, the pre-state, and the transaction environments happens in iai's
//! unmeasured setup, and the inputs and the execution result are handed back so they drop
//! outside the counted region.
//!
//! The teardown pins the block's gas used, receipt and log counts, and success counts. They are
//! fixed by the workload, so a mismatch means execution now behaves differently, and the
//! instruction count is not comparable with the base commit's.
//!
//! [`BaseEvmConfig`]: base_execution_evm::BaseEvmConfig

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

#[path = "fixtures/mainnet_block.rs"]
mod fixture;

use std::hint::black_box;

use alloy_consensus::TxReceipt;
use base_common_consensus::BaseReceipt;
use fixture::{BlockMix, BlockTx, MAINNET_MIX, MainnetBlock};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_execution_types::BlockExecutionResult;
use reth_revm::State;
use revm::database::InMemoryDB;

/// The mainnet block's deposits, with its user transactions replaced by token transfers and
/// pool swaps.
const CONTRACT_MIX: BlockMix = BlockMix {
    transfers: 0,
    token_calls: 400,
    token_holders: 256,
    swaps: 120,
    pools: 4,
    withdrawals: 0,
    failed_call_every: 10,
    ..MAINNET_MIX
};

/// A block ready to execute: the fixture, a fresh pre-state, and the converted transactions.
type Input = (MainnetBlock, State<InMemoryDB>, Vec<BlockTx>);

/// The execution result, plus the inputs that must drop outside the counted region.
type Output = (BlockExecutionResult<BaseReceipt>, (MainnetBlock, State<InMemoryDB>));

/// Outcome counts the workload fixes.
#[derive(Debug, PartialEq, Eq)]
struct Outcomes {
    gas_used: u64,
    receipts: usize,
    logs: usize,
    successful_deposits: usize,
    successful_calls: usize,
    failed_calls: usize,
}

/// Builds the fixture, the pre-state, and the transaction environments outside the counted
/// region.
fn setup() -> Input {
    let block = MainnetBlock::new(CONTRACT_MIX);
    let state = block.fresh_state();
    let txs = block.block_txs();
    (block, state, txs)
}

/// Checks the block's outcomes against the workload's documented counts.
fn check_outcomes((result, _): Output) {
    let count = |deposit: bool, success: bool| {
        result
            .receipts
            .iter()
            .filter(|receipt| {
                matches!(receipt, BaseReceipt::Deposit(_)) == deposit && receipt.status() == success
            })
            .count()
    };
    let outcomes = Outcomes {
        gas_used: result.gas_used,
        receipts: result.receipts.len(),
        logs: result.receipts.iter().map(|receipt| receipt.logs().len()).sum(),
        successful_deposits: count(true, true),
        successful_calls: count(false, true),
        failed_calls: count(false, false),
    };
    // 1 L1 attributes deposit and 4 user deposits; 360 token transfers and 108 swaps succeed and
    // log once each; the 40 token calls and 12 swaps from callers without a balance revert.
    let expected = Outcomes {
        gas_used: 20_747_946,
        receipts: 525,
        logs: 468,
        successful_deposits: 5,
        successful_calls: 468,
        failed_calls: 52,
    };
    assert_eq!(outcomes, expected, "block_execution/contract_block outcomes changed");
}

#[library_benchmark]
#[bench::contract_block(setup = setup, teardown = check_outcomes)]
fn execute_contract_block((block, mut state, txs): Input) -> Output {
    let result = block.execute(&mut state, txs);
    (black_box(result), (block, state))
}

library_benchmark_group!(
    name = contract_block_execution;
    benchmarks = execute_contract_block
);

main!(library_benchmark_groups = contract_block_execution);
