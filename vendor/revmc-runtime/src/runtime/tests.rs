//! Tests for the runtime module.

use std::sync::{Arc, Mutex};

use alloy_primitives::{B256, Bytes};
use revm_primitives::hardfork::SpecId;

use super::*;

// ---------------------------------------------------------------------------
// Test bytecodes.
// ---------------------------------------------------------------------------

/// PUSH1 0x42 PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN — returns 0x42.
const BYTECODE_RET42: &[u8] = &[0x60, 0x42, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

/// PUSH1 1 PUSH1 1 ADD PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN — returns 2.
const BYTECODE_ADD: &[u8] = &[0x60, 0x01, 0x60, 0x01, 0x01, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3];

/// Returns a simple bytecode that varies by index (for generating distinct code hashes).
fn indexed_bytecode(i: u8) -> Vec<u8> {
    vec![0x60, i, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]
}

// ---------------------------------------------------------------------------
// Test harness.
// ---------------------------------------------------------------------------

/// Helper: polls `f` until it returns `Some(T)` or the deadline is reached.
fn poll_until<T>(timeout: std::time::Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "timed out polling");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Test harness wrapping a [`JitBackend`] with convenience methods.
///
/// Automatically shuts down the backend on drop.
struct TestBackend {
    backend: JitBackend,
}

impl TestBackend {
    /// Creates a backend with the given config.
    fn new(config: RuntimeConfig) -> Self {
        Self { backend: JitBackend::new(config).unwrap() }
    }

    /// Creates a backend with `enabled: true` and the given tuning overrides.
    fn with_tuning(tuning: RuntimeTuning) -> Self {
        Self::new(RuntimeConfig { enabled: true, tuning, ..Default::default() })
    }

    /// Creates a backend with `enabled: true`, 1 worker, and the given tuning overrides.
    fn with_tuning_1w(tuning: RuntimeTuning) -> Self {
        Self::with_tuning(RuntimeTuning { jit_worker_count: 1, ..tuning })
    }

    /// Creates a backend with `enabled: true`, 1 worker, and default tuning.
    fn default_1w() -> Self {
        Self::with_tuning_1w(RuntimeTuning::default())
    }

    /// Builds a [`LookupRequest`] for the given bytecode and spec.
    fn req(bytecode: &[u8], spec_id: SpecId) -> LookupRequest {
        LookupRequest {
            key: RuntimeCacheKey { code_hash: alloy_primitives::keccak256(bytecode), spec_id },
            code: Bytes::copy_from_slice(bytecode),
        }
    }

    /// Builds a [`LookupRequest`] with [`SpecId::CANCUN`].
    fn req_cancun(bytecode: &[u8]) -> LookupRequest {
        Self::req(bytecode, SpecId::CANCUN)
    }

    /// Sends lookups until JIT promotion triggers and the program appears in the resident map.
    /// Returns the compiled program.
    fn trigger_jit(&self, bytecode: &[u8], spec_id: SpecId) -> Arc<CompiledProgram> {
        for _ in 0..10 {
            let _ = self.backend.lookup(Self::req(bytecode, spec_id));
        }
        self.wait_compiled(bytecode, spec_id)
    }

    /// Like [`trigger_jit`](Self::trigger_jit) but with [`SpecId::CANCUN`].
    fn trigger_jit_cancun(&self, bytecode: &[u8]) -> Arc<CompiledProgram> {
        self.trigger_jit(bytecode, SpecId::CANCUN)
    }

    /// Polls until the given bytecode is compiled and available in the resident map.
    fn wait_compiled(&self, bytecode: &[u8], spec_id: SpecId) -> Arc<CompiledProgram> {
        poll_until(std::time::Duration::from_secs(30), || {
            match self.backend.lookup(Self::req(bytecode, spec_id)) {
                LookupDecision::Compiled(p) => Some(p),
                _ => None,
            }
        })
    }

    /// Polls until the resident map reaches the expected entry count.
    fn wait_resident_count(&self, expected: u64) {
        poll_until(std::time::Duration::from_secs(5), || {
            let n = self.backend.stats().resident_entries;
            if n == expected { Some(()) } else { None }
        });
    }

    /// Polls until the stats snapshot satisfies the given predicate, then returns it.
    fn wait_stats(
        &self,
        mut pred: impl FnMut(&RuntimeStatsSnapshot) -> bool,
    ) -> RuntimeStatsSnapshot {
        poll_until(std::time::Duration::from_secs(5), || {
            let s = self.backend.stats();
            if pred(&s) { Some(s) } else { None }
        })
    }

    fn stats(&self) -> RuntimeStatsSnapshot {
        self.backend.stats()
    }
}

impl std::ops::Deref for TestBackend {
    type Target = JitBackend;
    fn deref(&self) -> &JitBackend {
        &self.backend
    }
}

// `JitBackend` shuts down automatically when the last `Arc<BackendInner>` drops.

// ---------------------------------------------------------------------------
// Artifact stores for testing.
// ---------------------------------------------------------------------------

/// A no-op artifact store that returns no artifacts.
struct EmptyStore;

impl ArtifactStore for EmptyStore {
    fn load_all(&self) -> eyre::Result<Vec<(ArtifactKey, StoredArtifact)>> {
        Ok(vec![])
    }

    fn load(&self, _key: &ArtifactKey) -> eyre::Result<Option<StoredArtifact>> {
        Ok(None)
    }

    fn store(
        &self,
        _key: &ArtifactKey,
        _manifest: &ArtifactManifest,
        _dylib_bytes: &[u8],
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn delete(&self, _key: &ArtifactKey) -> eyre::Result<()> {
        Ok(())
    }

    fn clear(&self) -> eyre::Result<()> {
        Ok(())
    }
}

/// A store that fails on load_all.
struct FailingStore;

impl ArtifactStore for FailingStore {
    fn load_all(&self) -> eyre::Result<Vec<(ArtifactKey, StoredArtifact)>> {
        Err(std::io::Error::other("boom").into())
    }

    fn load(&self, _key: &ArtifactKey) -> eyre::Result<Option<StoredArtifact>> {
        Ok(None)
    }

    fn store(
        &self,
        _key: &ArtifactKey,
        _manifest: &ArtifactManifest,
        _dylib_bytes: &[u8],
    ) -> eyre::Result<()> {
        Ok(())
    }

    fn delete(&self, _key: &ArtifactKey) -> eyre::Result<()> {
        Ok(())
    }

    fn clear(&self) -> eyre::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Tests: startup / basic.
// ===========================================================================

#[test]
fn start_no_store() {
    let tb = TestBackend::new(RuntimeConfig::default());
    assert_eq!(tb.stats().resident_entries, 0);
}

#[test]
fn start_empty_store() {
    let config =
        RuntimeConfig { enabled: true, store: Some(Arc::new(EmptyStore)), ..Default::default() };
    let tb = TestBackend::new(config);
    assert_eq!(tb.stats().resident_entries, 0);
}

#[test]
fn startup_store_failure() {
    let config =
        RuntimeConfig { enabled: true, store: Some(Arc::new(FailingStore)), ..Default::default() };
    let result = JitBackend::new(config);
    assert!(result.is_err());
}

// ===========================================================================
// Tests: lookup behavior.
// ===========================================================================

#[test]
fn zero_workers_fails_sync_compile_without_blocking() {
    let tb = TestBackend::new(RuntimeConfig {
        blocking: true,
        tuning: RuntimeTuning { jit_worker_count: 0, ..Default::default() },
        ..Default::default()
    });

    let decision = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
    assert!(matches!(decision, LookupDecision::Interpret(InterpretReason::JitFailed)));

    let stats = tb.stats();
    assert_eq!(stats.resident_entries, 0);
    assert_eq!(stats.compilations_dispatched, 0);
}

#[test]
fn lookup_disabled() {
    let tb = TestBackend::new(RuntimeConfig { enabled: false, ..Default::default() });

    let req = TestBackend::req_cancun(&[0x00]);
    let decision = tb.lookup(req);
    assert!(matches!(decision, LookupDecision::Interpret(InterpretReason::Disabled)));

    let stats = tb.stats();
    assert_eq!(stats.lookup_hits, 0);
    assert_eq!(stats.lookup_misses, 0);
}

#[test]
fn lookup_miss_when_enabled() {
    let tb = TestBackend::new(RuntimeConfig { enabled: true, ..Default::default() });

    let decision = tb.lookup(TestBackend::req_cancun(&[0x00]));
    assert!(matches!(decision, LookupDecision::Interpret(InterpretReason::NotReady)));

    let stats = tb.wait_stats(|s| s.lookup_misses == 1);
    assert_eq!(stats.lookup_misses, 1);
    assert_eq!(stats.lookup_hits, 0);
}

#[test]
fn set_enabled_toggle() {
    let tb = TestBackend::new(RuntimeConfig { enabled: false, ..Default::default() });
    let req = TestBackend::req_cancun(&[0x00]);

    assert!(matches!(tb.lookup(req.clone()), LookupDecision::Interpret(InterpretReason::Disabled)));

    tb.set_enabled(true).unwrap();
    assert!(matches!(tb.lookup(req.clone()), LookupDecision::Interpret(InterpretReason::NotReady)));

    tb.set_enabled(false).unwrap();
    assert!(matches!(tb.lookup(req), LookupDecision::Interpret(InterpretReason::Disabled)));
}

#[test]
fn pause_processes_lookup_events() {
    let tb = TestBackend::with_tuning(RuntimeTuning { jit_worker_count: 0, ..Default::default() });
    let req = TestBackend::req_cancun(&[0x00]);

    assert!(!tb.is_paused());
    tb.pause();
    tb.pause();
    assert!(tb.is_paused());
    assert!(matches!(tb.lookup(req.clone()), LookupDecision::Interpret(InterpretReason::NotReady)));

    let stats = tb.wait_stats(|s| s.lookup_misses == 1);
    assert_eq!(stats.lookup_misses, 1);
    assert_eq!(stats.lookup_hits, 0);

    tb.resume();
    assert!(tb.is_paused());
    tb.resume();
    assert!(!tb.is_paused());
    assert!(matches!(tb.lookup(req), LookupDecision::Interpret(InterpretReason::NotReady)));

    let stats = tb.wait_stats(|s| s.lookup_misses == 2);
    assert_eq!(stats.lookup_misses, 2);
    assert_eq!(stats.lookup_hits, 0);
}

#[test]
fn lookup_increments_miss_counter() {
    let tb = TestBackend::new(RuntimeConfig { enabled: true, ..Default::default() });

    for _ in 0..10 {
        let _ = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
    }

    // All 10 lookups missed (no compiled program), so misses == 10.
    let stats = tb.wait_stats(|s| s.lookup_misses == 10);
    assert_eq!(stats.lookup_misses, 10);
    assert_eq!(stats.lookup_hits, 0);
}

#[test]
fn drop_shuts_down_backend() {
    let backend = JitBackend::new(RuntimeConfig::default()).unwrap();
    let backend2 = backend.clone();
    drop(backend);

    // Lookups still work (no panic) — backend is still running because backend2 holds a ref.
    let _ = backend2.lookup(TestBackend::req_cancun(&[]));
}

#[test]
fn backend_clone() {
    let tb = TestBackend::new(RuntimeConfig { enabled: true, ..Default::default() });
    let b2 = tb.backend.clone();

    let _ = tb.lookup(TestBackend::req_cancun(&[0x00]));
    let _ = b2.lookup(TestBackend::req_cancun(&[0x00]));

    let stats = tb.wait_stats(|s| s.lookup_misses == 2);
    assert_eq!(stats.lookup_misses, 2);
}

// ===========================================================================
// Tests: enqueue APIs (fire-and-forget, no LLVM needed).
// ===========================================================================

#[test]
fn compile_jit_enqueue() {
    let tb = TestBackend::with_tuning(RuntimeTuning { jit_worker_count: 0, ..Default::default() });
    tb.compile_jit(TestBackend::req_cancun(BYTECODE_RET42));
}

#[test]
fn prepare_aot_enqueue() {
    let tb = TestBackend::with_tuning(RuntimeTuning { jit_worker_count: 0, ..Default::default() });
    tb.prepare_aot(AotRequest {
        code_hash: alloy_primitives::keccak256(BYTECODE_RET42),
        code: Bytes::copy_from_slice(BYTECODE_RET42),
        spec_id: SpecId::CANCUN,
    });
}

#[test]
fn prepare_aot_batch_enqueue() {
    let tb = TestBackend::with_tuning(RuntimeTuning { jit_worker_count: 0, ..Default::default() });
    let reqs = vec![
        AotRequest {
            code_hash: alloy_primitives::keccak256(BYTECODE_RET42),
            code: Bytes::copy_from_slice(BYTECODE_RET42),
            spec_id: SpecId::CANCUN,
        },
        AotRequest {
            code_hash: alloy_primitives::keccak256(BYTECODE_ADD),
            code: Bytes::copy_from_slice(BYTECODE_ADD),
            spec_id: SpecId::CANCUN,
        },
    ];
    tb.prepare_aot_batch(reqs);
}

#[test]
fn clear_resident() {
    let tb = TestBackend::new(RuntimeConfig { enabled: true, ..Default::default() });
    tb.clear_resident();

    let decision = tb.lookup(TestBackend::req_cancun(&[0x00]));
    assert!(matches!(decision, LookupDecision::Interpret(InterpretReason::NotReady)));
}

#[test]
fn sub_threshold_misses_do_not_dispatch() {
    // Inline counters live on the resident entry; threshold-crossing dispatches
    // exactly one CompileJit. With a tiny command channel and threshold above
    // the call count, no dispatch occurs and lookups never block.
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        channel_capacity: 2,
        jit_hot_threshold: 1000,
        ..Default::default()
    });

    for _ in 0..200 {
        let _ = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
    }

    // Misses are tracked asynchronously by the backend, with overflow dropped.
    // The two together must add up to the total lookup count.
    let stats = tb.wait_stats(|s| s.lookup_misses + s.events_dropped == 200);
    assert_eq!(stats.lookup_misses + stats.events_dropped, 200);
    assert_eq!(stats.compilations_dispatched, 0);
}

#[test]
fn pause_resume_never_block_without_backend_thread() {
    // When the runtime is constructed but compilation is never enabled, the backend thread is
    // never spawned and nothing drains the bounded command channel. Pause/resume are issued once
    // per validated block on the engine's critical path, so queueing them would fill the channel
    // after `channel_capacity` sends and block the caller forever (observed in production as a
    // consensus-engine deadlock after exactly capacity/2 blocks). They must be skipped, not
    // queued, while the backend is not started.
    let tb = TestBackend::new(RuntimeConfig {
        enabled: false,
        tuning: RuntimeTuning { channel_capacity: 2, ..Default::default() },
        ..Default::default()
    });

    // More pause/resume cycles than the channel can hold: deadlocks without the started check.
    for _ in 0..16 {
        tb.pause();
        assert!(tb.is_paused());
        tb.resume();
        assert!(!tb.is_paused());
    }

    // Nothing was queued or dropped: the sends were skipped entirely.
    let stats = tb.stats();
    assert_eq!(stats.command_queue_len, 0);
    assert_eq!(stats.commands_dropped, 0);
}

#[test]
fn pause_resume_depth_tracking_with_backend_thread() {
    // With the backend running, pause/resume still track depth and deliver commands.
    let tb = TestBackend::with_tuning_1w(RuntimeTuning::default());

    tb.pause();
    tb.pause();
    assert!(tb.is_paused());
    tb.resume();
    assert!(tb.is_paused());
    tb.resume();
    assert!(!tb.is_paused());

    // Commands are drained by the backend thread rather than accumulating.
    let stats = tb.wait_stats(|s| s.command_queue_len == 0);
    assert_eq!(stats.command_queue_len, 0);
    assert_eq!(stats.commands_dropped, 0);
}

// ===========================================================================
// Tests: JIT compilation (require LLVM).
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn blocking_mode() {
    let tb = TestBackend::new(RuntimeConfig {
        blocking: true,
        tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    let req = || TestBackend::req_cancun(BYTECODE_RET42);

    // First lookup should block until JIT finishes, then return Compiled.
    let decision = tb.lookup(req());
    assert!(
        matches!(&decision, LookupDecision::Compiled(p) if p.kind == ProgramKind::Jit),
        "blocking mode should return Compiled on first lookup, got: {decision:?}",
    );

    // Second lookup should hit the resident map immediately.
    assert!(matches!(tb.lookup(req()), LookupDecision::Compiled(_)));

    // Empty bytecodes return Ineligible (nothing to compile).
    let decision = tb.lookup(TestBackend::req_cancun(&[]));
    assert!(matches!(decision, LookupDecision::Interpret(InterpretReason::Ineligible)));
}

#[test]
fn default_jit_max_bytecode_len_matches_eth_limit() {
    let tuning = RuntimeTuning::default();
    let max_len_bytecode = vec![0; 24 * 1024];
    let too_large_bytecode = vec![0; 24 * 1024 + 1];

    assert_eq!(tuning.jit_max_bytecode_len, 24 * 1024);
    assert!(tuning.should_compile(&max_len_bytecode));
    assert!(!tuning.should_compile(&too_large_bytecode));
}

#[test]
#[cfg(feature = "llvm")]
fn jit_hotness_promotion() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 3, ..Default::default() });

    let req = || TestBackend::req_cancun(BYTECODE_RET42);

    // Below threshold: should remain NotReady.
    for _ in 0..2 {
        assert!(matches!(tb.lookup(req()), LookupDecision::Interpret(InterpretReason::NotReady)));
    }

    // Hit threshold and beyond: JIT should eventually compile.
    let p = tb.trigger_jit_cancun(BYTECODE_RET42);
    assert_eq!(p.kind, ProgramKind::Jit);

    let stats = tb.wait_stats(|s| s.resident_entries >= 1 && s.lookup_hits >= 1);
    assert!(stats.resident_entries >= 1);
    assert!(stats.lookup_hits >= 1);
}

