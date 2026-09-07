//! Background JIT and AOT compilation workers.
//!
//! Each worker thread owns a long-lived `EvmCompiler` instance tied to its
//! thread-local LLVM context.
//!
//! With ORCv2, JIT code lifetime is managed per-module via `ResourceTracker`s.
//! Each successful JIT compilation extracts the committed module's tracker and
//! returns it as a [`JitCodeBacking`], which frees the machine code on drop.
//! Workers no longer need to stay alive for code lifetime — they exit as soon
//! as the job channel closes.

#[cfg(feature = "llvm")]
use std::{cell::RefCell, fs::File, io::Read, time::Instant};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_primitives::Bytes;
use crossbeam_channel as chan;
use rayon::{ThreadPool, ThreadPoolBuilder};

use crate::{
    CompileTimings, EvmCompilerFn, OptimizationLevel,
    runtime::{
        config::{CompilationKind, RuntimeConfig},
        stats::RuntimeStats,
        storage::RuntimeCacheKey,
    },
};
#[cfg(feature = "llvm")]
use crate::{
    EvmCompiler, EvmLlvmBackend, Linker,
    llvm::{JitDylibGuard, orc::ResourceTracker},
    runtime::config::JitMode,
};

#[cfg(all(feature = "llvm", unix))]
type OutOfProcessHelper = Option<Arc<super::out_of_process::HelperProcess>>;
#[cfg(not(all(feature = "llvm", unix)))]
type OutOfProcessHelper = ();

/// Notifier for synchronous compilation requests.
///
/// Wraps an optional sender that is notified when the compilation
/// completes (success or failure). Passed from the backend through
/// the worker and back, then fired after the result is processed.
pub(crate) struct SyncNotifier(Option<chan::Sender<()>>);

impl SyncNotifier {
    pub(crate) fn none() -> Self {
        Self(None)
    }

    pub(crate) fn new(tx: chan::Sender<()>) -> Self {
        Self(Some(tx))
    }

    pub(crate) fn notify(self) {
        if let Some(tx) = self.0 {
            let _ = tx.send(());
        }
    }
}

/// A compilation job sent from the backend to a worker.
#[derive(derive_more::Debug)]
pub(crate) struct CompileJob {
    /// Whether this job compiles JIT or AOT output.
    pub(crate) kind: CompilationKind,
    /// The key to compile for.
    pub(crate) key: RuntimeCacheKey,
    /// The raw bytecode to compile.
    pub(crate) bytecode: Bytes,
    /// The symbol name to use for the compiled function.
    pub(crate) symbol_name: String,
    /// Optimization level for compilation.
    pub(crate) opt_level: OptimizationLevel,
    /// Optional notifier for synchronous callers.
    #[debug(skip)]
    pub(crate) sync_notifier: SyncNotifier,
    /// Generation at the time the job was dispatched.
    pub(crate) generation: u64,
}

/// Result of a compilation attempt, sent back from a worker to the backend.
pub(crate) struct WorkerResult {
    /// The key that was compiled.
    pub(crate) key: RuntimeCacheKey,
    /// The compilation outcome.
    pub(crate) outcome: Result<WorkerSuccess, String>,
    /// Whether this was a JIT or AOT compilation job.
    pub(crate) kind: CompilationKind,
    /// Optional notifier for synchronous callers, passed through from the job.
    pub(crate) sync_notifier: SyncNotifier,
    /// Generation at the time the job was dispatched.
    pub(crate) generation: u64,
    /// Wall-clock time spent compiling.
    pub(crate) compile_duration: Duration,
    /// Per-phase timing breakdown from the compiler.
    pub(crate) timings: CompileTimings,
}

/// Successful compilation output.
pub(crate) enum WorkerSuccess {
    /// JIT compilation produced an in-memory function pointer.
    Jit(JitSuccess),
    /// AOT compilation produced shared-library bytes.
    Aot(AotSuccess),
    /// JIT compilation produced relocatable object bytes to link in the parent.
    JitObject(JitObjectSuccess),
}

/// Successful JIT compilation output.
pub(crate) struct JitSuccess {
    /// The compiled function pointer.
    pub(crate) func: EvmCompilerFn,
    /// Owns the JIT machine code via an ORCv2 `ResourceTracker`.
    /// Dropping this frees the compiled code.
    pub(crate) backing: Arc<JitCodeBacking>,
}

