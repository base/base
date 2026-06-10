//! Smoke tests for load testing core functionality.

use std::time::Duration;

use alloy_primitives::{Address, TxHash, TxKind, U256};
use base_load_tests::{
    AccountPool, MetricsCollector, Payload, SeededRng, TransactionMetrics, TransferPayload,
    WorkloadConfig, WorkloadGenerator,
};

#[test]
fn deterministic_accounts() {
    let pool1 = AccountPool::new(12345, 5).unwrap();
    let pool2 = AccountPool::new(12345, 5).unwrap();
    assert_eq!(pool1.addresses(), pool2.addresses());
}

#[test]
fn deterministic_accounts_different_seeds() {
    let pool1 = AccountPool::new(111, 5).unwrap();
    let pool2 = AccountPool::new(222, 5).unwrap();
    assert_ne!(pool1.addresses(), pool2.addresses());
}

#[test]
fn transfer_payload_fixed_value() {
    let payload = TransferPayload::fixed(U256::from(1_000_000));
    let mut rng = SeededRng::new(42);

    let tx = payload.generate(&mut rng, Address::ZERO, Address::repeat_byte(1));
    assert_eq!(tx.value, Some(U256::from(1_000_000)));
    assert_eq!(tx.to, Some(TxKind::Call(Address::repeat_byte(1))));
    assert_eq!(tx.gas, Some(21_000));
}

#[test]
fn transfer_payload_random_value() {
    let min = U256::from(100);
    let max = U256::from(1000);
    let payload = TransferPayload::new(min, max);
    let mut rng = SeededRng::new(42);

    for _ in 0..10 {
        let tx = payload.generate(&mut rng, Address::ZERO, Address::repeat_byte(1));
        assert!(tx.value >= Some(min));
        assert!(tx.value <= Some(max));
    }
}

#[test]
fn workload_payload_generation() {
    let config = WorkloadConfig::new("test").with_seed(42);

    let mut generator =
        WorkloadGenerator::new(config).with_payload(TransferPayload::default(), 100.0);

    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);

    let tx = generator.generate_payload(from, to).unwrap();
    assert_eq!(tx.to, Some(TxKind::Call(to)));
}

#[test]
fn workload_deterministic_generation() {
    let config1 = WorkloadConfig::new("test").with_seed(42);
    let mut generator1 =
        WorkloadGenerator::new(config1).with_payload(TransferPayload::default(), 100.0);

    let config2 = WorkloadConfig::new("test").with_seed(42);
    let mut generator2 =
        WorkloadGenerator::new(config2).with_payload(TransferPayload::default(), 100.0);

    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);

    for _ in 0..5 {
        let tx1 = generator1.generate_payload(from, to).unwrap();
        let tx2 = generator2.generate_payload(from, to).unwrap();
        assert_eq!(tx1.to, tx2.to);
        assert_eq!(tx1.value, tx2.value);
    }
}

#[test]
fn metrics_collector_counts() {
    let mut collector = MetricsCollector::new();

    collector.record_submitted(TxHash::ZERO);
    collector.record_submitted(TxHash::repeat_byte(1));
    collector.record_submitted(TxHash::repeat_byte(2));

    collector.record_confirmed(TransactionMetrics::new(
        TxHash::ZERO,
        None,
        None,
        21000,
        1_000_000_000,
        Some(1),
    ));

    collector.record_failed(TxHash::repeat_byte(1), "timeout");

    assert_eq!(collector.submitted_count(), 3);
    assert_eq!(collector.confirmed_count(), 1);
    assert_eq!(collector.failed_count(), 1);
}

#[test]
fn metrics_summary_latency() {
    let mut collector = MetricsCollector::new();

    let latencies_ms = [100, 200, 300, 400, 500];
    for (i, ms) in latencies_ms.iter().enumerate() {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(*ms)),
            Some(Duration::from_millis(*ms / 2)),
            21000,
            1_000_000_000,
            Some(i as u64),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(10), None, None);

    assert_eq!(summary.throughput.total_confirmed, 5);

    let block_latency = &summary.block_latency;
    assert_eq!(block_latency.min, Duration::from_millis(100));
    assert_eq!(block_latency.max, Duration::from_millis(500));
    assert_eq!(block_latency.p50, Duration::from_millis(300));

    let fb_latency = &summary.flashblocks_latency;
    assert_eq!(fb_latency.p50, Duration::from_millis(150));

    let mut metrics = TransactionMetrics::new(
        TxHash::repeat_byte(99),
        Some(Duration::from_millis(600)),
        None,
        21000,
        1_000_000_000,
        Some(99),
    );
    metrics.block_receipt_delay = Some(Duration::from_millis(75));
    collector.record_confirmed(metrics);
    let summary = collector.summarize(Duration::from_secs(10), None, None);
    assert_eq!(summary.block_receipt_delay.p50, Duration::from_millis(75));
}