#[test]
#[cfg(feature = "llvm")]
fn jit_max_bytecode_len_prevents_promotion() {
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 1,
        jit_max_bytecode_len: 4,
        ..Default::default()
    });

    let req = || TestBackend::req_cancun(BYTECODE_RET42);

    // Send many lookups to exceed any threshold.
    for _ in 0..20 {
        let _ = tb.lookup(req());
    }

    // Give the backend time to process events.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Should be Ineligible — bytecode length exceeds jit_max_bytecode_len.
    assert!(matches!(tb.lookup(req()), LookupDecision::Interpret(InterpretReason::Ineligible)));
}

#[test]
#[cfg(feature = "llvm")]
fn clear_resident_discards_inflight_jit() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 1, ..Default::default() });

    let req = || TestBackend::req_cancun(BYTECODE_RET42);

    // Trigger JIT promotion.
    for _ in 0..5 {
        let _ = tb.lookup(req());
    }

    // Immediately clear — in-flight JIT results should be discarded.
    tb.clear_resident();

    // Give time for worker to finish and backend to process.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // The result from the old generation should have been discarded.
    assert!(
        matches!(tb.lookup(req()), LookupDecision::Interpret(InterpretReason::NotReady)),
        "stale JIT result should not appear after clear_resident",
    );
}

