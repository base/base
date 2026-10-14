//! Periodic samples of the node's tokio runtime.

use std::time::{Duration, Instant};

use tokio::runtime::{Handle, RuntimeMetrics};

base_metrics::define_metrics! {
    base.runtime,
    struct = TokioRuntimeMetrics,
    #[describe("Worker threads in the node's tokio runtime")]
    workers: gauge,
    #[describe("Tasks alive in the node's tokio runtime")]
    alive_tasks: gauge,
    #[describe("Tasks waiting in the runtime's global queue")]
    global_queue_depth: gauge,
    #[describe("Fraction of worker time spent busy over the last sample interval, from 0 to 1")]
    busy_ratio: gauge,
}

/// Records the node's tokio runtime load into [`TokioRuntimeMetrics`] gauges.
///
/// Uses only tokio's stable runtime metrics, so it needs no `tokio_unstable` build. A runtime
/// whose workers stay busy delays every task it schedules, including RPC handlers, which no
/// per-request timer attributes to the runtime.
#[derive(Debug)]
pub struct RuntimeMetricsSampler {
    metrics: RuntimeMetrics,
    busy: Duration,
    sampled_at: Instant,
}

impl RuntimeMetricsSampler {
    /// Time between samples.
    pub const INTERVAL: Duration = Duration::from_secs(10);

    /// Creates a sampler for the runtime behind `handle`.
    pub fn new(handle: &Handle) -> Self {
        let metrics = handle.metrics();
        let busy = Self::total_busy(&metrics);
        Self { metrics, busy, sampled_at: Instant::now() }
    }

    /// Records one sample; the busy ratio covers the time since the previous sample.
    pub fn sample(&mut self) {
        let now = Instant::now();
        let busy = Self::total_busy(&self.metrics);
        let workers = self.metrics.num_workers();
        let capacity = now.duration_since(self.sampled_at).as_secs_f64() * workers as f64;
        if capacity > 0.0 {
            let ratio = busy.saturating_sub(self.busy).as_secs_f64() / capacity;
            TokioRuntimeMetrics::busy_ratio().set(ratio.min(1.0));
        }
        self.busy = busy;
        self.sampled_at = now;
        TokioRuntimeMetrics::workers().set(workers as f64);
        TokioRuntimeMetrics::alive_tasks().set(self.metrics.num_alive_tasks() as f64);
        TokioRuntimeMetrics::global_queue_depth().set(self.metrics.global_queue_depth() as f64);
    }

    /// Samples every [`Self::INTERVAL`] until the task is dropped.
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(Self::INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.sample();
        }
    }

    fn total_busy(metrics: &RuntimeMetrics) -> Duration {
        (0..metrics.num_workers()).map(|worker| metrics.worker_total_busy_duration(worker)).sum()
    }
}

#[cfg(test)]
mod tests {
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };

    use super::*;

    fn gauge(snapshot: &[(CompositeKey, DebugValue)], name: &str) -> f64 {
        snapshot
            .iter()
            .find_map(|(key, value)| match value {
                DebugValue::Gauge(value)
                    if key.kind() == MetricKind::Gauge && key.key().name() == name =>
                {
                    Some(value.into_inner())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("gauge {name} was not recorded"))
    }

    /// A worker kept busy for most of the interval shows up in the busy ratio, alongside the
    /// runtime's shape.
    #[test]
    fn sample_reports_worker_count_and_busy_time() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || {
            let mut sampler = RuntimeMetricsSampler::new(runtime.handle());
            runtime.block_on(async {
                tokio::spawn(async {
                    let end = Instant::now() + Duration::from_millis(100);
                    while Instant::now() < end {
                        std::hint::spin_loop();
                    }
                })
                .await
                .unwrap();
            });
            sampler.sample();
        });

        let snapshot: Vec<_> =
            snapshotter.snapshot().into_vec().into_iter().map(|(k, _, _, v)| (k, v)).collect();
        assert_eq!(gauge(&snapshot, "base.runtime.workers"), 2.0);
        let busy = gauge(&snapshot, "base.runtime.busy_ratio");
        assert!(busy > 0.2 && busy <= 1.0, "one of two workers spun for the interval: {busy}");
    }
}