#[test]
fn metrics_summary_observed_window_split() {
    let mut collector = MetricsCollector::new();

    // 30 txs across blocks 100..=129.
    // configured_duration = 30s
    // → expected_block_count = 30 / 2 = 15
    // → end_block = 100 + 15 - 1 = 114
    // → first half includes blocks 100..=114 (15 txs).
    for i in 0..30u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(100 + i * 10)),
            Some(Duration::from_millis(50 + i * 5)),
            21_000,
            1_000_000_000,
            Some(100 + i),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(60), Some(Duration::from_secs(30)), None);

    let fh = &summary.observed_window;
    assert_eq!(fh.expected_block_count, 15, "30s / 2 = 15 expected blocks");
    assert_eq!(fh.confirmed_count, 15, "blocks 100..=114");
    assert_eq!(fh.block_range.first_block, Some(100));
    assert_eq!(fh.block_range.last_block, Some(114));
    assert_eq!(fh.block_range.block_count, 15);
    // duration = expected_block_count * BLOCK_INTERVAL = 15 blocks × 2s/block = 30s.
    assert_eq!(fh.duration, Duration::from_secs(30), "15 blocks × 2s/block = 30s of L2 time");
    // TPS = 15 txs / 30s = 0.5.
    assert!((fh.tps - 0.5).abs() < 1e-6, "tps={}, expected 0.5", fh.tps);
    // First-half block latencies for txs 0..=14 → 100..=240ms.
    assert_eq!(fh.block_latency.min, Duration::from_millis(100));
    assert_eq!(fh.block_latency.max, Duration::from_millis(240));
    assert_eq!(fh.flashblocks_latency.count, 15);

    // Full-range still spans all 30 blocks.
    assert_eq!(summary.throughput.total_confirmed, 30);
    assert_eq!(summary.block_range.block_count, 30);
}

#[test]
fn metrics_summary_observed_window_inclusion_gap() {
    let mut collector = MetricsCollector::new();

    // 6 txs across blocks 100..=105 with configured_duration=30s.
    // → expected_block_count = 15
    // → end_block = 114
    // → all 6 txs are in the first-half window
    // → inclusion gap = 15 - 6 = 9 blocks (60% gap) — reproduces the user-reported scenario.
    for i in 0..6u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(100)),
            None,
            21_000,
            1_000_000_000,
            Some(100 + i),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(60), Some(Duration::from_secs(30)), None);

    let fh = &summary.observed_window;
    assert_eq!(fh.expected_block_count, 15, "30s / 2 = 15 expected blocks");
    assert_eq!(fh.confirmed_count, 6);
    assert_eq!(fh.block_range.block_count, 6, "only 6 of 15 expected blocks had txs");
}

#[test]
fn metrics_summary_block_receipt_delay_per_window() {
    let mut collector = MetricsCollector::new();

    // 20 txs across blocks 100..=119 with configured_duration = 30s.
    // → expected_block_count = 15, observed_window_end_block = 114.
    // First half = txs in blocks 100..=114 (15 txs), receipt delays 100..=240ms.
    // Tail = txs in blocks 115..=119 (5 txs), receipt delays 250..=290ms.
    for i in 0..20u64 {
        let mut metrics = TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(50)),
            None,
            21_000,
            1_000_000_000,
            Some(100 + i),
        );
        metrics.block_receipt_delay = Some(Duration::from_millis(100 + i * 10));
        collector.record_confirmed(metrics);
    }

    let summary = collector.summarize(Duration::from_secs(60), Some(Duration::from_secs(30)), None);

    let fh_brd = &summary.observed_window.block_receipt_delay;
    assert_eq!(fh_brd.min, Duration::from_millis(100), "first half receipt delay min");
    assert_eq!(fh_brd.max, Duration::from_millis(240), "first half receipt delay max");

    let tail_brd = &summary.tail.as_ref().expect("tail Some").block_receipt_delay;
    assert_eq!(tail_brd.min, Duration::from_millis(250), "tail receipt delay min");
    assert_eq!(tail_brd.max, Duration::from_millis(290), "tail receipt delay max");
}