#[test]
#[cfg(feature = "llvm")]
fn jit_memory_tracks_compilation() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 1, ..Default::default() });

    let baseline = tb.stats().jit_code_bytes;

    tb.trigger_jit_cancun(BYTECODE_RET42);

    let stats = tb.stats();
    assert!(stats.jit_code_bytes > baseline, "jit_code_bytes should increase after JIT");
    assert_eq!(stats.resident_entries, 1);
}

#[test]
#[cfg(feature = "llvm")]
fn compile_jit_sync_blocks() {
    let tb = TestBackend::default_1w();

    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);
    tb.compile_jit_sync(TestBackend::req_cancun(BYTECODE_RET42)).unwrap();

    let program = tb.get_compiled(code_hash, SpecId::CANCUN);
    assert!(program.is_some(), "program should be resident after compile_jit_sync");
    assert_eq!(program.unwrap().kind, ProgramKind::Jit);
}

#[test]
#[cfg(feature = "llvm")]
fn compile_jit_sync_deduplicates() {
    let compiled_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let compiled_count2 = compiled_count.clone();

    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
        on_compilation: Some(Arc::new(move |event| {
            if event.success {
                compiled_count2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })),
        ..Default::default()
    });

    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    // Multiple sync compiles from different threads.
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let b = tb.backend.clone();
            let r = TestBackend::req_cancun(BYTECODE_RET42);
            std::thread::spawn(move || {
                let _ = b.compile_jit_sync(r);
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    assert!(tb.get_compiled(code_hash, SpecId::CANCUN).is_some());
    assert_eq!(
        compiled_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "should compile exactly once even with concurrent sync requests",
    );
}

// ===========================================================================
// Tests: multiple SpecIds.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn multiple_spec_ids() {
    let tb = TestBackend::with_tuning(RuntimeTuning {
        jit_hot_threshold: 1,
        jit_worker_count: 2,
        ..Default::default()
    });

    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    for &spec_id in &[SpecId::CANCUN, SpecId::PRAGUE] {
        tb.trigger_jit(BYTECODE_RET42, spec_id);
    }

    assert!(tb.stats().resident_entries >= 2);

    let p1 = tb.get_compiled(code_hash, SpecId::CANCUN).unwrap();
    let p2 = tb.get_compiled(code_hash, SpecId::PRAGUE).unwrap();
    assert_eq!(p1.key.spec_id, SpecId::CANCUN);
    assert_eq!(p2.key.spec_id, SpecId::PRAGUE);
}

