//! Runtime statistics.

use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic counters for runtime observability.
#[derive(Debug, Default)]
pub(crate) struct RuntimeStats {
    /// Total lookups that returned a compiled function.
    pub(crate) lookup_hits: AtomicU64,
    /// Total lookups that returned interpret (not ready).
    pub(crate) lookup_misses: AtomicU64,
    /// Total lookup events dropped due to event-queue overflow.
    pub(crate) events_dropped: AtomicU64,
    /// Total control commands dropped because the command channel was full.
    pub(crate) commands_dropped: AtomicU64,
    /// Total number of entries evicted (idle + budget).
    pub(crate) evictions: AtomicU64,
    /// Total number of compilations dispatched (JIT promotions + AOT requests).
    pub(crate) compilations_dispatched: AtomicU64,
    /// Total number of successful compilations (JIT + AOT).
    pub(crate) compilations_succeeded: AtomicU64,
    /// Total number of failed compilations (JIT + AOT).
    pub(crate) compilations_failed: AtomicU64,
    /// Total number of out-of-process JIT helper processes spawned successfully.
    pub(crate) jit_helper_spawns: AtomicU64,
    /// Total number of failed out-of-process JIT helper spawn attempts.
    pub(crate) jit_helper_spawn_failures: AtomicU64,
    /// Total number of out-of-process JIT helpers discarded for restart.
    pub(crate) jit_helper_restarts: AtomicU64,
    /// Total number of out-of-process JIT helper job timeouts.
    pub(crate) jit_helper_timeouts: AtomicU64,
    /// Total number of out-of-process JIT helper disconnects.
    pub(crate) jit_helper_disconnects: AtomicU64,
    /// Total number of out-of-process JIT helper pause requests.
    pub(crate) jit_helper_pause_requests: AtomicU64,
    /// Total number of graceful out-of-process JIT helper pause acknowledgements.
    pub(crate) jit_helper_pause_acknowledgements: AtomicU64,
    /// Total number of graceful out-of-process JIT helper pause failures.
    pub(crate) jit_helper_pause_failures: AtomicU64,
    /// Total number of graceful out-of-process JIT helper pause acknowledgement timeouts.
    pub(crate) jit_helper_pause_timeouts: AtomicU64,
    /// Total number of out-of-process JIT helper resume requests.
    pub(crate) jit_helper_resume_requests: AtomicU64,
    /// Total number of out-of-process JIT helper resume request failures.
    pub(crate) jit_helper_resume_failures: AtomicU64,
}

/// Gauge values sampled at snapshot time.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RuntimeStatsGauges {
    pub(crate) resident_entries: u64,
    pub(crate) events_queued: u64,
    pub(crate) command_queue_len: u64,
}

/// A point-in-time snapshot of runtime stats.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeStatsSnapshot {
    /// Total lookups that returned a compiled function.
    pub lookup_hits: u64,
    /// Total lookups that returned interpret (not ready).
    pub lookup_misses: u64,
    /// Total lookup events dropped due to event-queue overflow.
    pub events_dropped: u64,
    /// Total control commands dropped because the command channel was full.
    ///
    /// Pause/resume commands are best-effort and dropped instead of blocking the caller when
    /// the command channel is full.
    pub commands_dropped: u64,
    /// Number of entries in the resident compiled map.
    pub resident_entries: u64,
    /// Number of lookup events currently queued for the backend.
    pub events_queued: u64,
    /// Number of pending control commands queued for the backend.
    pub command_queue_len: u64,
    /// Number of compilation jobs currently in flight (dispatched but not yet completed).
    pub pending_jobs: u64,
    /// Bytes allocated for executable JIT code sections.
    ///
    /// Sourced from the LLVM JIT memory usage plugin. Reflects live memory:
    /// bytes are added on compilation and subtracted when JIT code is freed.
    /// `0` if the LLVM backend is not initialized.
    pub jit_code_bytes: u64,
    /// Bytes allocated for non-executable JIT data sections.
    ///
    /// Sourced from the LLVM JIT memory usage plugin. `0` if not initialized.
    pub jit_data_bytes: u64,
    /// Total number of entries evicted (idle + budget).
    pub evictions: u64,
    /// Total number of compilations dispatched (JIT promotions + AOT requests).
    pub compilations_dispatched: u64,
    /// Total number of successful compilations (JIT + AOT).
    pub compilations_succeeded: u64,
    /// Total number of failed compilations (JIT + AOT).
    pub compilations_failed: u64,
    /// Total number of out-of-process JIT helper processes spawned successfully.
    pub jit_helper_spawns: u64,
    /// Total number of failed out-of-process JIT helper spawn attempts.
    pub jit_helper_spawn_failures: u64,
    /// Total number of out-of-process JIT helpers discarded for restart.
    pub jit_helper_restarts: u64,
    /// Total number of out-of-process JIT helper job timeouts.
    pub jit_helper_timeouts: u64,
    /// Total number of out-of-process JIT helper disconnects.
    pub jit_helper_disconnects: u64,
    /// Total number of out-of-process JIT helper pause requests.
    pub jit_helper_pause_requests: u64,
    /// Total number of graceful out-of-process JIT helper pause acknowledgements.
    pub jit_helper_pause_acknowledgements: u64,
    /// Total number of graceful out-of-process JIT helper pause failures.
    pub jit_helper_pause_failures: u64,
    /// Total number of graceful out-of-process JIT helper pause acknowledgement timeouts.
    pub jit_helper_pause_timeouts: u64,
    /// Total number of out-of-process JIT helper resume requests.
    pub jit_helper_resume_requests: u64,
    /// Total number of out-of-process JIT helper resume request failures.
    pub jit_helper_resume_failures: u64,
}