/// Successful AOT compilation output.
pub(crate) struct JitObjectSuccess {
    /// The symbol name in the object file.
    pub(crate) symbol_name: String,
    /// The raw relocatable object bytes.
    pub(crate) object_bytes: Bytes,
    /// Builtin absolute symbols referenced by the object.
    pub(crate) builtin_symbols: Vec<String>,
}

pub(crate) struct AotSuccess {
    /// The symbol name in the shared library.
    pub(crate) symbol_name: String,
    /// The raw shared-library bytes (.so / .dylib).
    pub(crate) dylib_bytes: Vec<u8>,
    /// Length of the original bytecode.
    pub(crate) bytecode_len: usize,
}

/// Owns JIT-compiled machine code via an ORCv2 `ResourceTracker` and a
/// [`JitDylibGuard`].
///
/// The tracker provides per-entry code removal: dropping it calls
/// `tracker.remove()` which frees this entry's machine code.
/// The guard keeps the owning `JITDylib` alive — it won't be cleared
/// or recycled until all guards are dropped.
///
/// This enables true per-entry eviction: removing a `CompiledProgram`
/// from the resident map reclaims its machine code once all callers
/// release their `Arc<CompiledProgram>` handles.
pub(crate) struct JitCodeBacking {
    /// The tracker that owns this entry's machine code.
    /// Must be dropped (after `remove()`) BEFORE `_jd_guard` to ensure
    /// the JITDylib is still valid when we remove resources from it.
    #[cfg(feature = "llvm")]
    tracker: Option<ResourceTracker>,
    /// Keeps the owning JITDylib alive. Dropped after `tracker`.
    #[cfg(feature = "llvm")]
    _jd_guard: Arc<JitDylibGuard>,
}

impl JitCodeBacking {
    #[cfg(feature = "llvm")]
    pub(crate) fn new(tracker: ResourceTracker, jd_guard: Arc<JitDylibGuard>) -> Self {
        Self { tracker: Some(tracker), _jd_guard: jd_guard }
    }
}

#[cfg(feature = "llvm")]
impl Drop for JitCodeBacking {
    fn drop(&mut self) {
        if let Some(tracker) = self.tracker.take()
            && let Err(e) = tracker.remove()
        {
            warn!("failed to remove JIT code: {e}");
        }
    }
}

/// Handle to the worker pool.
pub(crate) struct WorkerPool {
    /// Rayon pool used to execute compilation jobs.
    pool: Option<ThreadPool>,
    /// Out-of-process helper used by this worker pool.
    out_of_process_helper: OutOfProcessHelper,
    /// Sender for worker results.
    result_tx: chan::Sender<WorkerResult>,
    /// Runtime configuration shared by spawned jobs.
    config: Arc<RuntimeConfig>,
    /// Number of queued jobs waiting to start.
    queued: Arc<AtomicUsize>,
    /// Maximum queued jobs accepted by the pool.
    queue_capacity: usize,
    /// Signals workers to stop accepting new work.
    shutdown: Arc<AtomicBool>,
}

impl WorkerPool {
    /// Creates and starts the worker pool.
    pub(crate) fn new(
        result_tx: chan::Sender<WorkerResult>,
        config: RuntimeConfig,
        stats: Arc<RuntimeStats>,
    ) -> Self {
        let worker_count = config.tuning.jit_worker_count;
        let queue_capacity = worker_count.saturating_mul(config.tuning.jit_worker_queue_capacity);
        let out_of_process_helper = create_out_of_process_helper(&config, stats);
        let pool = (worker_count > 0).then(|| {
            ThreadPoolBuilder::new()
                .num_threads(worker_count)
                .thread_name(|i| format!("revmc-{i:02}"))
                .exit_handler(|_| clear_thread_local_compilers())
                .build()
                .expect("failed to spawn compile workers")
        });

        Self {
            pool,
            out_of_process_helper,
            result_tx,
            config: Arc::new(config),
            queued: Arc::new(AtomicUsize::new(0)),
            queue_capacity,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Tries to send a job to the worker pool.
    /// Returns the job back on failure (queue full or shut down).
    pub(crate) fn try_send(&mut self, job: CompileJob) -> Result<(), CompileJob> {
        if self.pool.is_none() || self.shutdown.load(Ordering::Acquire) {
            return Err(job);
        }

        let mut current = self.queued.load(Ordering::Acquire);
        loop {
            if current >= self.queue_capacity {
                return Err(job);
            }
            match self.queued.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }

        let pool = self.pool.as_ref().unwrap();
        let queued = self.queued.clone();
        let result_tx = self.result_tx.clone();
        let config = self.config.clone();
        let out_of_process_helper = self.out_of_process_helper.clone();
        pool.spawn_fifo(move || {
            queued.fetch_sub(1, Ordering::AcqRel);
            let result = compile_job(job, &config, out_of_process_helper);
            let _ = result_tx.send(result);
        });
        Ok(())
    }

    /// Shuts down all workers after draining queued jobs.
    pub(crate) fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.cancel_in_flight();
        if let Some(pool) = &self.pool {
            pool.broadcast(|_| clear_thread_local_compilers());
        }
        self.pool.take();
    }

    /// Cancels any in-flight compilation that can be interrupted externally.
    pub(crate) fn cancel_in_flight(&self) {
        cancel_out_of_process_helper(&self.out_of_process_helper);
    }

    /// Pauses out-of-process helper execution.
    pub(crate) fn pause(&self) {
        pause_out_of_process_helper(&self.out_of_process_helper);
    }

    /// Resumes out-of-process helper execution.
    pub(crate) fn resume(&self) {
        resume_out_of_process_helper(&self.out_of_process_helper);
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "llvm", unix))]
fn create_out_of_process_helper(
    config: &RuntimeConfig,
    stats: Arc<RuntimeStats>,
) -> OutOfProcessHelper {
    (config.jit_mode == JitMode::OutOfProcess)
        .then(|| Arc::new(super::out_of_process::HelperProcess::new(stats)))
}