// ===========================================================================
// Tests: eviction.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn idle_eviction() {
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 1,
        idle_evict_duration: Some(std::time::Duration::from_millis(100)),
        eviction_sweep_interval: std::time::Duration::from_millis(50),
        ..Default::default()
    });

    tb.trigger_jit_cancun(BYTECODE_RET42);
    assert_eq!(tb.stats().resident_entries, 1);

    // Stop hitting the entry and wait for idle eviction.
    tb.wait_resident_count(0);
    assert_eq!(tb.stats().resident_entries, 0);
}

#[test]
#[cfg(feature = "llvm")]
fn eviction_frees_jit_memory() {
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 1,
        idle_evict_duration: Some(std::time::Duration::from_millis(100)),
        eviction_sweep_interval: std::time::Duration::from_millis(50),
        ..Default::default()
    });

    let baseline = tb.stats().jit_code_bytes;

    let compiled = tb.trigger_jit_cancun(BYTECODE_RET42);
    let after_compile = tb.stats().jit_code_bytes;
    assert!(
        after_compile > baseline,
        "jit_code_bytes should increase: baseline={baseline}, after={after_compile}",
    );

    // Drop our reference so eviction can free it.
    drop(compiled);

    tb.wait_resident_count(0);
    std::thread::sleep(std::time::Duration::from_millis(100));

    poll_until(std::time::Duration::from_secs(5), || {
        let after_eviction = tb.stats().jit_code_bytes;
        (after_eviction < after_compile).then_some(())
    });
}

