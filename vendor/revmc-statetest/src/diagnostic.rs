// Diagnostic utilities for comparing interpreter vs JIT execution.

use std::{convert::Infallible, io::stderr};

use revm_context::{Context, block::BlockEnv, cfg::CfgEnv, tx::TxEnv};
use revm_context_interface::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm_database::{self as database, bal::EvmDatabaseError};
use revm_database_interface::{DatabaseCommit, EmptyDB};
use revm_handler::{ExecuteCommitEvm, Handler, MainBuilder, MainContext};
use revm_inspector::{InspectCommitEvm, inspectors::TracerEip3155};
use revm_primitives::{B256, Bytes};
use revmc::{revm_evm::JitEvm, runtime::JitBackend};

use crate::merkle_trie::compute_test_roots;

type ExecResult =
    Result<ExecutionResult<HaltReason>, EVMError<EvmDatabaseError<Infallible>, InvalidTransaction>>;

/// Snapshot of a single test execution for comparison.
#[derive(Debug)]
pub struct ExecutionSnapshot {
    pub status: ExecStatus,
    pub output: Option<Bytes>,
    pub gas_used: u64,
    pub state_root: B256,
    pub logs_root: B256,
    pub post_state_dump: String,
}

/// Summarized execution outcome.
#[derive(Debug)]
pub enum ExecStatus {
    Success(String),
    Revert,
    Halt(String),
    Error(String),
}

impl std::fmt::Display for ExecStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success(r) => write!(f, "Success({r})"),
            Self::Revert => write!(f, "Revert"),
            Self::Halt(r) => write!(f, "Halt({r})"),
            Self::Error(e) => write!(f, "Error({e})"),
        }
    }
}

fn snapshot_from_result(
    exec_result: &ExecResult,
    db: &database::State<EmptyDB>,
) -> ExecutionSnapshot {
    let validation = compute_test_roots(exec_result, db);

    let (status, output, gas_used) = match exec_result {
        Ok(result) => {
            let status = match result {
                ExecutionResult::Success { reason, .. } => {
                    ExecStatus::Success(format!("{reason:?}"))
                }
                ExecutionResult::Revert { .. } => ExecStatus::Revert,
                ExecutionResult::Halt { reason, .. } => ExecStatus::Halt(format!("{reason:?}")),
            };
            (status, result.output().cloned(), result.tx_gas_used())
        }
        Err(e) => (ExecStatus::Error(e.to_string()), None, 0),
    };

    ExecutionSnapshot {
        status,
        output,
        gas_used,
        state_root: validation.state_root,
        logs_root: validation.logs_root,
        post_state_dump: db.cache.pretty_print(),
    }
}

/// Run a single test case with the interpreter, returning an execution snapshot.
pub fn run_interpreter(
    cfg: &CfgEnv,
    block: &BlockEnv,
    tx: &TxEnv,
    cache_state: &database::CacheState,
) -> ExecutionSnapshot {
    let prestate = cache_state.clone();
    let mut state =
        database::State::builder().with_cached_prestate(prestate).with_bundle_update().build();

    let mut evm = Context::mainnet()
        .with_block(block)
        .with_tx(tx)
        .with_cfg(cfg.clone())
        .with_db(&mut state)
        .build_mainnet();
    let exec_result = evm.transact_commit(tx);
    let db = evm.ctx.journaled_state.database;

    snapshot_from_result(&exec_result, db)
}

/// Run a single test case with JIT-compiled functions, returning an execution snapshot.
pub fn run_jit(
    backend: &JitBackend,
    cfg: &CfgEnv,
    block: &BlockEnv,
    tx: &TxEnv,
    cache_state: &database::CacheState,
) -> ExecutionSnapshot {
    let prestate = cache_state.clone();
    let state =
        database::State::builder().with_cached_prestate(prestate).with_bundle_update().build();

    let evm_context = Context::mainnet()
        .with_block(block.clone())
        .with_tx(tx.clone())
        .with_cfg(cfg.clone())
        .with_db(state);
    let inner = evm_context.build_mainnet();
    let mut evm = JitEvm::new(inner, backend.clone());
    let mut handler = revm_handler::MainnetHandler::default();
    let exec_result = handler.run(&mut evm);
    if exec_result.is_ok() {
        let s = evm.ctx.journaled_state.finalize();
        DatabaseCommit::commit(&mut evm.ctx.journaled_state.database, s);
    }

    snapshot_from_result(&exec_result, &evm.ctx.journaled_state.database)
}

/// Re-run a test case with the interpreter and EIP-3155 tracing to stderr.
pub fn trace_interpreter(
    cfg: &CfgEnv,
    block: &BlockEnv,
    tx: &TxEnv,
    cache_state: &database::CacheState,
) {
    let prestate = cache_state.clone();
    let mut state =
        database::State::builder().with_cached_prestate(prestate).with_bundle_update().build();

    let mut evm = Context::mainnet()
        .with_db(&mut state)
        .with_block(block)
        .with_tx(tx)
        .with_cfg(cfg.clone())
        .build_mainnet_with_inspector(TracerEip3155::buffered(stderr()).without_summary());
    let exec_result = evm.inspect_tx_commit(tx);

    eprintln!("\nExecution result: {exec_result:#?}");
    eprintln!("\nState after:\n{}", evm.ctx.journaled_state.database.cache.pretty_print());
}

/// Compare two snapshots and return a list of mismatched fields.
pub fn compare(interp: &ExecutionSnapshot, jit: &ExecutionSnapshot) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();

    let interp_status = format!("{}", interp.status);
    let jit_status = format!("{}", jit.status);
    if interp_status != jit_status {
        mismatches.push(Mismatch { field: "status", interpreter: interp_status, jit: jit_status });
    }

    if interp.output != jit.output {
        mismatches.push(Mismatch {
            field: "output",
            interpreter: format!("{:?}", interp.output),
            jit: format!("{:?}", jit.output),
        });
    }

    if interp.gas_used != jit.gas_used {
        mismatches.push(Mismatch {
            field: "gas_used",
            interpreter: interp.gas_used.to_string(),
            jit: jit.gas_used.to_string(),
        });
    }

    if interp.state_root != jit.state_root {
        mismatches.push(Mismatch {
            field: "state_root",
            interpreter: format!("{}", interp.state_root),
            jit: format!("{}", jit.state_root),
        });
    }

    if interp.logs_root != jit.logs_root {
        mismatches.push(Mismatch {
            field: "logs_root",
            interpreter: format!("{}", interp.logs_root),
            jit: format!("{}", jit.logs_root),
        });
    }

    mismatches
}

/// A single field mismatch between interpreter and JIT execution.
pub struct Mismatch {
    pub field: &'static str,
    pub interpreter: String,
    pub jit: String,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "  {}: interpreter={}, jit={}", self.field, self.interpreter, self.jit)
    }
}
