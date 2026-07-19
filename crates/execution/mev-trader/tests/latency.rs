//! Offline release latency accounting gate.
use std::{hint::black_box, time::Instant};

use base_mev_trader::{LATENCY_TIMED_RUNS, LATENCY_WARMUP_RUNS, LatencyRecorder};

fn deterministic_fixture_work(iteration: usize) -> u64 {
    (0..256u64)
        .fold(iteration as u64, |value, lane| black_box(value.wrapping_mul(17).wrapping_add(lane)))
}

#[test]
#[ignore = "run as the offline release latency gate"]
fn release_fixture_uses_ten_warmups_one_hundred_samples_and_drains() {
    for iteration in 0..LATENCY_WARMUP_RUNS {
        black_box(deterministic_fixture_work(iteration));
    }

    let mut recorder = LatencyRecorder::default();
    for iteration in 0..LATENCY_TIMED_RUNS {
        recorder.record_admission().expect("admission");
        let started = Instant::now();
        black_box(deterministic_fixture_work(iteration));
        let latency_ns = u64::try_from(started.elapsed().as_nanos()).expect("bounded latency");
        recorder.record_completion(latency_ns).expect("completion");
    }

    let report = recorder.finish().expect("terminal report");
    assert_eq!(report.admitted, LATENCY_TIMED_RUNS as u64);
    assert_eq!(report.completed, report.admitted);
    assert_eq!(report.dropped, 0);
    assert_eq!(report.truncated, 0);
    assert_eq!(report.in_flight, 0);
    assert_eq!(report.completed_under50_over_admitted, 1.0);
    assert_eq!(report.completed_under50_over_completed, 1.0);
    assert!(report.is_full(), "exclusive p95 gate failed: {report:?}");
}