#[test]
#[cfg(feature = "llvm")]
fn memory_budget_eviction() {
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 1,
        // Tiny budget: 1 byte — any entry should be evicted.
        resident_code_cache_bytes: 1,
        eviction_sweep_interval: std::time::Duration::from_millis(50),
        ..Default::default()
    });

    // Hammer lookups so the entry promotes; we can't observe it as Compiled
    // (eviction races with insertion under budget pressure), so wait on the
    // evictions counter instead.
    let req = TestBackend::req_cancun(BYTECODE_RET42);
    poll_until(std::time::Duration::from_secs(10), || {
        let _ = tb.lookup(req.clone());
        if tb.stats().evictions >= 1 { Some(()) } else { None }
    });
}

// ===========================================================================
// Tests: concurrency.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn concurrent_lookup_same_key() {
    let tb = TestBackend::with_tuning(RuntimeTuning {
        jit_hot_threshold: 1,
        jit_worker_count: 2,
        ..Default::default()
    });

    tb.trigger_jit_cancun(BYTECODE_RET42);

    // Hammer from many threads.
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let b = tb.backend.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    let d = b.lookup(TestBackend::req_cancun(BYTECODE_RET42));
                    assert!(
                        matches!(d, LookupDecision::Compiled(_)),
                        "expected Compiled, got: {d:?}"
                    );
                }
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    let stats = tb.wait_stats(|s| s.lookup_hits + s.events_dropped >= 800);
    assert!(
        stats.lookup_hits + stats.events_dropped >= 800,
        "expected ≥800 hits+dropped, got hits={} dropped={}",
        stats.lookup_hits,
        stats.events_dropped,
    );
}

