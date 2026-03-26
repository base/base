//! RAII timer that records elapsed duration to a histogram metric on drop.

/// Call [`.stop()`](Self::stop) to record early; otherwise the duration is
/// recorded when the guard is dropped.
pub struct DropTimer {
    histogram: metrics::Histogram,
    start: std::time::Instant,
    stopped: bool,
}

impl core::fmt::Debug for DropTimer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DropTimer").finish_non_exhaustive()
    }
}

impl DropTimer {
    /// Creates a new timer that starts immediately.
    ///
    /// Prefer the [`timed!`] macro instead of calling this directly.
    #[inline]
    pub fn new(histogram: metrics::Histogram) -> Self {
        Self { histogram, start: std::time::Instant::now(), stopped: false }
    }

    /// Stops the timer, recording the elapsed duration to the histogram.
    ///
    /// Subsequent calls and the drop are no-ops.
    #[inline]
    pub fn stop(&mut self) {
        if !self.stopped {
            self.histogram.record(self.start.elapsed().as_secs_f64());
            self.stopped = true;
        }
    }
}

impl Drop for DropTimer {
    fn drop(&mut self) {
        self.stop();
    }
}