#[cfg(not(all(feature = "llvm", unix)))]
fn create_out_of_process_helper(
    _config: &RuntimeConfig,
    _stats: Arc<RuntimeStats>,
) -> OutOfProcessHelper {
}

#[cfg(all(feature = "llvm", unix))]
fn cancel_out_of_process_helper(helper: &OutOfProcessHelper) {
    if let Some(helper) = helper {
        helper.cancel_in_flight();
    }
}

#[cfg(not(all(feature = "llvm", unix)))]
fn cancel_out_of_process_helper(_helper: &OutOfProcessHelper) {}

#[cfg(all(feature = "llvm", unix))]
fn pause_out_of_process_helper(helper: &OutOfProcessHelper) {
    if let Some(helper) = helper {
        helper.pause();
    }
}

#[cfg(not(all(feature = "llvm", unix)))]
fn pause_out_of_process_helper(_helper: &OutOfProcessHelper) {}

#[cfg(all(feature = "llvm", unix))]
fn resume_out_of_process_helper(helper: &OutOfProcessHelper) {
    if let Some(helper) = helper {
        helper.resume();
    }
}

#[cfg(not(all(feature = "llvm", unix)))]
fn resume_out_of_process_helper(_helper: &OutOfProcessHelper) {}

#[cfg(feature = "llvm")]
fn clear_thread_local_compilers() {
    JIT_COMPILER.with_borrow_mut(Option::take);
    AOT_COMPILER.with_borrow_mut(Option::take);
}

#[cfg(not(feature = "llvm"))]
fn clear_thread_local_compilers() {}

#[cfg(feature = "llvm")]
fn compile_job(
    job: CompileJob,
    config: &RuntimeConfig,
    out_of_process_helper: OutOfProcessHelper,
) -> WorkerResult {
    trace!(?job, "received job");
    match job.kind {
        CompilationKind::Jit if config.jit_mode == JitMode::OutOfProcess => {
            compile_out_of_process_job(job, config, out_of_process_helper)
        }
        CompilationKind::Jit => JIT_COMPILER
            .with_borrow_mut(|state| compile_with_state(job, config, CompilerTarget::Jit, state)),
        CompilationKind::Aot => AOT_COMPILER
            .with_borrow_mut(|state| compile_with_state(job, config, CompilerTarget::Aot, state)),
    }
}

#[cfg(all(feature = "llvm", unix))]
fn compile_out_of_process_job(
    job: CompileJob,
    config: &RuntimeConfig,
    helper: OutOfProcessHelper,
) -> WorkerResult {
    let helper = helper.expect("missing out-of-process JIT helper");
    super::out_of_process::compile_job(job, config, &helper)
}

#[cfg(all(feature = "llvm", not(unix)))]
fn compile_out_of_process_job(
    job: CompileJob,
    _config: &RuntimeConfig,
    _helper: OutOfProcessHelper,
) -> WorkerResult {
    WorkerResult {
        key: job.key,
        outcome: Err("out-of-process JIT is only available on Unix".into()),
        kind: job.kind,
        sync_notifier: job.sync_notifier,
        generation: job.generation,
        compile_duration: Duration::ZERO,
        timings: CompileTimings::default(),
    }
}