#[test]
#[cfg(feature = "llvm")]
fn single_jit_admission_per_key() {
    let compiled_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let compiled_count2 = compiled_count.clone();

    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        tuning: RuntimeTuning { jit_hot_threshold: 1, jit_worker_count: 2, ..Default::default() },
        on_compilation: Some(Arc::new(move |event| {
            if event.success {
                compiled_count2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })),
        ..Default::default()
    });

    // Many lookups to trigger admission multiple times.
    for _ in 0..50 {
        let _ = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
    }

    tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);

    // Give extra time for any spurious duplicate compilations.
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert_eq!(
        compiled_count.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "key should be compiled exactly once",
    );
}

#[test]
#[cfg(feature = "llvm")]
fn stats_accuracy_concurrent() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 1, ..Default::default() });

    tb.trigger_jit_cancun(BYTECODE_RET42);

    let before = tb.stats();

    let n_threads = 4u64;
    let n_lookups = 50u64;
    let threads: Vec<_> = (0..n_threads)
        .map(|_| {
            let b = tb.backend.clone();
            std::thread::spawn(move || {
                for _ in 0..n_lookups {
                    let _ = b.lookup(TestBackend::req_cancun(BYTECODE_RET42));
                }
            })
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }

    let total_lookups = n_threads * n_lookups;
    // Stats are updated asynchronously by the backend; on overflow events drop.
    // Pre-existing in-flight events from trigger_jit_cancun may also slip in,
    // so use `>=` rather than equality.
    let after = tb.wait_stats(|s| {
        let new_hits = s.lookup_hits - before.lookup_hits;
        let new_dropped = s.events_dropped - before.events_dropped;
        new_hits + new_dropped >= total_lookups
    });
    let new_hits = after.lookup_hits - before.lookup_hits;
    let new_dropped = after.events_dropped - before.events_dropped;
    let new_misses = after.lookup_misses - before.lookup_misses;

    assert!(
        new_hits + new_dropped >= total_lookups,
        "all lookups should be accounted for: hits={new_hits} dropped={new_dropped} total={total_lookups}",
    );
    assert_eq!(new_misses, 0, "all lookups should be hits");
}

// ===========================================================================
// Tests: lifecycle (clear, set_enabled, shutdown).
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn clear_during_active_work() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 1, ..Default::default() });

    // Fire many JIT requests to keep workers busy.
    for i in 0u8..20 {
        let bytecode = indexed_bytecode(i);
        let _ = tb.lookup(TestBackend::req_cancun(&bytecode));
    }

    // Clear while work is in-flight.
    tb.clear_resident();
    tb.clear_all();

    // Should not panic.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let _stats = tb.stats();
}

#[test]
#[cfg(feature = "llvm")]
fn set_enabled_during_compilation() {
    let tb =
        TestBackend::with_tuning_1w(RuntimeTuning { jit_hot_threshold: 1, ..Default::default() });

    // Trigger JIT.
    for _ in 0..5 {
        let _ = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
    }

    // Disable while compilation may be in progress.
    tb.set_enabled(false).unwrap();
    assert!(matches!(
        tb.lookup(TestBackend::req_cancun(BYTECODE_RET42)),
        LookupDecision::Interpret(InterpretReason::Disabled)
    ));

    // Re-enable and check compilation eventually completes.
    tb.set_enabled(true).unwrap();
    tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
}

