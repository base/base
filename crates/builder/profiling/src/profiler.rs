//! Sampling CPU profiler that drives `pprof` and renders captured reports as either an SVG
//! flamegraph or a gzipped `pprof` protobuf.

use std::{io::Write, sync::Arc, time::Duration};

use flate2::{Compression, write::GzEncoder};
use pprof::{ProfilerGuardBuilder, protos::Message};
use tokio::sync::Mutex;
use tracing::info;

// The preallocated pprof collector costs 200 MB+ of RSS while its guard is live. Reject longer
// captures so an unbounded request cannot retain that allocation on a mainnet node.
const MAX_CAPTURE_DURATION: Duration = Duration::from_secs(60);
const MIN_FREQUENCY_HZ: i32 = 1;
const MAX_FREQUENCY_HZ: i32 = 1_000;
// 100 Hz is exactly harmonic with both the 250 ms flashblock cadence (25 samples/flashblock) and
// the 1000 ms block cadence, which aliases periodic phases and skews profiles. 101 Hz is prime and
// non-harmonic. Re-evaluate this default for the 200 ms Denim cadence.
const DEFAULT_FREQUENCY_HZ: i32 = 101;

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
        frequency: i32,
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
    /// Gzip compression failed.
    #[error("failed to gzip cpu profile protobuf: {0}")]
    Gzip(#[source] std::io::Error),
}

/// On-demand, single-flight CPU profile capture service.
#[derive(Debug, Clone, Default)]
pub struct CpuProfiler {
    capture_lock: Arc<Mutex<()>>,
}

impl CpuProfiler {
    /// Captures one CPU profile and returns a gzip-wrapped pprof protobuf.
    ///
    /// A missing frequency uses 101 Hz. Captures above 60 seconds and frequencies outside
    /// 1..=1000 Hz are rejected before starting `pprof`.
    ///
    /// # Cancel safety
    ///
    /// Cancelling this future drops the active `pprof` guard and releases the single-flight lock.
    ///
    /// # Errors
    ///
    /// Returns [`ProfilerError::Busy`] when another capture is active, validation errors for an
    /// unsupported duration or frequency, or an encoding/capture error from the profiling path.
    pub async fn capture(
        &self,
        duration: Duration,
        frequency: Option<i32>,
    ) -> Result<Vec<u8>, ProfilerError> {
        if duration > MAX_CAPTURE_DURATION {
            return Err(ProfilerError::DurationTooLong {
                requested: duration,
                maximum: MAX_CAPTURE_DURATION,
            });
        }

        let hz = frequency.unwrap_or(DEFAULT_FREQUENCY_HZ);
        if !(MIN_FREQUENCY_HZ..=MAX_FREQUENCY_HZ).contains(&hz) {
            return Err(ProfilerError::InvalidFrequency { frequency: hz });
        }

        let capture_permit = self.capture_lock.try_lock().map_err(|_| ProfilerError::Busy)?;
        let guard = ProfilerGuardBuilder::default()
            .frequency(hz)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map_err(|error| match error {
                pprof::Error::Running => ProfilerError::Busy,
                other => ProfilerError::Pprof(other),
            })?;
        let secs = duration.as_secs();
        info!(seconds = %secs, frequency = %hz, "cpu profile capture started");

        tokio::time::sleep(duration).await;
        let report = guard.report().build().map_err(ProfilerError::Pprof)?;
        drop(guard);
        drop(capture_permit);

        let profile = report.pprof().map_err(ProfilerError::Pprof)?;
        let protobuf = profile
            .write_to_bytes()
            .map_err(|error| ProfilerError::ProtobufEncode { message: error.to_string() })?;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&protobuf).map_err(ProfilerError::Gzip)?;
        let bytes = encoder.finish().map_err(ProfilerError::Gzip)?;

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
    fn capture_rejects_duration_above_maximum() {
        let profiler = CpuProfiler::default();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();

        let result = runtime.block_on(profiler.capture(Duration::from_secs(61), None));

        assert!(matches!(result, Err(ProfilerError::DurationTooLong { .. })));
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