#[cfg(feature = "llvm")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CompilerTarget {
    Jit,
    JitObject,
    Aot,
}

#[cfg(feature = "llvm")]
impl CompilerTarget {
    const fn is_aot_backend(self) -> bool {
        matches!(self, Self::JitObject | Self::Aot)
    }
}

#[cfg(feature = "llvm")]
pub(super) fn compile_with_state(
    job: CompileJob,
    config: &RuntimeConfig,
    target: CompilerTarget,
    state_slot: &mut Option<CompilerState>,
) -> WorkerResult {
    let _span = match job.kind {
        CompilationKind::Jit => {
            debug_span!("jit_compile", hash=%job.key.code_hash, spec_id=?job.key.spec_id).entered()
        }
        CompilationKind::Aot => {
            debug_span!("aot_compile", hash=%job.key.code_hash, spec_id=?job.key.spec_id).entered()
        }
    };
    let t0 = Instant::now();

    if state_slot.is_none() {
        match CompilerState::new(config, target) {
            Ok(s) => *state_slot = Some(s),
            Err(e) => {
                error!(error = %e, "failed to create LLVM backend");
                return WorkerResult {
                    key: job.key,
                    outcome: Err(e),
                    kind: job.kind,
                    sync_notifier: job.sync_notifier,
                    generation: job.generation,
                    compile_duration: t0.elapsed(),
                    timings: CompileTimings::default(),
                };
            }
        }
    }

    let state = state_slot.as_mut().unwrap();
    let compiler = &mut state.compiler;

    if job.kind == CompilationKind::Jit
        && let Some(base) = &config.dump_dir
    {
        let dir =
            base.join(format!("{:?}", job.key.spec_id)).join(format!("{}", job.key.code_hash));
        compiler.set_dump_to(Some(dir));
    }
    compiler.set_opt_level(job.opt_level);

    let outcome = match target {
        CompilerTarget::Jit => compile_jit_artifact(&job, compiler),
        CompilerTarget::JitObject => compile_jit_object_artifact(&job, compiler),
        CompilerTarget::Aot => compile_aot_artifact(&job, compiler),
    };
    let timings = compiler.take_timings();

    if let Err(err) = compiler.clear_ir() {
        warn!(%err, "clear_ir failed");
    }

    state.compilations_since_recycle += 1;
    if config.tuning.compiler_recycle_threshold > 0
        && state.compilations_since_recycle >= config.tuning.compiler_recycle_threshold
    {
        debug!(compilations_since_recycle = state.compilations_since_recycle, "recycling compiler");
        match CompilerState::new(config, target) {
            Ok(new_state) => {
                *state_slot = Some(new_state);
                revmc_llvm::global_gc();
            }
            Err(e) => {
                error!(error = %e, "failed to recreate compiler");
                state.compilations_since_recycle = 0;
            }
        }
    }

    WorkerResult {
        key: job.key,
        outcome,
        kind: job.kind,
        sync_notifier: job.sync_notifier,
        generation: job.generation,
        compile_duration: t0.elapsed(),
        timings,
    }
}

#[cfg(feature = "llvm")]
pub(super) struct CompilerState {
    compiler: EvmCompiler<EvmLlvmBackend>,
    compilations_since_recycle: usize,
}

#[cfg(feature = "llvm")]
impl CompilerState {
    fn new(config: &RuntimeConfig, target: CompilerTarget) -> Result<Self, String> {
        Ok(Self {
            compiler: create_compiler(config, target.is_aot_backend())?,
            compilations_since_recycle: 0,
        })
    }
}

#[cfg(feature = "llvm")]
thread_local! {
    static JIT_COMPILER: RefCell<Option<CompilerState>> = const { RefCell::new(None) };
    static AOT_COMPILER: RefCell<Option<CompilerState>> = const { RefCell::new(None) };
}

#[cfg(feature = "llvm")]
pub(super) fn create_compiler(
    config: &RuntimeConfig,
    aot: bool,
) -> Result<EvmCompiler<EvmLlvmBackend>, String> {
    let backend = EvmLlvmBackend::new(aot).map_err(|e| e.to_string())?;
    let mut compiler = EvmCompiler::new(backend);
    compiler.set_opt_level(if aot {
        config.tuning.aot_opt_level
    } else {
        config.tuning.jit_opt_level
    });
    compiler.debug_assertions(config.debug_assertions);
    compiler.single_error(config.single_error);
    compiler.set_dedup(!config.no_dedup);
    compiler.set_dse(!config.no_dse);
    if let Some(gas_params) = &config.gas_params {
        compiler.set_gas_params(gas_params.clone());
    }
    Ok(compiler)
}