// ===========================================================================
// Tests: on_compilation callback.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn on_compilation_callback() {
    let events: Arc<Mutex<Vec<CompilationEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let events2 = events.clone();

    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        tuning: RuntimeTuning { jit_hot_threshold: 1, jit_worker_count: 1, ..Default::default() },
        on_compilation: Some(Arc::new(move |event| {
            events2.lock().unwrap().push(event);
        })),
        ..Default::default()
    });

    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);
    tb.trigger_jit_cancun(BYTECODE_RET42);

    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 1, "expected exactly one compilation event");
    let event = &captured[0];
    assert_eq!(event.code_hash, code_hash);
    assert_eq!(event.spec_id, SpecId::CANCUN);
    assert!(event.success);
    assert_eq!(event.kind, CompilationKind::Jit);
    assert!(!event.duration.is_zero());
}

// ===========================================================================
// Tests: AOT prepare and persistence.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn prepare_aot_persist_and_load() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        store: Some(store.clone()),
        tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);
    tb.prepare_aot(AotRequest {
        code_hash,
        code: Bytes::copy_from_slice(BYTECODE_RET42),
        spec_id: SpecId::CANCUN,
    });

    let p = tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    assert_eq!(p.kind, ProgramKind::Aot);
    assert_eq!(store.len(), 1);
}

#[test]
#[cfg(feature = "llvm")]
fn aot_mode_promotes_misses_to_aot() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        aot: true,
        store: Some(store.clone()),
        tuning: RuntimeTuning { jit_hot_threshold: 2, jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    assert!(matches!(
        tb.lookup(TestBackend::req_cancun(BYTECODE_RET42)),
        LookupDecision::Interpret(_)
    ));
    assert!(matches!(
        tb.lookup(TestBackend::req_cancun(BYTECODE_RET42)),
        LookupDecision::Interpret(_)
    ));

    let p = tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    assert_eq!(p.kind, ProgramKind::Aot);
    assert_eq!(store.len(), 1);
}

#[test]
#[cfg(feature = "llvm")]
fn prepare_aot_batch_persist_and_load() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        store: Some(store.clone()),
        tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    let bytecodes: &[&[u8]] = &[BYTECODE_RET42, BYTECODE_ADD];
    let hashes: Vec<B256> = bytecodes.iter().map(alloy_primitives::keccak256).collect();

    let reqs: Vec<AotRequest> = bytecodes
        .iter()
        .zip(&hashes)
        .map(|(code, hash)| AotRequest {
            code_hash: *hash,
            code: Bytes::copy_from_slice(code),
            spec_id: SpecId::CANCUN,
        })
        .collect();
    tb.prepare_aot_batch(reqs);

    for bc in bytecodes {
        tb.wait_compiled(bc, SpecId::CANCUN);
    }
    assert_eq!(store.len(), 2);
}

#[test]
#[cfg(feature = "llvm")]
fn aot_artifacts_survive_restart() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    // First backend: compile and persist an AOT artifact.
    {
        let tb = TestBackend::new(RuntimeConfig {
            enabled: true,
            store: Some(store.clone()),
            tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
            ..Default::default()
        });

        tb.prepare_aot(AotRequest {
            code_hash,
            code: Bytes::copy_from_slice(BYTECODE_RET42),
            spec_id: SpecId::CANCUN,
        });
        tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    }

    assert_eq!(store.len(), 1);

    // Second backend: should preload the artifact at startup.
    {
        let tb = TestBackend::new(RuntimeConfig {
            enabled: true,
            store: Some(store),
            ..Default::default()
        });

        let decision = tb.lookup(TestBackend::req_cancun(BYTECODE_RET42));
        assert!(
            matches!(&decision, LookupDecision::Compiled(p) if p.kind == ProgramKind::Aot),
            "expected AOT hit after restart, got: {decision:?}",
        );
        assert_eq!(tb.stats().resident_entries, 1);
    }
}

#[test]
#[cfg(feature = "llvm")]
fn preload_aot_seeds_resident() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    // First backend: compile and persist.
    {
        let tb = TestBackend::new(RuntimeConfig {
            enabled: true,
            store: Some(store.clone()),
            tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
            ..Default::default()
        });

        tb.prepare_aot(AotRequest {
            code_hash,
            code: Bytes::copy_from_slice(BYTECODE_RET42),
            spec_id: SpecId::CANCUN,
        });
        tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    }

    // Second backend: preloaded AOT should be available immediately.
    {
        let tb = TestBackend::new(RuntimeConfig {
            enabled: true,
            store: Some(store),
            ..Default::default()
        });
        assert_eq!(tb.stats().resident_entries, 1);
    }
}

