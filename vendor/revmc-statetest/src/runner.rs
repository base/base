// Vendored from revm's `bins/revme/src/cmd/statetest/runner.rs`.
// Keep in sync with upstream; revmc-specific code lives in `compiled.rs`.

use std::{
    convert::Infallible,
    fmt::Debug,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use console::Term;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use revm_context::{Context, block::BlockEnv, cfg::CfgEnv, tx::TxEnv};
use revm_context_interface::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm_database::{self as database, bal::EvmDatabaseError};
use revm_database_interface::EmptyDB;
use revm_handler::{ExecuteCommitEvm, MainBuilder, MainContext};
use revm_primitives::{B256, Bytes, U256, hardfork::SpecId};
use revm_statetest_types_42_0_0::{SpecName, Test, TestSuite, TestUnit};
use serde_json::json;
use thiserror::Error;

use crate::merkle_trie::{TestValidationResult, compute_test_roots};

/// Error that occurs during test execution.
#[derive(Debug, Error)]
#[error("Path: {path}\nName: {name}\nError: {kind}")]
pub struct TestError {
    pub name: String,
    pub path: String,
    pub kind: TestErrorKind,
}

/// Specific kind of error that occurred during test execution.
#[derive(Debug, Error)]
pub enum TestErrorKind {
    #[error("logs root mismatch: got {got}, expected {expected}")]
    LogsRootMismatch { got: B256, expected: B256 },
    #[error("state root mismatch: got {got}, expected {expected}")]
    StateRootMismatch { got: B256, expected: B256 },
    #[error("unknown private key: {0:?}")]
    UnknownPrivateKey(B256),
    #[error("unexpected exception: got {got_exception:?}, expected {expected_exception:?}")]
    UnexpectedException { expected_exception: Option<String>, got_exception: Option<String> },
    #[error("unexpected output: got {got_output:?}, expected {expected_output:?}")]
    UnexpectedOutput { expected_output: Option<Bytes>, got_output: Option<Bytes> },
    #[error(transparent)]
    SerdeDeserialize(#[from] serde_json::Error),
    #[error("thread panicked")]
    Panic,
    #[error("path does not exist")]
    InvalidPath,
    #[error("no JSON test files found in path")]
    NoJsonFiles,
    #[error("compilation failed: {0}")]
    CompilationError(String),
}

/// Check if a test should be skipped based on its filename.
/// Some tests are known to be problematic or take too long.
pub fn skip_test(path: &Path) -> bool {
    let path_str = path.to_str().unwrap_or_default();

    // Skip tests that have storage for newly created account.
    if path_str.contains("paris/eip7610_create_collision") {
        return true;
    }

    let name = path.file_name().unwrap().to_str().unwrap_or_default();

    matches!(
        name,
        // Test check if gas price overflows, we handle this correctly but does not match tests
        // specific exception.
        | "CreateTransactionHighNonce.json"

        // Test with some storage check.
        | "RevertInCreateInInit_Paris.json"
        | "RevertInCreateInInit.json"
        | "dynamicAccountOverwriteEmpty.json"
        | "dynamicAccountOverwriteEmpty_Paris.json"
        | "RevertInCreateInInitCreate2Paris.json"
        | "create2collisionStorage.json"
        | "RevertInCreateInInitCreate2.json"
        | "create2collisionStorageParis.json"
        | "InitCollision.json"
        | "InitCollisionParis.json"
        | "test_init_collision_create_opcode.json"

        // Malformed value.
        | "ValueOverflow.json"
        | "ValueOverflowParis.json"

        // These tests are passing, but they take a lot of time to execute so we are going to skip them.
        | "Call50000_sha256.json"
        | "static_Call50000_sha256.json"
        | "loopMul.json"
        | "CALLBlake2f_MaxRounds.json"
    )
}

struct TestExecutionContext<'a> {
    name: &'a str,
    unit: &'a TestUnit,
    test: &'a Test,
    cfg: &'a CfgEnv,
    block: &'a BlockEnv,
    tx: &'a TxEnv,
    cache_state: &'a database::CacheState,
    elapsed: &'a Arc<Mutex<Duration>>,
    #[allow(dead_code)]
    trace: bool,
    print_json_outcome: bool,
}

fn build_json_output(
    test: &Test,
    test_name: &str,
    exec_result: &Result<
        ExecutionResult<HaltReason>,
        EVMError<EvmDatabaseError<Infallible>, InvalidTransaction>,
    >,
    validation: &TestValidationResult,
    spec: SpecId,
    error: Option<String>,
) -> serde_json::Value {
    json!({
        "stateRoot": validation.state_root,
        "logsRoot": validation.logs_root,
        "output": exec_result.as_ref().ok().and_then(|r| r.output().cloned()).unwrap_or_default(),
        "gasUsed": exec_result.as_ref().ok().map(|r| r.tx_gas_used()).unwrap_or_default(),
        "pass": error.is_none(),
        "errorMsg": error.unwrap_or_default(),
        "evmResult": format_evm_result(exec_result),
        "postLogsHash": validation.logs_root,
        "fork": spec,
        "test": test_name,
        "d": test.indexes.data,
        "g": test.indexes.gas,
        "v": test.indexes.value,
    })
}

fn format_evm_result(
    exec_result: &Result<
        ExecutionResult<HaltReason>,
        EVMError<EvmDatabaseError<Infallible>, InvalidTransaction>,
    >,
) -> String {
    match exec_result {
        Ok(r) => match r {
            ExecutionResult::Success { reason, .. } => format!("Success: {reason:?}"),
            ExecutionResult::Revert { .. } => "Revert".to_string(),
            ExecutionResult::Halt { reason, .. } => format!("Halt: {reason:?}"),
        },
        Err(e) => e.to_string(),
    }
}

fn validate_exception(
    test: &Test,
    exec_result: &Result<
        ExecutionResult<HaltReason>,
        EVMError<EvmDatabaseError<Infallible>, InvalidTransaction>,
    >,
) -> Result<bool, TestErrorKind> {
    match (&test.expect_exception, exec_result) {
        (None, Ok(_)) => Ok(false),
        (Some(_), Err(_)) => Ok(true),
        _ => Err(TestErrorKind::UnexpectedException {
            expected_exception: test.expect_exception.clone(),
            got_exception: exec_result.as_ref().err().map(|e| e.to_string()),
        }),
    }
}

fn validate_output(
    expected_output: Option<&Bytes>,
    actual_result: &ExecutionResult<HaltReason>,
) -> Result<(), TestErrorKind> {
    if let Some((expected, actual)) = expected_output.zip(actual_result.output()) {
        if expected != actual {
            return Err(TestErrorKind::UnexpectedOutput {
                expected_output: Some(expected.clone()),
                got_output: actual_result.output().cloned(),
            });
        }
    }
    Ok(())
}

pub(crate) fn check_evm_execution(
    test: &Test,
    expected_output: Option<&Bytes>,
    test_name: &str,
    exec_result: &Result<
        ExecutionResult<HaltReason>,
        EVMError<EvmDatabaseError<Infallible>, InvalidTransaction>,
    >,
    db: &mut database::State<EmptyDB>,
    spec: SpecId,
    print_json_outcome: bool,
) -> Result<(), TestErrorKind> {
    let validation = compute_test_roots(exec_result, db);

    let print_json = |error: Option<&TestErrorKind>| {
        if print_json_outcome {
            let json = build_json_output(
                test,
                test_name,
                exec_result,
                &validation,
                spec,
                error.map(|e| e.to_string()),
            );
            eprintln!("{json}");
        }
    };

    // Check if exception handling is correct.
    let exception_expected = validate_exception(test, exec_result).inspect_err(|e| {
        print_json(Some(e));
    })?;

    // If exception was expected and occurred, we're done.
    if exception_expected {
        print_json(None);
        return Ok(());
    }

    // Validate output if execution succeeded.
    if let Ok(result) = exec_result {
        validate_output(expected_output, result).inspect_err(|e| {
            print_json(Some(e));
        })?;
    }

    // Validate logs root.
    if validation.logs_root != test.logs {
        let error =
            TestErrorKind::LogsRootMismatch { got: validation.logs_root, expected: test.logs };
        print_json(Some(&error));
        return Err(error);
    }

    // Validate state root.
    if validation.state_root != test.hash {
        let error =
            TestErrorKind::StateRootMismatch { got: validation.state_root, expected: test.hash };
        print_json(Some(&error));
        return Err(error);
    }

    print_json(None);
    Ok(())
}

/// Execute a single test suite file containing multiple tests.
pub fn execute_test_suite(
    path: &Path,
    elapsed: &Arc<Mutex<Duration>>,
    trace: bool,
    print_json_outcome: bool,
) -> Result<(), TestError> {
    if skip_test(path) {
        return Ok(());
    }

    let s = std::fs::read_to_string(path).unwrap();
    let path = path.to_string_lossy().into_owned();
    let suite: TestSuite = serde_json::from_str(&s).map_err(|e| TestError {
        name: "Unknown".to_string(),
        path: path.clone(),
        kind: e.into(),
    })?;

    for (name, unit) in suite.0 {
        // Prepare initial state.
        let cache_state = unit.state();

        // Setup base configuration.
        let mut cfg = CfgEnv::default();
        cfg.chain_id = unit.env.current_chain_id.unwrap_or(U256::ONE).try_into().unwrap_or(1);

        // Post and execution.
        for (spec_name, tests) in &unit.post {
            // Skip Constantinople spec.
            if *spec_name == SpecName::Constantinople {
                continue;
            }

            cfg.set_spec_and_mainnet_gas_params(spec_name.to_spec_id());

            // Configure max blobs per spec.
            if cfg.spec().is_enabled_in(SpecId::OSAKA) {
                cfg.set_max_blobs_per_tx(6);
            } else if cfg.spec().is_enabled_in(SpecId::PRAGUE) {
                cfg.set_max_blobs_per_tx(9);
            } else {
                cfg.set_max_blobs_per_tx(6);
            }

            // Setup block environment for this spec.
            let block = unit.block_env(&mut cfg);

            for test in tests.iter() {
                // Setup transaction environment.
                let tx = match test.tx_env(&unit) {
                    Ok(tx) => tx,
                    Err(_) if test.expect_exception.is_some() => continue,
                    Err(_) => {
                        return Err(TestError {
                            name,
                            path,
                            kind: TestErrorKind::UnknownPrivateKey(
                                unit.transaction.secret_key.unwrap_or_default(),
                            ),
                        });
                    }
                };

                // Execute the test.
                let result = execute_single_test(TestExecutionContext {
                    name: &name,
                    unit: &unit,
                    test,
                    cfg: &cfg,
                    block: &block,
                    tx: &tx,
                    cache_state: &cache_state,
                    elapsed,
                    trace,
                    print_json_outcome,
                });

                if let Err(e) = result {
                    // Handle error with debug trace if needed.
                    static FAILED: AtomicBool = AtomicBool::new(false);
                    if print_json_outcome || FAILED.swap(true, Ordering::SeqCst) {
                        return Err(TestError { name, path, kind: e });
                    }

                    return Err(TestError { path, name, kind: e });
                }
            }
        }
    }
    Ok(())
}

fn execute_single_test(ctx: TestExecutionContext) -> Result<(), TestErrorKind> {
    // Prepare state.
    let cache = ctx.cache_state.clone();
    let mut state =
        database::State::builder().with_cached_prestate(cache).with_bundle_update().build();

    let evm_context = Context::mainnet()
        .with_block(ctx.block)
        .with_tx(ctx.tx)
        .with_cfg(ctx.cfg.clone())
        .with_db(&mut state);

    // Execute.
    let timer = Instant::now();
    let mut evm = evm_context.build_mainnet();
    let exec_result = evm.transact_commit(ctx.tx);
    let db = evm.ctx.journaled_state.database;
    *ctx.elapsed.lock().unwrap() += timer.elapsed();

    // Check results.
    check_evm_execution(
        ctx.test,
        ctx.unit.out.as_ref(),
        ctx.name,
        &exec_result,
        db,
        *ctx.cfg.spec(),
        ctx.print_json_outcome,
    )
}

#[derive(Clone)]
pub(crate) struct TestRunnerState {
    pub(crate) n_errors: Arc<AtomicUsize>,
    pub(crate) console_bar: Arc<ProgressBar>,
    pub(crate) queue: Arc<Mutex<(usize, Vec<PathBuf>)>>,
    pub(crate) elapsed: Arc<Mutex<Duration>>,
    /// Set when any worker thread requests all others to stop (e.g. on panic).
    pub(crate) stop: Arc<AtomicBool>,
}

fn console_bar(n_files: usize) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(
        Some(n_files as u64),
        ProgressDrawTarget::term_like_with_hz(Box::new(Term::buffered_stderr()), 1),
    );
    bar.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {wide_bar} {pos}/{len} ({per_sec}, eta {eta})",
        )
        .unwrap()
        .progress_chars("=>-"),
    );
    bar.enable_steady_tick(Duration::from_secs(1));
    bar
}

impl TestRunnerState {
    pub(crate) fn new(test_files: Vec<PathBuf>) -> Self {
        let n_files = test_files.len();
        Self {
            n_errors: Arc::new(AtomicUsize::new(0)),
            console_bar: Arc::new(console_bar(n_files)),
            queue: Arc::new(Mutex::new((0usize, test_files))),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn next_test(&self) -> Option<PathBuf> {
        if self.stop.load(Ordering::Relaxed) {
            return None;
        }
        let (current_idx, queue) = &mut *self.queue.lock().unwrap();
        let idx = *current_idx;
        let test_path = queue.get(idx).cloned()?;
        *current_idx = idx + 1;
        Some(test_path)
    }
}