impl RuntimeStatsSnapshot {
    /// Total bytes allocated by the JIT engine (code + data).
    pub fn jit_total_bytes(&self) -> u64 {
        self.jit_code_bytes + self.jit_data_bytes
    }
}

impl RuntimeStats {
    pub(crate) fn snapshot(&self, gauges: RuntimeStatsGauges) -> RuntimeStatsSnapshot {
        #[cfg(feature = "llvm")]
        let (jit_code_bytes, jit_data_bytes) = crate::llvm::jit_memory_usage()
            .map(|u| (u.code_bytes as u64, u.data_bytes as u64))
            .unwrap_or((0, 0));
        #[cfg(not(feature = "llvm"))]
        let (jit_code_bytes, jit_data_bytes) = (0, 0);

        let dispatched = self.compilations_dispatched.load(Ordering::Relaxed);
        let succeeded = self.compilations_succeeded.load(Ordering::Relaxed);
        let failed = self.compilations_failed.load(Ordering::Relaxed);
        let pending_jobs = dispatched.saturating_sub(succeeded.saturating_add(failed));

        RuntimeStatsSnapshot {
            lookup_hits: self.lookup_hits.load(Ordering::Relaxed),
            lookup_misses: self.lookup_misses.load(Ordering::Relaxed),
            events_dropped: self.events_dropped.load(Ordering::Relaxed),
            commands_dropped: self.commands_dropped.load(Ordering::Relaxed),
            resident_entries: gauges.resident_entries,
            events_queued: gauges.events_queued,
            command_queue_len: gauges.command_queue_len,
            pending_jobs,
            jit_code_bytes,
            jit_data_bytes,
            evictions: self.evictions.load(Ordering::Relaxed),
            compilations_dispatched: dispatched,
            compilations_succeeded: succeeded,
            compilations_failed: failed,
            jit_helper_spawns: self.jit_helper_spawns.load(Ordering::Relaxed),
            jit_helper_spawn_failures: self.jit_helper_spawn_failures.load(Ordering::Relaxed),
            jit_helper_restarts: self.jit_helper_restarts.load(Ordering::Relaxed),
            jit_helper_timeouts: self.jit_helper_timeouts.load(Ordering::Relaxed),
            jit_helper_disconnects: self.jit_helper_disconnects.load(Ordering::Relaxed),
            jit_helper_pause_requests: self.jit_helper_pause_requests.load(Ordering::Relaxed),
            jit_helper_pause_acknowledgements: self
                .jit_helper_pause_acknowledgements
                .load(Ordering::Relaxed),
            jit_helper_pause_failures: self.jit_helper_pause_failures.load(Ordering::Relaxed),
            jit_helper_pause_timeouts: self.jit_helper_pause_timeouts.load(Ordering::Relaxed),
            jit_helper_resume_requests: self.jit_helper_resume_requests.load(Ordering::Relaxed),
            jit_helper_resume_failures: self.jit_helper_resume_failures.load(Ordering::Relaxed),
        }
    }
}