#[test]
fn metrics_summary_observed_window_empty_when_no_confirms() {
    let collector = MetricsCollector::new();
    let summary = collector.summarize(Duration::from_secs(60), None, None);

    assert_eq!(summary.observed_window.confirmed_count, 0);
    assert_eq!(summary.observed_window.block_range.block_count, 0);
    assert_eq!(summary.observed_window.tps, 0.0);
    assert!(summary.tail.is_none(), "tail should be None when no configured_duration");
}

#[test]
fn metrics_summary_tail_classification() {
    let mut collector = MetricsCollector::new();

    // 30 txs across blocks 100..=129.
    // configured_duration = 30s
    // → observed_window_expected_block_count = 30 / 2 = 15
    // → observed_window_end_block = 100 + 15 - 1 = 114
    // → tail = blocks > 114 → blocks 115..=129 (15 txs).
    for i in 0..30u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(100 + i * 10)),
            Some(Duration::from_millis(50 + i * 5)),
            21_000,
            1_000_000_000,
            Some(100 + i),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(60), Some(Duration::from_secs(30)), None);

    let tail = summary.tail.as_ref().expect("tail should be Some when configured_duration is set");
    assert_eq!(tail.observed_window_end_block, Some(114));
    assert_eq!(tail.count, 15, "blocks 115..=129 should be classified as tail");
    assert_eq!(tail.block_range.first_block, Some(115));
    assert_eq!(tail.block_range.last_block, Some(129));
    assert_eq!(tail.block_range.block_count, 15);
    assert!((tail.confirmed_pct - 50.0).abs() < 1e-6, "15 of 30 = 50%, got {}", tail.confirmed_pct);

    // time_past for tail blocks 115..=129 vs observed_window_end_block=114, at 2s/block:
    // → [2s, 4s, ..., 30s]. min=2s, max=30s, p50 (rank 8 of 15) = 16s.
    let tp = &tail.time_past_observed_window;
    assert_eq!(tp.min, Duration::from_secs(2));
    assert_eq!(tp.max, Duration::from_secs(30));
    assert_eq!(tp.p50, Duration::from_secs(16));

    // Tail block latencies for txs 15..=29 → 250..=390ms.
    assert_eq!(tail.block_latency.min, Duration::from_millis(250));
    assert_eq!(tail.block_latency.max, Duration::from_millis(390));
    assert_eq!(tail.flashblocks_latency.count, 15);
}

#[test]
fn metrics_summary_tail_straggler_after_gap() {
    let mut collector = MetricsCollector::new();

    // Reproduces the user-reported scenario: tight cluster at the start,
    // then a "big block of txs 10 blocks later".
    // configured_duration = 30s → observed_window_expected_block_count = 15
    // → observed_window_end_block = 100 + 14 = 114
    //
    // 6 txs in blocks 100..=105 (clean cluster).
    for i in 0..6u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(100)),
            None,
            21_000,
            1_000_000_000,
            Some(100 + i),
        ));
    }
    // Then 20 txs all in block 125 (10 blocks past observed_window_end_block=114).
    for i in 0..20u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(50 + i as u8),
            Some(Duration::from_millis(8000)),
            None,
            21_000,
            1_000_000_000,
            Some(125),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(60), Some(Duration::from_secs(30)), None);

    let tail = summary.tail.as_ref().expect("tail should be Some");
    assert_eq!(tail.observed_window_end_block, Some(114));
    assert_eq!(tail.count, 20, "the 20 stragglers in block 125 are tail");
    // 20 of 26 ≈ 76.9%.
    let expected_pct = 20.0 / 26.0 * 100.0;
    assert!((tail.confirmed_pct - expected_pct).abs() < 1e-6);
    // All 20 stragglers in block 125 → time_past = (125-114)*2s = 22s for every tx.
    assert_eq!(tail.time_past_observed_window.min, Duration::from_secs(22));
    assert_eq!(tail.time_past_observed_window.max, Duration::from_secs(22));
    assert_eq!(tail.time_past_observed_window.p50, Duration::from_secs(22));
}

