//! Runtime JIT backend: O(1) compiled-function lookup with background compilation.
//!
//! - Startup AOT preload from [`ArtifactStore::load_all`] into an immutable in-memory map.
//! - O(1) [`JitBackend::lookup`] that only reads the resident map.
//! - Fire-and-forget lookup-observed events to the backend thread.
//! - Background JIT compilation for hot keys (threshold-based promotion).

use std::{
    ops::ControlFlow,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use api::LoadedLibrary;
use backend::{Command, CompileJitRequest, EventQueue, PrepareAotRequest, ResidentMap};
use crossbeam_channel as chan;
use crossbeam_queue::ArrayQueue;
use revm_primitives::{B256, hardfork::SpecId, hints_util::cold_path};
use stats::RuntimeStats;
use worker::SyncNotifier;

use crate::{
    EvmCompilerFn,
    eyre::{self, WrapErr},
};

mod api;
pub use api::{
    AotRequest, CompiledProgram, InterpretReason, LookupDecision, LookupRequest, ProgramKind,
};

mod config;
pub use config::{CompilationEvent, CompilationKind, JitMode, RuntimeConfig, RuntimeTuning};

mod backend;

mod stats;
pub use stats::RuntimeStatsSnapshot;

mod storage;
pub use storage::{
    ArtifactKey, ArtifactManifest, ArtifactStore, BackendSelection, RuntimeArtifactStore,
    RuntimeCacheKey, StoredArtifact,
};

#[cfg(all(feature = "llvm", unix))]
mod out_of_process;

mod worker;

/// Runs the out-of-process JIT helper if this process was launched as one.
///
/// Returns [`ControlFlow::Break`] after the helper request has been handled and
/// the caller should exit immediately. Normal application startup should
/// continue on [`ControlFlow::Continue`].
pub fn maybe_run_jit_helper() -> eyre::Result<ControlFlow<()>> {
    #[cfg(all(feature = "llvm", unix))]
    {
        out_of_process::maybe_run_jit_helper()
    }
    #[cfg(not(all(feature = "llvm", unix)))]
    {
        if std::env::var_os("REVMC_JIT_HELPER").is_some() {
            eyre::bail!("out-of-process JIT helper is only available on Unix with LLVM")
        }
        Ok(ControlFlow::Continue(()))
    }
}

#[cfg(test)]
mod tests;

/// State shared between [`JitBackend`] (via [`BackendInner`]) and the backend
/// thread.
///
/// Held by the backend thread as `Arc<BackendShared>`. Crucially, the backend
/// thread does NOT hold an `Arc<BackendInner>` — that would create a reference
/// cycle since `BackendInner::Drop` is what signals the thread to stop. Keeping
/// thread-lifecycle fields outside this struct lets `BackendInner` drop (and
/// trigger shutdown) as soon as the last `JitBackend` clone is released.
#[derive(derive_more::Debug)]
pub(crate) struct BackendShared {
    /// Shared resident compiled map.
    #[debug(skip)]
    resident: ResidentMap,
    /// Lock-free queue of events.
    #[debug(skip)]
    events: EventQueue,
    /// Number of active out-of-process helper pauses.
    pause_depth: AtomicUsize,
    /// Shared stats counters.
    #[debug(skip)]
    stats: Arc<RuntimeStats>,
}

/// Inner state for [`JitBackend`]. Owns the backend thread lifecycle.
#[derive(derive_more::Debug)]
pub(crate) struct BackendInner {
    /// State shared with the backend thread.
    shared: Arc<BackendShared>,
    /// Global enable flag.
    enabled: AtomicBool,
    /// Whether the backend thread has been spawned and is draining the command channel.
    started: AtomicBool,
    /// Blocking mode: every lookup synchronously compiles and never falls back.
    blocking: bool,
    /// Tuning knobs (Copy). Cached for hot-path eligibility checks.
    tuning: crate::runtime::config::RuntimeTuning,
    /// Bounded channel for control commands (compile_jit, prepare_aot, clears,
    /// shutdown). The lookup hot path does NOT use this — see
    /// [`BackendShared::events`].
    #[debug(skip)]
    tx: chan::Sender<Command>,
    /// Backend thread + done signal. `None` after shutdown.
    #[debug(skip)]
    thread: std::sync::Mutex<Option<BackendThread>>,
    /// Shutdown timeout.
    shutdown_timeout: Duration,
    /// State for lazily spawning the backend thread.
    #[debug(skip)]
    lazy_spawn: std::sync::Mutex<Option<LazySpawnState>>,
}

/// State kept around for lazily spawning the backend thread.
struct LazySpawnState {
    rx: chan::Receiver<Command>,
    config: RuntimeConfig,
}

/// Backend thread handle and its completion signal.
struct BackendThread {
    handle: std::thread::JoinHandle<()>,
    done_rx: chan::Receiver<()>,
}

/// JIT compilation backend with O(1) compiled-function lookup.
///
/// Created via [`JitBackend::new`] or [`JitBackend::disabled`].
/// This type is cheaply clonable (backed by `Arc`).
/// All clones share the same backend thread, resident map, and statistics.
#[derive(Clone, Debug)]
pub struct JitBackend {
    inner: Arc<BackendInner>,
}

impl JitBackend {
    /// Creates a disabled backend that performs no compilation and spawns no threads.
    ///
    /// All [`lookup`](Self::lookup) calls return `LookupDecision::Interpret(Disabled)`.
    /// Call [`set_enabled`](Self::set_enabled) to lazily spawn the backend thread with
    /// a default [`RuntimeConfig`].
    pub fn disabled() -> Self {
        Self::new_inner(RuntimeConfig::default()).expect("default config cannot fail")
    }

    /// Creates a backend from the given config.
    ///
    /// If [`enabled`](RuntimeConfig::enabled) is `true`, the backend thread is spawned
    /// immediately and AOT artifacts are preloaded. Otherwise, both are deferred until the
    /// first [`set_enabled(true)`](Self::set_enabled) call.
    pub fn new(mut config: RuntimeConfig) -> eyre::Result<Self> {
        config = config.with_env_overrides()?;
        Self::new_inner(config)
    }

    fn new_inner(mut config: RuntimeConfig) -> eyre::Result<Self> {
        if config.blocking {
            config.enabled = true;
            config.tuning.jit_hot_threshold = 0;
        }
        #[cfg(not(unix))]
        if config.jit_mode == JitMode::OutOfProcess {
            eyre::bail!("out-of-process JIT is only available on Unix");
        }

        let enabled = config.enabled;
        let (tx, rx) = chan::bounded::<Command>(config.tuning.channel_capacity);
        let events = ArrayQueue::new(config.tuning.channel_capacity);
        let tuning = config.tuning;
        let shared = Arc::new(BackendShared {
            resident: ResidentMap::default(),
            events,
            pause_depth: AtomicUsize::new(0),
            stats: Arc::new(RuntimeStats::default()),
        });
        let this = Self {
            inner: Arc::new(BackendInner {
                shared,
                enabled: AtomicBool::new(false),
                started: AtomicBool::new(false),
                blocking: config.blocking,
                tx,
                thread: std::sync::Mutex::new(None),
                shutdown_timeout: config.tuning.shutdown_timeout,
                tuning,
                lazy_spawn: std::sync::Mutex::new(Some(LazySpawnState { rx, config })),
            }),
        };
        this.set_enabled(enabled)?;
        Ok(this)
    }

    /// Looks up a compiled function for the given request.
    ///
    /// In normal mode this never blocks. In [`blocking`](RuntimeConfig::blocking) mode,
    /// a miss triggers synchronous JIT compilation and the call blocks until it completes.
    pub fn lookup(&self, mut req: LookupRequest) -> LookupDecision {
        let inner = &*self.inner;
        let shared = &*inner.shared;

        if !inner.enabled.load(Ordering::Relaxed) {
            cold_path();
            return LookupDecision::Interpret(InterpretReason::Disabled);
        }
        if inner.blocking {
            cold_path();
            return self.lookup_blocking(req);
        }
        if !inner.tuning.should_compile(&req.code) {
            cold_path();
            return LookupDecision::Interpret(InterpretReason::Ineligible);
        }

        let decision = if let Some(program_ref) = shared.resident.try_get(&req.key).try_unwrap() {
            let program = Arc::clone(&program_ref);
            drop(program_ref);
            req.code.clear();
            LookupDecision::Compiled(program)
        } else {
            LookupDecision::Interpret(InterpretReason::NotReady)
        };

        if let Err(_v) = shared.events.push(req) {
            cold_path();
            shared.stats.events_dropped.fetch_add(1, Ordering::Relaxed);
        }

        decision
    }

    /// Checks the resident map for a compiled program without enqueuing an event.
    pub fn get_compiled(&self, code_hash: B256, spec_id: SpecId) -> Option<Arc<CompiledProgram>> {
        let key = RuntimeCacheKey { code_hash, spec_id };
        self.inner.shared.resident.get(&key).map(|entry| Arc::clone(&entry))
    }

    /// Like [`get_compiled`](Self::get_compiled), but also records hit/miss stats.
    pub fn get_compiled_tracked(
        &self,
        code_hash: B256,
        spec_id: SpecId,
    ) -> Option<Arc<CompiledProgram>> {
        let result = self.get_compiled(code_hash, spec_id);
        if result.is_some() {
            self.inner.shared.stats.lookup_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.inner.shared.stats.lookup_misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Looks up a compiled function, blocking until compilation completes if not yet ready.
    ///
    /// If the function is already compiled, returns it immediately. Otherwise, enqueues
    /// a synchronous JIT compilation request and blocks until the result is available.
    /// Returns [`LookupDecision::Interpret`] if the bytecode is ineligible for
    /// compilation (see [`RuntimeTuning::should_compile`]) or compilation fails.
    pub fn lookup_blocking(&self, req: LookupRequest) -> LookupDecision {
        if !self.inner.tuning.should_compile(&req.code) {
            return LookupDecision::Interpret(InterpretReason::Ineligible);
        }
        let code_hash = req.key.code_hash;
        let spec_id = req.key.spec_id;
        if let Some(program) = self.get_compiled_tracked(code_hash, spec_id) {
            return LookupDecision::Compiled(program);
        }
        if self.compile_jit_sync(req).is_err() {
            return LookupDecision::Interpret(InterpretReason::JitFailed);
        }
        match self.get_compiled_tracked(code_hash, spec_id) {
            Some(program) => LookupDecision::Compiled(program),
            None => LookupDecision::Interpret(InterpretReason::JitFailed),
        }
    }

    /// Enqueues an explicit JIT compilation request for the given bytecode.
    ///
    /// Blocks if the command channel is full to guarantee delivery.
    pub fn compile_jit(&self, req: LookupRequest) {
        let _ = self.ensure_started();
        let cmd = Command::CompileJit(CompileJitRequest {
            key: req.key,
            bytecode: req.code,
            sync_notifier: SyncNotifier::none(),
        });
        let _ = self.inner.tx.send(cmd);
    }

    /// Enqueues a JIT compilation request and blocks until the compilation completes.
    ///
    /// Returns `Ok(())` when the compiled function is available in the resident map,
    /// or when the compilation fails. Use [`get_compiled`](Self::get_compiled) to
    /// retrieve the result after this returns.
    pub fn compile_jit_sync(&self, req: LookupRequest) -> eyre::Result<()> {
        self.ensure_started()?;
        let (tx, rx) = chan::bounded(1);
        let cmd = Command::CompileJit(CompileJitRequest {
            key: req.key,
            bytecode: req.code,
            sync_notifier: SyncNotifier::new(tx),
        });
        self.inner.tx.send(cmd).map_err(|_| eyre::eyre!("backend channel closed"))?;
        rx.recv().map_err(|_| eyre::eyre!("backend shut down before compilation completed"))
    }

    /// Enqueues a single AOT preparation request.
    ///
    /// This is enqueue-only and returns immediately. The compilation happens
    /// asynchronously on the worker pool. The resulting artifact is persisted
    /// via [`ArtifactStore::store`] and loaded into the resident map.
    pub fn prepare_aot(&self, req: AotRequest) {
        self.prepare_aot_batch(vec![req]);
    }

    /// Enqueues a batch of AOT preparation requests.
    ///
    /// Blocks if the command channel is full to guarantee delivery.
    pub fn prepare_aot_batch(&self, reqs: Vec<AotRequest>) {
        let _ = self.ensure_started();
        let owned: Vec<PrepareAotRequest> = reqs
            .into_iter()
            .map(|r| PrepareAotRequest {
                key: RuntimeCacheKey { code_hash: r.code_hash, spec_id: r.spec_id },
                bytecode: r.code,
            })
            .collect();
        let cmd = Command::PrepareAot(owned);
        let _ = self.inner.tx.send(cmd);
    }

    /// Clears the in-memory resident compiled map.
    ///
    /// All compiled programs are removed from the map. Active references
    /// held by callers remain valid until dropped.
    pub fn clear_resident(&self) {
        let _ = self.inner.tx.send(Command::ClearResident);
    }

    /// Clears persisted artifacts from the artifact store.
    pub fn clear_persisted(&self) {
        let _ = self.inner.tx.send(Command::ClearPersisted);
    }

    /// Clears both the resident map and persisted artifacts.
    pub fn clear_all(&self) {
        let _ = self.inner.tx.send(Command::ClearAll);
    }

    /// Returns whether the runtime is enabled.
    pub fn enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Pauses out-of-process helper execution.
    ///
    /// Resident compiled functions are still returned by [`lookup`](Self::lookup), and lookup
    /// events are still processed for stats, hotness tracking, and compilation dispatch. In
    /// out-of-process mode, the helper process group is stopped until the pause depth returns to
    /// zero, so dispatched helper requests remain buffered and resume once the helper continues.
    /// In in-process mode, pause only tracks pause depth.
    ///
    /// Pause delivery is best-effort and never blocks: callers pause around block validation on
    /// the engine's critical path. If the backend thread is not running or the command channel
    /// is full, the command is skipped or dropped instead of blocking.
    pub fn pause(&self) {
        if self.inner.shared.pause_depth.fetch_add(1, Ordering::Relaxed) == 0 {
            self.try_send_control(Command::Pause);
        }
    }

    /// Resumes out-of-process helper execution once all active pauses have been released.
    ///
    /// Resume delivery is best-effort and never blocks, like [`pause`](Self::pause).
    pub fn resume(&self) {
        if self
            .inner
            .shared
            .pause_depth
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |depth| Some(depth.saturating_sub(1)))
            .is_ok_and(|depth| depth == 1)
        {
            self.try_send_control(Command::Resume);
        }
    }

    /// Sends a best-effort control command without ever blocking the caller.
    ///
    /// The command channel is bounded, so a plain `send` can block. Pause/resume are issued on
    /// the caller's block-validation critical path and must never block there:
    ///
    /// - If the backend thread has not been started (the runtime was constructed but compilation
    ///   was never enabled), nothing drains the channel. Queued commands would only accumulate
    ///   until the channel fills and `send` blocks the caller forever, so the send is skipped
    ///   entirely — there is no helper to pause anyway.
    /// - If the channel is full, the command is dropped and recorded in
    ///   [`commands_dropped`](RuntimeStatsSnapshot::commands_dropped). Pause/resume signals are
    ///   idempotent and re-issued on the next pause cycle, so a dropped command self-heals.
    fn try_send_control(&self, cmd: Command) {
        if !self.inner.started.load(Ordering::Relaxed) {
            return;
        }
        if self.inner.tx.try_send(cmd).is_err() {
            self.inner.shared.stats.commands_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns whether out-of-process helper execution is paused.
    pub fn is_paused(&self) -> bool {
        self.inner.shared.pause_depth.load(Ordering::Relaxed) != 0
    }

    /// Sets whether the runtime is enabled, spawning the backend thread on first enable.
    ///
    /// When `enabled` is `true` and the backend thread has not been spawned yet, this
    /// lazily starts it using the config provided at construction time.
    pub fn set_enabled(&self, enabled: bool) -> eyre::Result<()> {
        debug!(enabled, "set_enabled");
        self.inner.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            self.ensure_started()?;
        }
        Ok(())
    }

    /// Returns a point-in-time snapshot of runtime statistics.
    pub fn stats(&self) -> RuntimeStatsSnapshot {
        self.inner.stats()
    }

    /// Spawns the backend thread if it hasn't been started yet.
    fn ensure_started(&self) -> eyre::Result<()> {
        let mut guard = self.inner.lazy_spawn.lock().unwrap();
        let Some(lazy) = guard.take() else {
            return Ok(());
        };

        let LazySpawnState { rx, config } = lazy;

        debug!(
            blocking = self.inner.blocking,
            workers = config.tuning.jit_worker_count,
            jit_mode = ?config.jit_mode,
            hot_threshold = config.tuning.jit_hot_threshold,
            channel_capacity = config.tuning.channel_capacity,
            "spawning backend thread",
        );

        // Preload AOT artifacts into the already-allocated resident map.
        match Self::preload_aot(config.store.as_deref()) {
            Ok(entries) => {
                for (key, prog) in entries {
                    self.inner.shared.resident.insert(key, prog);
                }
            }
            Err(e) => {
                // Restore lazy_spawn so a subsequent attempt can retry.
                *guard = Some(LazySpawnState { rx, config });
                return Err(e);
            }
        }

        drop(guard);

        let (done_tx, done_rx) = chan::bounded::<()>(1);
        let shared = Arc::clone(&self.inner.shared);

        let thread = std::thread::Builder::new()
            .name(config.thread_name.clone())
            .spawn(move || {
                backend::run(shared, rx, config);
                let _ = done_tx.send(());
            })
            .wrap_err("failed to spawn backend thread")?;

        *self.inner.thread.lock().unwrap() = Some(BackendThread { handle: thread, done_rx });
        self.inner.started.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Preloads AOT artifacts from the store as `Arc<CompiledProgram>`s ready to insert.
    fn preload_aot(
        store: Option<&dyn ArtifactStore>,
    ) -> eyre::Result<Vec<(RuntimeCacheKey, Arc<CompiledProgram>)>> {
        let Some(store) = store else {
            debug!("no artifact store configured, skipping AOT preload");
            return Ok(Vec::new());
        };

        let span = info_span!("aot_preload");
        let _enter = span.enter();

        let artifacts = store.load_all()?;
        info!(count = artifacts.len(), "loading AOT artifacts");

        let mut out: Vec<(RuntimeCacheKey, Arc<CompiledProgram>)> =
            Vec::with_capacity(artifacts.len());
        let mut seen = alloy_primitives::map::HashSet::<RuntimeCacheKey>::default();
        let mut loaded = 0u64;
        let mut failed = 0u64;

        for (artifact_key, stored) in artifacts {
            match Self::load_artifact(&artifact_key, &stored) {
                Ok(program) => {
                    let key = artifact_key.runtime;
                    if !seen.insert(key) {
                        warn!(
                            code_hash = %key.code_hash,
                            spec_id = ?key.spec_id,
                            "duplicate artifact key, keeping first",
                        );
                        continue;
                    }
                    out.push((key, Arc::new(program)));
                    loaded += 1;
                }
                Err(e) => {
                    warn!(
                        code_hash = %artifact_key.runtime.code_hash,
                        error = %e,
                        "failed to load artifact, skipping",
                    );
                    failed += 1;
                }
            }
        }

        info!(loaded, failed, "AOT preload complete");
        Ok(out)
    }

    /// Loads a single artifact: `dlopen`s the dylib path and resolves the symbol.
    fn load_artifact(key: &ArtifactKey, stored: &StoredArtifact) -> eyre::Result<CompiledProgram> {
        let library = unsafe { libloading::Library::new(&stored.dylib_path) }
            .wrap_err_with(|| format!("dlopen {:?}", stored.dylib_path))?;

        let func: EvmCompilerFn = unsafe {
            let sym: libloading::Symbol<'_, EvmCompilerFn> = library
                .get(stored.manifest.symbol_name.as_bytes())
                .wrap_err_with(|| format!("symbol '{}'", stored.manifest.symbol_name))?;
            *sym
        };

        let library = Arc::new(LoadedLibrary::new(library));
        Ok(CompiledProgram::new_aot(key.runtime, func, library))
    }
}

impl BackendShared {
    /// Returns a point-in-time snapshot of runtime statistics.
    ///
    /// `command_queue_len` is omitted because the command channel sender lives
    /// on [`BackendInner`], not [`BackendShared`].
    pub(crate) fn stats(&self) -> RuntimeStatsSnapshot {
        self.stats.snapshot(stats::RuntimeStatsGauges {
            resident_entries: self.resident.len() as u64,
            events_queued: self.events.len() as u64,
            command_queue_len: 0,
        })
    }
}

impl BackendInner {
    /// Returns a point-in-time snapshot of runtime statistics.
    pub(crate) fn stats(&self) -> RuntimeStatsSnapshot {
        let mut snap = self.shared.stats();
        snap.command_queue_len = self.tx.len() as u64;
        snap
    }

    fn shutdown(&self) -> eyre::Result<()> {
        debug!("shutting down JIT backend");
        if let Some(ct) = self.thread.lock().unwrap().take() {
            // Ignoring send error — backend may already be gone.
            let _ = self.tx.send(Command::Shutdown);

            // Wait for the thread to signal completion, with a timeout.
            match ct.done_rx.recv_timeout(self.shutdown_timeout) {
                Ok(()) | Err(chan::RecvTimeoutError::Disconnected) => {}
                Err(chan::RecvTimeoutError::Timeout) => {
                    eyre::bail!(
                        "backend thread did not exit within timeout ({:?})",
                        self.shutdown_timeout
                    );
                }
            }

            // Thread signaled done, join should return immediately.
            ct.handle.join().map_err(|_| eyre::eyre!("backend thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for BackendInner {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown() {
            warn!(%err, "failed to shutdown JIT backend");
        }
    }
}
