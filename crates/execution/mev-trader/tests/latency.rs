//! Independent public staged-accounting integration gate.

use base_mev_trader::{StageLatencyRecorder, StageLatencySample};

#[test]
fn public_staged_accounting_preserves_split_losses_and_drain() {
    let mut recorder = StageLatencyRecorder::default();
    recorder.record_pre_admission_drop().expect("pre-admission drop");
    recorder.record_admission().expect("completed admission");
    recorder
        .record_completion(StageLatencySample {
            discover_ns: 1,
            canonicalize_ns: 2,
            bind_ns: 3,
            two_hop_ns: 4,
            encode_ns: 5,
            end_to_end_ns: 15,
        })
        .expect("atomic completion");
    recorder.record_admission().expect("dropped admission");
    recorder.record_post_admission_drop().expect("post-admission drop");

    let report = recorder.finish().expect("drained report");
    assert_eq!(report.received, 3);
    assert_eq!(report.pre_admission_dropped, 1);
    assert_eq!(report.admitted, 2);
    assert_eq!(report.completed, 1);
    assert_eq!(report.post_admission_dropped, 1);
    assert_eq!(report.dropped, 2);
    assert_eq!(report.truncated, 0);
    assert_eq!(report.in_flight, 0);
    assert_eq!(report.end_to_end.sample_count, 1);
    assert_eq!(report.completed_under50_over_admitted, Some(0.5));
    assert_eq!(report.completed_under50_over_completed, Some(1.0));
    assert!(!report.is_full());
}