#[test]
fn metrics_summary_tail_empty_when_all_inside_observed_window() {
    let mut collector = MetricsCollector::new();

    // 5 txs in blocks 100..=104 with configured_duration = 30s
    // → observed_window_end_block = 100 + 14 = 114
    // → no tail (max block 104 < 114).
    for i in 0..5u64 {
        collector.record_confirmed(TransactionMetrics::new(
            TxHash::repeat_byte(i as u8),
            Some(Duration::from_millis(100)),
            None,
            21_000,
            1_000_000_000,
            Some(100 + i),
        ));
    }

    let summary = collector.summarize(Duration::from_secs(20), Some(Duration::from_secs(30)), None);

    let tail = summary.tail.as_ref().expect("tail should be Some when configured_duration is set");
    assert_eq!(tail.observed_window_end_block, Some(114));
    assert_eq!(tail.count, 0, "no txs landed past the first half");
    assert_eq!(tail.confirmed_pct, 0.0);
    assert_eq!(tail.block_range.block_count, 0);
}

#[test]
fn metrics_summary_gas() {
    let mut collector = MetricsCollector::new();

    collector.record_confirmed(TransactionMetrics::new(
        TxHash::ZERO,
        None,
        None,
        21000,
        1_000_000_000,
        Some(1),
    ));

    collector.record_confirmed(TransactionMetrics::new(
        TxHash::repeat_byte(1),
        None,
        None,
        42000,
        2_000_000_000,
        Some(2),
    ));

    let summary = collector.summarize(Duration::from_secs(10), None, None);

    assert_eq!(summary.gas.total_gas, 63000);
    assert_eq!(summary.gas.avg_gas, 31500);
}

#[test]
fn metrics_summary_json_serialization() {
    let mut collector = MetricsCollector::new();

    collector.record_confirmed(TransactionMetrics::new(
        TxHash::ZERO,
        None,
        None,
        21000,
        1_000_000_000,
        Some(1),
    ));

    let summary = collector.summarize(Duration::from_secs(10), None, None);
    let json = summary.to_json().unwrap();

    assert!(json.contains("block_latency"));
    assert!(json.contains("block_receipt_delay"));
    assert!(json.contains("throughput"));
    assert!(json.contains("gas"));
}

#[test]
fn seeded_rng_deterministic() {
    let mut rng1 = SeededRng::new(42);
    let mut rng2 = SeededRng::new(42);

    let bytes1: [u8; 32] = rng1.gen_bytes();
    let bytes2: [u8; 32] = rng2.gen_bytes();

    assert_eq!(bytes1, bytes2);
}

#[test]
fn seeded_rng_reset() {
    let mut rng = SeededRng::new(42);

    let first: u64 = rng.random();
    let _second: u64 = rng.random();

    rng.reset();

    let after_reset: u64 = rng.random();
    assert_eq!(first, after_reset);
}

#[test]
fn account_pool_random_account() {
    let mut pool = AccountPool::new(42, 10).unwrap();

    let addr1 = pool.random_account().address;
    let addr2 = pool.random_account().address;

    assert!(pool.addresses().contains(&addr1));
    assert!(pool.addresses().contains(&addr2));
}

#[test]
fn funded_account_nonce_increment() {
    let pool = AccountPool::new(42, 1).unwrap();
    let mut account = pool.accounts()[0].clone();

    assert_eq!(account.nonce, 0);
    assert_eq!(account.next_nonce(), 0);
    assert_eq!(account.next_nonce(), 1);
    assert_eq!(account.next_nonce(), 2);
    assert_eq!(account.nonce, 3);
}

#[test]
fn account_pool_with_offset() {
    let pool_no_offset = AccountPool::new(42, 10).unwrap();
    let pool_with_offset = AccountPool::with_offset(42, 5, 5).unwrap();

    let addrs_no_offset = pool_no_offset.addresses();
    let addrs_with_offset = pool_with_offset.addresses();

    assert_eq!(addrs_with_offset.len(), 5);
    assert_eq!(addrs_with_offset[0], addrs_no_offset[5]);
    assert_eq!(addrs_with_offset[4], addrs_no_offset[9]);
}

#[test]
fn account_pool_from_mnemonic() {
    let mnemonic = "test test test test test test test test test test test junk";
    let pool = AccountPool::from_mnemonic(mnemonic, 3, 0).unwrap();

    assert_eq!(pool.len(), 3);

    let pool2 = AccountPool::from_mnemonic(mnemonic, 3, 0).unwrap();
    assert_eq!(pool.addresses(), pool2.addresses());
}

#[test]
fn account_pool_mnemonic_with_offset() {
    let mnemonic = "test test test test test test test test test test test junk";
    let pool_full = AccountPool::from_mnemonic(mnemonic, 10, 0).unwrap();
    let pool_offset = AccountPool::from_mnemonic(mnemonic, 5, 5).unwrap();

    assert_eq!(pool_offset.addresses()[0], pool_full.addresses()[5]);
}
