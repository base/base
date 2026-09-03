//! Sampling CPU profiler that drives `pprof` and renders captures as gzipped `pprof` protobufs.

use std::{io::Write, sync::Arc, time::Duration};

use flate2::{Compression, write::GzEncoder};
use pprof::{ProfilerGuardBuilder, protos::Message};
use tokio::sync::Mutex;
use tracing::info;

const MIN_FREQUENCY_HZ: u32 = 1;
const MAX_FREQUENCY_HZ: u32 = 1_000;

/// Errors returned while capturing a CPU profile.
#[derive(Debug, thiserror::Error)]
pub enum ProfilerError {
    /// Another CPU profile capture is already active.
    #[error("cpu profile capture already in progress")]
    Busy,
    /// Requested duration exceeds the bounded capture window.
    #[error("cpu profile duration {requested:?} exceeds maximum {maximum:?}")]
    DurationTooLong {
        /// Requested capture duration.
        requested: Duration,
        /// Maximum supported capture duration.
        maximum: Duration,
    },
    /// Requested sampling frequency is outside the supported range.
    #[error("cpu profile frequency {frequency} Hz must be between 1 and 1000 Hz")]
    InvalidFrequency {
        /// Rejected sampling frequency.
        frequency: u32,
    },
    /// `pprof` could not start, collect, or build the profile.
    #[error("cpu profiler failed: {0}")]
    Pprof(pprof::Error),
    /// Protobuf serialization failed.
    #[error("failed to encode cpu profile protobuf: {message}")]
    ProtobufEncode {
        /// Protobuf encoder error message.
        message: String,
    },
    /// The blocking profile-finalization task failed to run to completion (panicked or the runtime
    /// shut down).
    #[error("cpu profile finalization task failed: {message}")]
    TaskJoin {
        /// Join error message.
        message: String,
    },
    /// Gzip compression failed.
    #[error("failed to gzip cpu profile protobuf: {0}")]
    Gzip(#[source] std::io::Error),
}

/// On-demand, single-flight CPU profile capture service.
#[derive(Debug, Clone)]
pub struct CpuProfiler {
    capture_lock: Arc<Mutex<()>>,
    max_capture_seconds: u64,
    default_frequency_hz: u32,
}

impl Default for CpuProfiler {
    fn default() -> Self {
        // 100 Hz aliases the 250 ms flashblock and 1000 ms block cadences. Keep the existing prime,
        // non-harmonic 101 Hz default when callers do not provide runtime configuration.
        Self::new(60, 101)
    }
}

impl CpuProfiler {
    /// Creates a profiler with the supplied capture-duration limit and default frequency.
    pub fn new(max_capture_seconds: u64, default_frequency_hz: u32) -> Self {
        Self { capture_lock: Arc::new(Mutex::new(())), max_capture_seconds, default_frequency_hz }
    }

    /// Returns the configured maximum capture duration in seconds.
    pub const fn max_capture_seconds(&self) -> u64 {
        self.max_capture_seconds
    }

    /// Returns the configured sampling frequency used when a request omits one.
    pub const fn default_frequency_hz(&self) -> u32 {
        self.default_frequency_hz
    }

