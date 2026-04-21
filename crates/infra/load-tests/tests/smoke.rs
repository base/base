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

    let summary = collector.summarize(Duration::from_secs(10));

    assert_eq!(summary.throughput.total_confirmed, 5);

    let block_latency = &summary.block_latency;
    assert_eq!(block_latency.min, Duration::from_millis(100));
    assert_eq!(block_latency.max, Duration::from_millis(500));
    assert_eq!(block_latency.p50, Duration::from_millis(300));

    let fb_latency = &summary.flashblocks_latency;
    assert_eq!(fb_latency.p50, Duration::from_millis(150));
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

    let summary = collector.summarize(Duration::from_secs(10));

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

    let summary = collector.summarize(Duration::from_secs(10));
    let json = summary.to_json().unwrap();

    assert!(json.contains("block_latency"));
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