#[cfg(feature = "llvm")]
fn compile_jit_artifact(
    job: &CompileJob,
    compiler: &mut EvmCompiler<EvmLlvmBackend>,
) -> Result<WorkerSuccess, String> {
    let result = unsafe { compiler.jit(&job.symbol_name, &job.bytecode[..], job.key.spec_id) };
    match result {
        Ok(func) => {
            let jd_guard = compiler.backend_mut().jit_dylib_guard();
            debug!("JIT compilation succeeded");
            let tracker = compiler
                .backend_mut()
                .take_last_resource_tracker()
                .expect("no ResourceTracker after JIT");
            let backing = Arc::new(JitCodeBacking::new(tracker, jd_guard));
            Ok(WorkerSuccess::Jit(JitSuccess { func, backing }))
        }
        Err(err) => {
            warn!(%err, "JIT compilation failed");
            Err(format!("{err}"))
        }
    }
}

/// Compiles a single bytecode to a shared library and returns the raw bytes.
#[cfg(feature = "llvm")]
pub(super) fn compile_jit_object_artifact(
    job: &CompileJob,
    compiler: &mut EvmCompiler<EvmLlvmBackend>,
) -> Result<WorkerSuccess, String> {
    compiler
        .translate(&job.symbol_name, &job.bytecode[..], job.key.spec_id)
        .map_err(|e| format!("JIT object translate failed: {e}"))?;

    let mut object_bytes = Vec::new();
    compiler
        .write_object(&mut object_bytes)
        .map_err(|e| format!("JIT object write failed: {e}"))?;
    let mut builtin_symbols: Vec<String> = compiler
        .backend()
        .pending_symbol_names()
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if builtin_symbols.is_empty() {
        builtin_symbols =
            revmc_builtins::Builtin::ALL.iter().map(|builtin| builtin.name().to_string()).collect();
    }

    debug!(
        bytecode_len = job.bytecode.len(),
        object_len = object_bytes.len(),
        "JIT object compilation succeeded",
    );

    Ok(WorkerSuccess::JitObject(JitObjectSuccess {
        symbol_name: job.symbol_name.clone(),
        object_bytes: Bytes::from(object_bytes),
        builtin_symbols,
    }))
}

#[cfg(feature = "llvm")]
fn compile_aot_artifact(
    job: &CompileJob,
    compiler: &mut EvmCompiler<EvmLlvmBackend>,
) -> Result<WorkerSuccess, String> {
    compiler
        .translate(&job.symbol_name, &job.bytecode[..], job.key.spec_id)
        .map_err(|e| format!("AOT translate failed: {e}"))?;

    let tmp_dir = tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;

    let obj_path = tmp_dir.path().join("a.o");
    let so_path = tmp_dir.path().join("a.so");

    compiler
        .write_object_to_file(&obj_path)
        .map_err(|e| format!("AOT write object failed: {e}"))?;

    let linker = Linker::new();
    linker
        .link(&so_path, [obj_path.to_str().unwrap()])
        .map_err(|e| format!("AOT link failed: {e}"))?;

    let mut dylib_bytes = Vec::new();
    File::open(&so_path)
        .and_then(|mut f| f.read_to_end(&mut dylib_bytes))
        .map_err(|e| format!("failed to read linked .so: {e}"))?;

    debug!(
        bytecode_len = job.bytecode.len(),
        dylib_len = dylib_bytes.len(),
        "AOT compilation succeeded",
    );

    Ok(WorkerSuccess::Aot(AotSuccess {
        symbol_name: job.symbol_name.clone(),
        dylib_bytes,
        bytecode_len: job.bytecode.len(),
    }))
}

#[cfg(not(feature = "llvm"))]
fn compile_job(
    job: CompileJob,
    _config: &RuntimeConfig,
    _helper: OutOfProcessHelper,
) -> WorkerResult {
    WorkerResult {
        key: job.key,
        outcome: Err("LLVM backend not available".into()),
        kind: job.kind,
        sync_notifier: job.sync_notifier,
        generation: job.generation,
        compile_duration: Duration::ZERO,
        timings: CompileTimings::default(),
    }
}
