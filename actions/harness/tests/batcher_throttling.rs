#![doc = "Action tests for the batcher throttle controller."]

use base_action_harness::{
    ActionL2Source, ActionTestHarness, BatcherConfig, L1MinerConfig, SharedL1Chain,
};
use base_batcher_core::{ThrottleConfig, ThrottleController, ThrottleStrategy};
use base_consensus_registry::Registry;

/// When the batcher accumulates frames without submitting them, the total
/// encoded bytes should exceed a configured threshold and activate
/// throttling. This test builds real L2 blocks, encodes them into frames,
/// and verifies that `ThrottleController::update` fires.
#[tokio::test]
async fn test_throttle_activates_when_frames_accumulate() {
    let l1_cfg = L1MinerConfig { block_time: 2 };
    let batcher_cfg = BatcherConfig::default();
    let rollup_cfg = {
        let mut rc = Registry::rollup_config(8453).unwrap().clone();
        rc.batch_inbox_address = batcher_cfg.inbox_address;
        rc.genesis.system_config.as_mut().unwrap().batcher_address = batcher_cfg.batcher_address;
        rc.genesis.l2_time = 0;
        rc.genesis.l1 = Default::default();
        rc.genesis.l2 = Default::default();
        rc.hardforks.canyon_time = Some(0);
        rc.hardforks.delta_time = Some(0);
        rc.hardforks.ecotone_time = Some(0);
        rc.hardforks.fjord_time = Some(0);
        rc
    };
    let mut h = ActionTestHarness::new(l1_cfg, rollup_cfg.clone());
    let l1_chain = SharedL1Chain::from_blocks(h.l1.chain().to_vec());
    let mut sequencer = h.create_l2_sequencer(l1_chain);

    // Build 3 L2 blocks.
    let mut source = ActionL2Source::new();
    for _ in 0..3 {
        let block = sequencer.build_next_block().unwrap();
        source.push(block);
    }

    // Encode frames but do NOT submit them — simulates unsubmitted DA backlog.
    let mut batcher = h.create_batcher(source, batcher_cfg.clone());
    let frames = batcher.encode_frames().unwrap();

    // Compute total encoded bytes as a proxy for DA backlog.
    let encoded_bytes: u64 = frames.iter().map(|f| f.encode().len() as u64).sum();

    // Configure throttle with a low threshold so the test-sized data triggers it.
    let config = ThrottleConfig {
        threshold_bytes: encoded_bytes / 2, // threshold below what we encoded
        ..ThrottleConfig::default()
    };
    let ctrl = ThrottleController::new(config, ThrottleStrategy::Linear);

    let params = ctrl.update(encoded_bytes);
    assert!(params.is_some(), "throttle should activate when backlog exceeds threshold");
    let params = params.unwrap();
    assert!(params.is_throttling(), "intensity should be > 0");
    assert!(params.max_block_size < ThrottleConfig::default().block_size_upper_limit);
    assert!(params.max_tx_size < ThrottleConfig::default().tx_size_upper_limit);
}

/// When the DA backlog drops to zero the throttle controller should return
/// `None`, indicating that no throttling is needed.
#[test]
fn test_throttle_deactivates_when_backlog_clears() {
    let config = ThrottleConfig { threshold_bytes: 1_000, ..ThrottleConfig::default() };
    let ctrl = ThrottleController::new(config, ThrottleStrategy::Linear);

    // High backlog: throttle active.
    assert!(ctrl.update(5_000).is_some(), "throttle should be active above threshold");

    // Zero backlog: throttle deactivated.
    assert!(ctrl.update(0).is_none(), "throttle should be inactive when backlog clears");
}

/// Verify that the linear strategy produces the expected intensity and DA
/// size limits for a known backlog midpoint.
#[test]
fn test_throttle_params_follow_linear_formula() {
    let config = ThrottleConfig {
        threshold_bytes: 1_000,
        max_intensity: 1.0,
        block_size_lower_limit: 0,
        block_size_upper_limit: 100_000,
        tx_size_lower_limit: 0,
        tx_size_upper_limit: 10_000,
    };
    let ctrl = ThrottleController::new(config, ThrottleStrategy::Linear);

    // 1500 bytes = midpoint between threshold (1000) and 2x threshold (2000)
    // intensity = 0.5, max_block_size = 50_000, max_tx_size = 5_000
    let params = ctrl.update(1_500).unwrap();
    assert!(
        (params.intensity - 0.5).abs() < 0.01,
        "intensity should be ~0.5, got {}",
        params.intensity,
    );
    assert!(
        (params.max_block_size as i64 - 50_000).abs() <= 1,
        "max_block_size should be ~50_000, got {}",
        params.max_block_size,
    );
    assert!(
        (params.max_tx_size as i64 - 5_000).abs() <= 1,
        "max_tx_size should be ~5_000, got {}",
        params.max_tx_size,
    );
}

/// The step strategy should jump straight to full intensity (and the
/// corresponding lower limits) for any backlog above the threshold.
#[test]
fn test_throttle_step_strategy_full_intensity() {
    let ctrl = ThrottleController::new(ThrottleConfig::default(), ThrottleStrategy::Step);

    // Any backlog above threshold -> full intensity -> lower limits applied.
    let params = ctrl.update(ThrottleConfig::default().threshold_bytes + 1).unwrap();
    assert_eq!(params.intensity, 1.0);
    assert_eq!(params.max_block_size, 2_000, "step at full intensity: block lower limit");
    assert_eq!(params.max_tx_size, 150, "step at full intensity: tx lower limit");
}