    /// Captures one CPU profile and returns a gzip-wrapped pprof protobuf.
    ///
    /// A missing frequency uses the configured default. Captures above the configured maximum and
    /// frequencies outside 1..=1000 Hz are rejected before starting `pprof`.
    ///
    /// # Cancel safety
    ///
    /// Cancelling during sampling drops the active `pprof` guard and releases the single-flight
    /// lock. Once report finalization starts, the blocking task runs to completion and releases
    /// both resources there.
    ///
    /// # Errors
    ///
    /// Returns [`ProfilerError::Busy`] when another capture is active, validation errors for an
    /// unsupported duration or frequency, or an encoding/capture error from the profiling path.
    pub async fn capture(
        &self,
        duration: Duration,
        frequency: Option<u32>,
    ) -> Result<Vec<u8>, ProfilerError> {
        // The preallocated pprof collector costs 200 MB+ of RSS while its guard is live. Reject
        // longer captures so an unbounded request cannot retain that allocation on a mainnet node.
        let maximum = Duration::from_secs(self.max_capture_seconds);
        if duration > maximum {
            return Err(ProfilerError::DurationTooLong { requested: duration, maximum });
        }

        let hz = frequency.unwrap_or(self.default_frequency_hz);
        if !(MIN_FREQUENCY_HZ..=MAX_FREQUENCY_HZ).contains(&hz) {
            return Err(ProfilerError::InvalidFrequency { frequency: hz });
        }
        let pprof_frequency =
            i32::try_from(hz).map_err(|_| ProfilerError::InvalidFrequency { frequency: hz })?;

        let capture_permit =
            Arc::clone(&self.capture_lock).try_lock_owned().map_err(|_| ProfilerError::Busy)?;
        let guard = ProfilerGuardBuilder::default()
            .frequency(pprof_frequency)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map_err(|error| match error {
                pprof::Error::Running => ProfilerError::Busy,
                other => ProfilerError::Pprof(other),
            })?;
        let secs = duration.as_secs();
        info!(seconds = %secs, frequency = %hz, "cpu profile capture started");

        tokio::time::sleep(duration).await;
        let bytes = tokio::task::spawn_blocking(move || {
            let report = guard.report().build().map_err(ProfilerError::Pprof)?;
            drop(guard);
            drop(capture_permit);

            let profile = report.pprof().map_err(ProfilerError::Pprof)?;
            let protobuf = profile
                .write_to_bytes()
                .map_err(|error| ProfilerError::ProtobufEncode { message: error.to_string() })?;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&protobuf).map_err(ProfilerError::Gzip)?;
            encoder.finish().map_err(ProfilerError::Gzip)
        })
        .await
        .map_err(|error| ProfilerError::TaskJoin { message: error.to_string() })??;

        info!(seconds = %secs, frequency = %hz, bytes = %bytes.len(), "cpu profile capture completed");
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        hint::black_box,
        io::Read,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use flate2::read::GzDecoder;

    use super::*;

    #[test]
    fn capture_rejects_duration_above_configured_maximum() {
        let maximum = Duration::from_secs(7);
        let requested = Duration::from_secs(8);
        let profiler = CpuProfiler::new(maximum.as_secs(), 101);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let result = runtime.block_on(profiler.capture(requested, None));

        assert!(matches!(
            result,
            Err(ProfilerError::DurationTooLong {
                requested: rejected,
                maximum: configured,
            }) if rejected == requested && configured == maximum
        ));
    }

    #[test]
    fn capture_uses_configured_default_frequency_when_omitted() {
        let profiler = CpuProfiler::new(60, 0);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let result = runtime.block_on(profiler.capture(Duration::ZERO, None));

        assert!(matches!(result, Err(ProfilerError::InvalidFrequency { frequency: 0 })));
    }

    #[test]
    fn capture_rejects_u32_frequency_that_pprof_cannot_represent() {
        let profiler = CpuProfiler::new(60, u32::MAX);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let result = runtime.block_on(profiler.capture(Duration::ZERO, None));

        assert!(matches!(
            result,
            Err(ProfilerError::InvalidFrequency { frequency }) if frequency == u32::MAX
        ));
    }

    #[test]
    fn capture_rejects_frequency_outside_supported_range() {
        let profiler = CpuProfiler::default();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        for frequency in [0, 100_000] {
            let result =
                runtime.block_on(profiler.capture(Duration::from_secs(1), Some(frequency)));

            assert!(matches!(
                result,
                Err(ProfilerError::InvalidFrequency { frequency: rejected })
                    if rejected == frequency
            ));
        }
    }

    #[test]
    fn capture_returns_busy_while_another_capture_is_active() {
        let profiler = CpuProfiler::default();
        let first_capture = profiler.capture_lock.try_lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let result = runtime.block_on(profiler.capture(Duration::ZERO, None));

        assert!(matches!(result, Err(ProfilerError::Busy)));
        drop(first_capture);
    }

    #[test]
    fn capture_returns_gzipped_pprof_bytes() {
        let profiler = CpuProfiler::default();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let worker = thread::spawn(move || {
            let mut value = 0_u64;
            while worker_running.load(Ordering::Relaxed) {
                value = black_box(value.wrapping_mul(31).wrapping_add(1));
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();

        let result = runtime.block_on(profiler.capture(Duration::from_secs(1), None));
        running.store(false, Ordering::Relaxed);
        worker.join().unwrap();

        let bytes = result.unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[..2], &[0x1f, 0x8b]);

        let mut decompressed = Vec::new();
        GzDecoder::new(bytes.as_slice()).read_to_end(&mut decompressed).unwrap();
        assert!(!decompressed.is_empty());
        assert!(pprof::protos::Profile::parse_from_bytes(&decompressed).is_ok());
        assert!(bytes.len() < decompressed.len());
    }
}