/// `prepare_aot()` on a key that is already persisted in the store but not resident
/// (e.g. after a `clear_resident`) loads from the store without recompiling.
#[test]
#[cfg(feature = "llvm")]
fn prepare_aot_already_persisted_not_resident() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        store: Some(store.clone()),
        tuning: RuntimeTuning { jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    // Compile and persist an AOT artifact.
    tb.prepare_aot(AotRequest {
        code_hash,
        code: Bytes::copy_from_slice(BYTECODE_RET42),
        spec_id: SpecId::CANCUN,
    });
    tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    assert_eq!(store.len(), 1);

    let dispatched_before = tb.stats().compilations_dispatched;

    // Clear resident — artifact is still persisted in the store but not in memory.
    tb.clear_resident();
    tb.wait_resident_count(0);

    // Requesting prepare_aot again should load from store without recompiling.
    tb.prepare_aot(AotRequest {
        code_hash,
        code: Bytes::copy_from_slice(BYTECODE_RET42),
        spec_id: SpecId::CANCUN,
    });

    let p = tb.wait_compiled(BYTECODE_RET42, SpecId::CANCUN);
    assert_eq!(p.kind, ProgramKind::Aot);

    // No new compilation should have been dispatched — loaded from store.
    assert_eq!(
        tb.stats().compilations_dispatched,
        dispatched_before,
        "should not recompile when artifact is already persisted",
    );
}

/// `prepare_aot()` on a key that is already resident as JIT should be skipped.
/// This documents the current behavior (todo: JIT-resident code should still persist).
#[test]
#[cfg(feature = "llvm")]
fn prepare_aot_skips_jit_resident() {
    let store = Arc::new(RuntimeArtifactStore::new().unwrap());
    let code_hash = alloy_primitives::keccak256(BYTECODE_RET42);

    let tb = TestBackend::new(RuntimeConfig {
        enabled: true,
        store: Some(store.clone()),
        tuning: RuntimeTuning { jit_hot_threshold: 1, jit_worker_count: 1, ..Default::default() },
        ..Default::default()
    });

    // JIT-compile first.
    let p = tb.trigger_jit_cancun(BYTECODE_RET42);
    assert_eq!(p.kind, ProgramKind::Jit);
    assert_eq!(tb.stats().resident_entries, 1);

    // Now request AOT — should be skipped because key is already resident.
    tb.prepare_aot(AotRequest {
        code_hash,
        code: Bytes::copy_from_slice(BYTECODE_RET42),
        spec_id: SpecId::CANCUN,
    });

    // Give backend time to process.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Nothing was persisted — AOT was skipped because the key was already resident as JIT.
    assert_eq!(store.len(), 0, "prepare_aot should skip JIT-resident keys");
    // Still JIT, not replaced.
    let p = tb.get_compiled(code_hash, SpecId::CANCUN).unwrap();
    assert_eq!(p.kind, ProgramKind::Jit);
}

// ===========================================================================
// Tests: cold-entry eviction.
// ===========================================================================

#[test]
#[cfg(feature = "llvm")]
fn cold_entries_are_evicted() {
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 2,
        jit_max_pending_jobs: 1,
        idle_evict_duration: Some(std::time::Duration::from_millis(20)),
        eviction_sweep_interval: std::time::Duration::from_millis(10),
        event_drain_interval: std::time::Duration::from_millis(10),
        ..Default::default()
    });

    for i in 0..10 {
        let _ = tb.lookup(TestBackend::req_cancun(&indexed_bytecode(i)));
    }

    let compile_me = indexed_bytecode(42);
    for _ in 0..2 {
        let _ = tb.lookup(TestBackend::req_cancun(&compile_me));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(tb.stats().resident_entries, 0);

    std::thread::sleep(std::time::Duration::from_millis(100));
    tb.wait_compiled(&compile_me, SpecId::CANCUN);
}

// ===========================================================================
// Tests: compiler recycling.
// ===========================================================================

/// Compile several distinct contracts with `compiler_recycle_threshold: 1` so
/// the worker recreates its LLVM context after every single compilation.
/// All contracts must still compile successfully.
#[test]
#[cfg(feature = "llvm")]
fn compiler_recycle_every_compilation() {
    let n = 5u8;
    let tb = TestBackend::with_tuning_1w(RuntimeTuning {
        jit_hot_threshold: 1,
        compiler_recycle_threshold: 1,
        ..Default::default()
    });

    for i in 0..n {
        let bc = indexed_bytecode(i);
        tb.trigger_jit_cancun(&bc);
    }

    assert_eq!(tb.stats().resident_entries, n as u64);
}
