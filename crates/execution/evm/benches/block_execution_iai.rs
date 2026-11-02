//! Deterministic instruction-count benchmark for executing a Base mainnet-shaped block.
//!
//! Counts the per-block execution work every node does on block import and every builder does
//! when it validates or re-executes a block: about 4×10^5 blocks per day after the Denim
//! upgrade. The counted path is the one the engine's payload validator runs: build the EVM from
//! [`BaseEvmConfig`], create the production `BaseBlockExecutor`, apply pre-execution changes,
//! execute each transaction, and finish. Transactions arrive with their EVM environments
//! already built, as the payload processor's conversion workers hand them over. The state is an
//! in-memory database behind `reth_revm`'s `State`, configured as block import configures it,
//! so database reads cost a hash-map lookup rather than a provider read.
//!
//! The block is described in `fixtures/mainnet_block.rs` (`MAINNET_MIX`): 387 transactions,
//! mostly plain transfers carrying calldata, plus token transfers and withdrawals. Building it, the pre-state, and the
//! transaction environments happens in iai's unmeasured setup, and the inputs and the
//! execution result are handed back so they drop outside the counted region.
//!
//! The teardown pins the block's gas used, DA footprint, receipt count, and success counts.
//! They are fixed by the workload, so a mismatch means execution now behaves differently, and
//! the instruction count is not comparable with the base commit's.
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
use fixture::{BlockTx, MAINNET_MIX, MainnetBlock};
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use reth_execution_types::BlockExecutionResult;
use reth_revm::State;
use revm::database::InMemoryDB;

/// A block ready to execute: the fixture, a fresh pre-state, and the converted transactions.
type Input = (MainnetBlock, State<InMemoryDB>, Vec<BlockTx>);

/// The execution result, plus the inputs that must drop outside the counted region.
type Output = (BlockExecutionResult<BaseReceipt>, (MainnetBlock, State<InMemoryDB>));

/// Outcome counts the workload fixes.
#[derive(Debug, PartialEq, Eq)]
struct Outcomes {
    gas_used: u64,
    da_footprint: u64,
    receipts: usize,
    successful_deposits: usize,
    successful_user_txs: usize,
    failed_user_txs: usize,
}

/// Builds the fixture, the pre-state, and the transaction environments outside the counted
/// region.
fn setup() -> Input {
    let block = MainnetBlock::new(MAINNET_MIX);
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
        da_footprint: result.blob_gas_used,
        receipts: result.receipts.len(),
        successful_deposits: count(true, true),
        successful_user_txs: count(false, true),
        failed_user_txs: count(false, false),
    };
    // 1 L1 attributes deposit and 4 user deposits; 300 transfers, 60 token transfers, and 2
    // withdrawals succeed; the 20 token calls from callers without tokens revert.
    let expected = Outcomes {
        gas_used: 13_228_002,
        da_footprint: 45_874_000,
        receipts: 387,
        successful_deposits: 5,
        successful_user_txs: 362,
        failed_user_txs: 20,
    };
    assert_eq!(outcomes, expected, "block_execution/mainnet_block outcomes changed");
}

#[library_benchmark]
#[bench::mainnet_block(setup = setup, teardown = check_outcomes)]
fn execute_block((block, mut state, txs): Input) -> Output {
    let result = block.execute(&mut state, txs);
    (black_box(result), (block, state))
}

library_benchmark_group!(
    name = block_execution;
    benchmarks = execute_block
);

main!(library_benchmark_groups = block_execution);
