//! Head-lag sampling between reports.

use base_telemetry_types::Heads;

/// A window of head-lag samples, drained once per report.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LatencyWindow {
    /// Every sample taken in the window, oldest first.
    pub samples: Vec<f64>,
    /// The largest sample seen in the window.
    ///
    /// This is the value only the client can supply. A node that stalls for two minutes and
    /// recovers looks perfectly healthy in every point sample taken by the server.
    pub worst_secs: f64,
}

impl LatencyWindow {
    /// Returns the most recent sample, or zero if the window is empty.
    pub fn latest_secs(&self) -> f64 {
        self.samples.last().copied().unwrap_or_default()
    }

    /// Writes the window into the three latency fields of `heads`.
    ///
    /// The sampler owns those fields; the caller fills in the block numbers. Keeping the
    /// mapping here means the reporting actor and `base telemetry preview` cannot disagree
    /// about which sample becomes `unsafe_latency_secs`.
    pub fn apply(self, heads: &mut Heads) {
        heads.unsafe_latency_secs = self.latest_secs();
        heads.worst_unsafe_latency_secs = self.worst_secs;
        heads.unsafe_latency_samples = self.samples;
    }
}

/// Accumulates head-lag samples and their high-water mark between reports.
///
/// Pure and synchronous. Sampling is how often we look; reporting is how often we send.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencySampler {
    samples: Vec<f64>,
    worst_secs: f64,
    max_samples: usize,
}

impl LatencySampler {
    /// Creates a sampler that carries at most `max_samples` readings per report.
    pub fn new(max_samples: usize) -> Self {
        let max_samples = max_samples.max(1);
        Self { samples: Vec::with_capacity(max_samples), worst_secs: 0.0, max_samples }
    }

    /// Records one head-lag reading, in seconds.
    ///
    /// Negative readings are clamped to zero. A head timestamp slightly ahead of local wall
    /// clock is a clock-skew artifact, not negative lag, and letting it through would drag the
    /// fleet's lag distribution below zero.
    ///
    /// Once the window is full the sample is still folded into the high-water mark but is not
    /// retained, so a misconfigured interval cannot grow the payload without bound.
    pub fn record(&mut self, latency_secs: f64) {
        if !latency_secs.is_finite() {
            return;
        }
        let latency_secs = latency_secs.max(0.0);
        self.worst_secs = self.worst_secs.max(latency_secs);
        if self.samples.len() < self.max_samples {
            self.samples.push(latency_secs);
        }
    }

    /// Takes the accumulated window and resets for the next report.
    pub fn drain(&mut self) -> LatencyWindow {
        let window = LatencyWindow {
            samples: std::mem::take(&mut self.samples),
            worst_secs: self.worst_secs,
        };
        self.worst_secs = 0.0;
        window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worst_tracks_the_high_water_mark() {
        let mut sampler = LatencySampler::new(8);
        for latency in [0.1, 12.5, 0.2, 0.1] {
            sampler.record(latency);
        }

        let window = sampler.drain();
        assert_eq!(window.worst_secs, 12.5, "a stall between samples must survive to the report");
        assert_eq!(window.latest_secs(), 0.1, "the point value is the most recent sample");
        assert_eq!(window.samples.len(), 4);
    }

    #[test]
    fn test_drain_resets_the_window() {
        let mut sampler = LatencySampler::new(8);
        sampler.record(30.0);
        assert_eq!(sampler.drain().worst_secs, 30.0);

        sampler.record(1.0);
        let window = sampler.drain();
        assert_eq!(window.worst_secs, 1.0, "the high-water mark must not leak across reports");
        assert_eq!(window.samples, vec![1.0]);
    }

    #[test]
    fn test_empty_window_reports_zero() {
        let window = LatencySampler::new(8).drain();
        assert_eq!(window.worst_secs, 0.0);
        assert_eq!(window.latest_secs(), 0.0);
        assert!(window.samples.is_empty());
    }

    #[test]
    fn test_samples_are_capped_but_the_worst_is_not() {
        let mut sampler = LatencySampler::new(2);
        sampler.record(1.0);
        sampler.record(2.0);
        sampler.record(99.0);

        let window = sampler.drain();
        assert_eq!(window.samples, vec![1.0, 2.0], "the payload must stay bounded");
        assert_eq!(window.worst_secs, 99.0, "a capped sample still counts toward the worst case");
    }

    #[test]
    fn test_clock_skew_does_not_produce_negative_lag() {
        let mut sampler = LatencySampler::new(4);
        sampler.record(-3.0);
        sampler.record(f64::NAN);

        let window = sampler.drain();
        assert_eq!(window.samples, vec![0.0], "skew clamps to zero and NaN is discarded");
        assert_eq!(window.worst_secs, 0.0);
    }
}
