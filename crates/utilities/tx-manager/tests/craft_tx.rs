//! Integration tests for [`SimpleTxManager`] transaction construction with Anvil.

mod common;

use std::sync::Arc;

use alloy_consensus::{TxEip1559, TxEip4844Variant, TxEnvelope};
use alloy_eips::{eip4844::Blob, eip7594::CELLS_PER_EXT_BLOB};
use alloy_node_bindings::Anvil;
use alloy_primitives::{Bytes, TxKind, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{
    FeeCalculator, FeeOverride, NoopTxMetrics, PreparedTx, SignerConfig, SimpleTxManager,
    TxCandidate, TxManager, TxManagerConfig, TxManagerError,
};
use common::{
    TEST_RECIPIENT, setup_anvil, setup_with_config, setup_with_failing_signer, value_transfer,
};

async fn setup() -> (SimpleTxManager<RootProvider>, alloy_node_bindings::AnvilInstance) {
    setup_with_config(TxManagerConfig::default()).await
}

fn decode_eip1559(prepared: &PreparedTx) -> TxEip1559 {
    match prepared.to_envelope().expect("should decode as valid TxEnvelope") {
        TxEnvelope::Eip1559(signed) => signed.strip_signature(),
        other => panic!("expected EIP-1559, got {other:?}"),
    }
}

fn single_blob_candidate() -> TxCandidate {
    TxCandidate {
        to: Some(TEST_RECIPIENT),
        blobs: Arc::from(vec![Box::<Blob>::default()]),
        ..Default::default()
    }
}

#[tokio::test]
async fn craft_tx_produces_valid_signed_eip1559_transaction() {
    let (manager, anvil) = setup().await;

    let candidate = value_transfer(1_000_000_000);

    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let tx = decode_eip1559(&prepared);

    assert_eq!(tx.to, TxKind::Call(TEST_RECIPIENT));
    assert_eq!(tx.value, U256::from(1_000_000_000u64));
    assert_eq!(tx.chain_id, anvil.chain_id());
    assert_eq!(tx.nonce, 0, "first tx should have nonce 0");
    assert_eq!(tx.gas_limit, 21_000, "plain value transfer intrinsic gas should be 21,000");
    assert!(tx.max_fee_per_gas > 0, "max_fee_per_gas should be non-zero");
    assert!(tx.max_priority_fee_per_gas > 0, "max_priority_fee_per_gas should be non-zero");
    assert!(prepared.gas_tip_cap > 0, "PreparedTx gas_tip_cap should be non-zero");
    assert!(
        prepared.gas_fee_cap > prepared.gas_tip_cap,
        "PreparedTx gas_fee_cap should exceed gas_tip_cap",
    );
    assert_eq!(
        prepared.gas_tip_cap, tx.max_priority_fee_per_gas,
        "PreparedTx gas_tip_cap should match decoded max_priority_fee_per_gas",
    );
    assert_eq!(
        prepared.gas_fee_cap, tx.max_fee_per_gas,
        "PreparedTx gas_fee_cap should match decoded max_fee_per_gas",
    );
}

#[tokio::test]
async fn craft_tx_with_explicit_gas_limit_above_estimate() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate { gas_limit: 100_000, ..value_transfer(1_000) };

    let prepared =
        manager.craft_tx(&candidate, None).await.expect("should craft tx with explicit gas");
    let tx = decode_eip1559(&prepared);

    assert_eq!(tx.gas_limit, 100_000);
}

#[tokio::test]
async fn craft_tx_rejects_too_many_blobs() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(TEST_RECIPIENT),
        blobs: Arc::from((0..7).map(|_| Box::<Blob>::default()).collect::<Vec<_>>()),
        ..Default::default()
    };

    let err = manager.craft_tx(&candidate, None).await.expect_err("should reject too many blobs");
    assert!(
        matches!(&err, TxManagerError::Unsupported(msg) if msg.contains("exceeds maximum")),
        "expected TxManagerError::Unsupported with 'exceeds maximum', got {err:?}",
    );
}

#[tokio::test]
async fn craft_tx_rejects_blob_without_recipient() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: None,
        blobs: Arc::from(vec![Box::<Blob>::default()]),
        ..Default::default()
    };

    let err =
        manager.craft_tx(&candidate, None).await.expect_err("should reject blob tx without to");
    assert!(
        matches!(&err, TxManagerError::Unsupported(msg) if msg.contains("recipient address")),
        "expected TxManagerError::Unsupported with 'recipient address', got {err:?}",
    );
}

#[tokio::test]
async fn craft_tx_produces_valid_signed_blob_transaction() {
    // This test also implicitly verifies that `estimate_gas` succeeds when
    // the sidecar is stripped but blob versioned hashes remain on the
    // TransactionRequest (the sidecar-strip path in craft_tx_with_caps
    // Step 5). If the node rejected hashes-without-sidecar, this test
    // would fail at craft_tx.
    let (manager, anvil) = setup().await;

    let candidate = single_blob_candidate();

    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft blob tx");
    let envelope = prepared.to_envelope().expect("should decode TxEnvelope");
    assert!(envelope.is_eip4844(), "expected EIP-4844 transaction, got {envelope:?}");

    let signed = envelope.as_eip4844().expect("should be EIP-4844");
    let variant = signed.tx();
    let inner = variant.tx();
    assert_eq!(inner.to, TEST_RECIPIENT, "recipient should match");
    assert_eq!(inner.chain_id, anvil.chain_id(), "chain_id should match");
    assert!(!inner.blob_versioned_hashes.is_empty(), "blob_versioned_hashes should be populated");
    assert_eq!(inner.blob_versioned_hashes.len(), 1, "should have one versioned hash for one blob",);
    assert!(inner.max_fee_per_blob_gas > 0, "max_fee_per_blob_gas should be non-zero");
    for hash in &inner.blob_versioned_hashes {
        assert_eq!(hash.0[0], 0x01, "versioned hash should start with 0x01, got: {hash}");
    }
    assert!(
        matches!(variant, TxEip4844Variant::TxEip4844WithSidecar(_)),
        "expected TxEip4844WithSidecar variant, got standalone TxEip4844",
    );
    assert!(
        prepared.blob_fee_cap.is_some_and(|v| v > 0),
        "PreparedTx blob_fee_cap should be Some and non-zero",
    );
    assert_eq!(
        prepared.blob_fee_cap.unwrap(),
        inner.max_fee_per_blob_gas,
        "PreparedTx blob_fee_cap should match decoded max_fee_per_blob_gas",
    );
}

#[tokio::test]
async fn craft_tx_produces_cell_proof_sidecar_by_default() {
    let (manager, _anvil) = setup().await;

    let candidate = single_blob_candidate();

    let prepared =
        manager.craft_tx(&candidate, None).await.expect("should craft cell-proof blob tx");

    assert_eq!(
        prepared.sidecar.as_ref().expect("sidecar should be Some for blob tx").cell_proofs.len(),
        CELLS_PER_EXT_BLOB,
    );

    // Cell proofs are sidecar-internal; on the wire it is still EIP-4844 type.
    let envelope = prepared.to_envelope().expect("should decode TxEnvelope");
    assert!(envelope.is_eip4844(), "expected EIP-4844 transaction type");
}

#[tokio::test]
async fn craft_tx_contract_creation() {
    let (manager, _anvil) = setup().await;

    // Minimal valid contract bytecode (STOP opcode).
    let candidate = TxCandidate {
        to: None,
        tx_data: Bytes::from_static(&[0x00]),
        gas_limit: 0,
        ..Default::default()
    };

    let prepared =
        manager.craft_tx(&candidate, None).await.expect("should craft contract creation tx");
    let tx = decode_eip1559(&prepared);

    assert_eq!(tx.to, TxKind::Create);
}

#[tokio::test]
async fn suggest_gas_price_caps_returns_valid_estimates() {
    let (manager, _anvil) = setup().await;

    let caps = manager.suggest_gas_price_caps().await.expect("should return gas price caps");

    assert!(caps.gas_tip_cap > 0, "tip_cap should be non-zero");
    assert!(caps.gas_fee_cap > caps.gas_tip_cap, "fee_cap should exceed tip_cap");
    assert!(caps.raw_gas_fee_cap > 0, "raw_gas_fee_cap should be non-zero");
    assert!(
        caps.raw_gas_fee_cap <= caps.gas_fee_cap,
        "raw_gas_fee_cap should be <= gas_fee_cap after enforcing minimums",
    );
    assert!(caps.blob_fee_cap.is_none(), "blob_fee_cap should be None");
}

#[tokio::test]
async fn prepare_produces_valid_signed_transaction() {
    let (manager, _anvil) = setup().await;

    let candidate = value_transfer(1_000);

    let prepared = manager.prepare(&candidate, None).await.expect("should prepare tx");
    let tx = decode_eip1559(&prepared);

    assert_eq!(tx.to, TxKind::Call(TEST_RECIPIENT));
    assert_eq!(tx.value, U256::from(1_000u64));
}

#[tokio::test]
async fn prepare_returns_channel_closed_when_manager_is_closed() {
    let (manager, _anvil) = setup().await;

    manager.close();

    let candidate = value_transfer(0);

    let err = manager.prepare(&candidate, None).await.expect_err("should fail");
    assert_eq!(err, TxManagerError::ChannelClosed);
}

#[tokio::test]
async fn sender_address_matches_wallet() {
    let (manager, anvil) = setup().await;

    let expected_address = anvil.addresses()[0];
    assert_eq!(manager.sender_address(), expected_address);
}

#[tokio::test]
async fn is_closed_reflects_manager_state() {
    let (manager, _anvil) = setup().await;

    assert!(!manager.is_closed());
    manager.close();
    assert!(manager.is_closed());
}

#[tokio::test]
async fn sequential_craft_tx_increments_nonce() {
    let (manager, _anvil) = setup().await;

    let candidate = value_transfer(1);

    let prepared1 = manager.craft_tx(&candidate, None).await.expect("first tx");
    let prepared2 = manager.craft_tx(&candidate, None).await.expect("second tx");

    assert_eq!(decode_eip1559(&prepared1).nonce, 0);
    assert_eq!(decode_eip1559(&prepared2).nonce, 1);
}

/// Verifies that [`SimpleTxManager::new`] with a [`SignerConfig::Local`]
/// successfully builds the wallet and creates a functional manager.
#[tokio::test]
async fn new_with_signer_config_local_creates_functional_manager() {
    let anvil = Anvil::new().spawn();
    let provider = RootProvider::new_http(anvil.endpoint_url());
    let private_key = anvil.keys()[0].clone();
    let signer = PrivateKeySigner::from_slice(&private_key.to_bytes()).expect("valid anvil key");
    let signer_config = SignerConfig::local(signer);
    let manager = SimpleTxManager::new(
        provider,
        signer_config,
        TxManagerConfig::default(),
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect("should create manager via SignerConfig::Local");

    assert_eq!(manager.sender_address(), anvil.addresses()[0]);
}

#[tokio::test]
async fn new_rejects_chain_id_mismatch() {
    let (provider, wallet, _anvil) = setup_anvil();

    let err = SimpleTxManager::from_wallet(
        provider,
        wallet,
        TxManagerConfig::default(),
        999, // wrong chain_id
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect_err("should reject mismatched chain_id");

    assert!(
        matches!(&err, TxManagerError::InvalidConfig(msg) if msg.contains("chain_id mismatch")),
        "expected TxManagerError::InvalidConfig with 'chain_id mismatch', got {err:?}",
    );
}

#[tokio::test]
async fn craft_tx_preserves_calldata() {
    let (manager, _anvil) = setup().await;

    let calldata = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
    let candidate = TxCandidate {
        to: Some(TEST_RECIPIENT),
        tx_data: calldata.clone(),
        value: U256::ZERO,
        gas_limit: 0,
        ..Default::default()
    };

    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx with calldata");
    let tx = decode_eip1559(&prepared);

    assert_eq!(tx.input, calldata, "calldata should be preserved in the decoded transaction");
}

#[tokio::test]
async fn suggest_gas_price_caps_enforces_min_tip_cap_and_min_basefee() {
    // Minimums well above Anvil defaults (~1 gwei) to exercise the .max() path.
    let high_min_tip = 50_000_000_000u128;
    let high_min_basefee = 100_000_000_000u128;
    let config = TxManagerConfig {
        min_tip_cap: high_min_tip,
        min_basefee: high_min_basefee,
        ..TxManagerConfig::default()
    };

    let (manager, _anvil) = setup_with_config(config).await;

    let caps = manager.suggest_gas_price_caps().await.expect("should return caps");

    assert!(
        caps.gas_tip_cap >= high_min_tip,
        "gas_tip_cap {} should be >= min_tip_cap {high_min_tip}",
        caps.gas_tip_cap,
    );
    // gas_fee_cap = tip + 2 * base_fee
    let expected_min_fee_cap = high_min_tip + 2 * high_min_basefee;
    assert!(
        caps.gas_fee_cap >= expected_min_fee_cap,
        "gas_fee_cap {} should be >= {expected_min_fee_cap} (min_tip + 2 * min_basefee)",
        caps.gas_fee_cap,
    );
    assert!(
        caps.raw_gas_fee_cap < caps.gas_fee_cap,
        "raw_gas_fee_cap {} should be < gas_fee_cap {} when minimums are enforced",
        caps.raw_gas_fee_cap,
        caps.gas_fee_cap,
    );
}

#[tokio::test]
async fn new_rejects_invalid_config() {
    let (provider, wallet, anvil) = setup_anvil();

    let err = SimpleTxManager::from_wallet(
        provider,
        wallet,
        TxManagerConfig { num_confirmations: 0, ..TxManagerConfig::default() },
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect_err("should reject invalid config");

    assert!(
        matches!(&err, TxManagerError::InvalidConfig(msg) if msg.contains("num_confirmations")),
        "expected TxManagerError::InvalidConfig with 'num_confirmations', got {err:?}",
    );
}

#[tokio::test]
async fn craft_tx_returns_fee_limit_exceeded_when_minimums_inflate_beyond_multiplier() {
    // fee_limit_multiplier = 1 means the enforced fee cap must not exceed
    // 1 × raw_gas_fee_cap.  Setting min_tip_cap and min_basefee far above
    // Anvil's actual values (~1 gwei each) inflates gas_fee_cap well past
    // that ceiling, triggering FeeLimitExceeded.
    let config = TxManagerConfig {
        min_tip_cap: 500_000_000_000, // 500 gwei
        min_basefee: 500_000_000_000, // 500 gwei
        fee_limit_multiplier: 1,
        fee_limit_threshold: 0, // always enforce the limit
        ..TxManagerConfig::default()
    };

    let (manager, _anvil) = setup_with_config(config).await;

    let candidate = value_transfer(1);

    let err = manager.craft_tx(&candidate, None).await.expect_err("should exceed fee limit");
    assert!(
        matches!(err, TxManagerError::FeeLimitExceeded { .. }),
        "expected TxManagerError::FeeLimitExceeded, got {err:?}",
    );
}

/// Verifies that `prepare()` exits immediately on a non-retryable error
/// rather than looping through all retry attempts.
#[tokio::test]
async fn prepare_exits_immediately_on_non_retryable_error() {
    let (manager, _anvil) = setup().await;

    // Blob transactions without a recipient trigger Unsupported (non-retryable).
    let candidate = TxCandidate {
        to: None,
        blobs: Arc::from(vec![Box::<Blob>::default()]),
        ..Default::default()
    };

    let err = manager
        .prepare(&candidate, None)
        .await
        .expect_err("should reject blob tx without recipient");

    assert!(
        matches!(err, TxManagerError::Unsupported(_)),
        "expected TxManagerError::Unsupported, got {err:?}",
    );
}

#[tokio::test]
async fn accessors_return_expected_values() {
    let (manager, anvil) = setup().await;

    assert_eq!(manager.chain_id(), anvil.chain_id());
    assert_eq!(manager.config().num_confirmations, TxManagerConfig::default().num_confirmations);
    let _ = manager.provider();
    let _ = manager.wallet();
    let _ = manager.nonce_manager();
}

/// Verifies that when signing fails, the nonce is rolled back and the
/// next `next_nonce()` call on the same manager reuses the same value.
///
/// This exercises the `guard.rollback()` path in step 6 of `craft_tx`.
/// By querying the *same* `NonceManager` instance after the failure, we
/// confirm rollback restored the nonce — if `rollback()` were removed,
/// the nonce would advance to 1.
#[tokio::test]
async fn craft_tx_rolls_back_nonce_on_sign_failure() {
    let (failing_manager, _anvil) = setup_with_failing_signer(TxManagerConfig::default()).await;

    let candidate = value_transfer(1);

    let err = failing_manager.craft_tx(&candidate, None).await.expect_err("should fail to sign");
    assert!(matches!(err, TxManagerError::Sign(_)), "expected TxManagerError::Sign, got {err:?}",);

    let guard = failing_manager.nonce_manager().next_nonce().await.expect("should reserve nonce");
    assert_eq!(guard.nonce(), 0, "nonce should be 0 — rollback must have restored it");
}

/// When fee overrides are above network fees, the [`PreparedTx`] must use the
/// overrides (since `craft_tx` takes `max(network_fee, override)`).
#[tokio::test]
async fn craft_tx_with_fee_overrides_uses_overrides_when_above_network() {
    let (manager, _anvil) = setup().await;

    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");
    let override_tip = caps.gas_tip_cap + 50_000_000_000;
    let override_fee_cap = caps.gas_fee_cap + 100_000_000_000;

    let candidate = value_transfer(1_000);

    let overrides = FeeOverride::new(override_tip, override_fee_cap);
    let prepared = manager
        .craft_tx(&candidate, Some(overrides))
        .await
        .expect("should craft tx with fee overrides");

    assert_eq!(prepared.gas_tip_cap, override_tip);
    assert_eq!(prepared.gas_fee_cap, override_fee_cap);

    let tx = decode_eip1559(&prepared);
    assert_eq!(tx.max_priority_fee_per_gas, override_tip);
    assert_eq!(tx.max_fee_per_gas, override_fee_cap);
}

/// When fee overrides are below network fees, the [`PreparedTx`] must use the
/// network fees (since `craft_tx` takes `max(network_fee, override)`).
#[tokio::test]
async fn craft_tx_with_fee_overrides_uses_network_when_overrides_below() {
    let (manager, _anvil) = setup().await;

    let candidate = value_transfer(1_000);

    // 1 wei overrides are far below any real network fee.
    let overrides = FeeOverride::new(1, 1);
    let prepared = manager
        .craft_tx(&candidate, Some(overrides))
        .await
        .expect("should craft tx with low fee overrides");

    assert!(prepared.gas_tip_cap > 1, "network tip should override the 1-wei override");
    assert!(prepared.gas_fee_cap > 1, "network fee cap should override the 1-wei override");
}

#[test]
fn simple_tx_manager_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimpleTxManager<RootProvider>>();
}

// ── increase_gas_price ──────────────────────────────────────────────

#[tokio::test]
async fn increase_gas_price_bumps_above_threshold() {
    let (manager, _anvil) = setup().await;

    let candidate = value_transfer(1_000);

    // Use the initial on-wire fees as the "old" values.
    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft initial tx");
    let initial_tip = prepared.gas_tip_cap;
    let initial_fee_cap = prepared.gas_fee_cap;

    let bumped = manager
        .increase_gas_price(&candidate, initial_tip, initial_fee_cap, None)
        .await
        .expect("should compute bumped fees");

    // Bumped values must satisfy the 10% threshold for non-blob txs.
    let threshold_tip = FeeCalculator::calc_threshold_value(initial_tip, false);
    let threshold_fee_cap = FeeCalculator::calc_threshold_value(initial_fee_cap, false);
    assert!(
        bumped.gas_tip_cap >= threshold_tip,
        "bumped tip {} should be >= threshold {}",
        bumped.gas_tip_cap,
        threshold_tip,
    );
    assert!(
        bumped.gas_fee_cap >= threshold_fee_cap,
        "bumped fee_cap {} should be >= threshold {}",
        bumped.gas_fee_cap,
        threshold_fee_cap,
    );
    assert!(bumped.blob_fee_cap.is_none(), "blob_fee_cap should be None for non-blob tx");
}

#[tokio::test]
async fn increase_gas_price_fee_limit_exceeded() {
    let config = TxManagerConfig {
        fee_limit_multiplier: 1,
        fee_limit_threshold: 0,
        ..TxManagerConfig::default()
    };

    let (manager, _anvil) = setup_with_config(config).await;

    let candidate = value_transfer(1_000);

    // Use high old fees that will bump above the ceiling. The 10% bump
    // on a fee_cap of 500 gwei will produce ~550 gwei, which exceeds
    // `1 × raw_gas_fee_cap` (Anvil's ~3 gwei).
    let old_tip = 500_000_000_000u128; // 500 gwei
    let old_fee_cap = 500_000_000_000u128; // 500 gwei

    let err = manager
        .increase_gas_price(&candidate, old_tip, old_fee_cap, None)
        .await
        .expect_err("should exceed fee limit");

    assert!(
        matches!(err, TxManagerError::FeeLimitExceeded { .. }),
        "expected TxManagerError::FeeLimitExceeded, got {err:?}",
    );
}

#[tokio::test]
async fn increase_gas_price_bumps_blob_fee_cap() {
    let (manager, _anvil) = setup().await;

    let candidate = single_blob_candidate();

    // Use craft_tx to get valid initial fees for a blob transaction.
    let initial = manager.craft_tx(&candidate, None).await.expect("should craft initial blob tx");
    let old_blob_fee_cap = initial.blob_fee_cap.expect("blob tx should have blob_fee_cap");

    let bumped = manager
        .increase_gas_price(
            &candidate,
            initial.gas_tip_cap,
            initial.gas_fee_cap,
            Some(old_blob_fee_cap),
        )
        .await
        .expect("should compute bumped fees with blob fee cap");

    assert!(
        bumped.blob_fee_cap.is_some(),
        "blob_fee_cap should be Some when old_blob_fee_cap is Some"
    );

    let threshold = FeeCalculator::calc_threshold_value(old_blob_fee_cap, true);
    assert!(
        bumped.blob_fee_cap.unwrap() >= threshold,
        "bumped blob_fee_cap {} should be >= 100% threshold {}",
        bumped.blob_fee_cap.unwrap(),
        threshold,
    );
}

#[tokio::test]
async fn increase_gas_price_rejects_blob_fee_cap_mismatch() {
    let (manager, _anvil) = setup().await;

    // Non-blob candidate with old_blob_fee_cap = Some should be rejected.
    let non_blob_candidate = value_transfer(0);

    let err = manager
        .increase_gas_price(&non_blob_candidate, 1_000, 2_000, Some(500))
        .await
        .expect_err("should reject blob fee cap on non-blob tx");

    assert!(
        matches!(err, TxManagerError::Unsupported(_)),
        "expected TxManagerError::Unsupported, got {err:?}",
    );
}

#[tokio::test]
async fn blob_fee_bump_round_trip() {
    let (manager, _anvil) = setup().await;

    let candidate = single_blob_candidate();

    let initial = manager.craft_tx(&candidate, None).await.expect("should craft initial blob tx");
    let initial_blob_fee = initial.blob_fee_cap.expect("initial tx should have blob_fee_cap");
    assert!(initial_blob_fee > 0, "initial blob_fee_cap should be non-zero");

    let bumped = manager
        .increase_gas_price(
            &candidate,
            initial.gas_tip_cap,
            initial.gas_fee_cap,
            Some(initial_blob_fee),
        )
        .await
        .expect("should compute bumped fees");

    let bumped_blob_fee = bumped.blob_fee_cap.expect("bumped fees should have blob_fee_cap");
    assert!(
        bumped_blob_fee >= initial_blob_fee,
        "bumped blob_fee_cap {bumped_blob_fee} should be >= initial {initial_blob_fee}",
    );

    let fee_override = FeeOverride::new(bumped.gas_tip_cap, bumped.gas_fee_cap)
        .with_blob_fee_cap(bumped_blob_fee)
        .with_gas_limit_floor(initial.gas_limit);

    let replacement = manager
        .craft_tx(&candidate, Some(fee_override))
        .await
        .expect("should craft replacement blob tx");

    let replacement_blob_fee =
        replacement.blob_fee_cap.expect("replacement tx should have blob_fee_cap");
    assert!(
        replacement_blob_fee >= bumped_blob_fee,
        "replacement blob_fee_cap {replacement_blob_fee} should be >= bumped {bumped_blob_fee}",
    );
    assert!(
        replacement.gas_limit >= initial.gas_limit,
        "replacement gas_limit {} should be >= initial {}",
        replacement.gas_limit,
        initial.gas_limit,
    );
    assert!(
        replacement.gas_tip_cap >= bumped.gas_tip_cap,
        "replacement tip {} should be >= bumped tip {}",
        replacement.gas_tip_cap,
        bumped.gas_tip_cap,
    );

    let envelope = replacement.to_envelope().expect("should decode replacement TxEnvelope");
    assert!(envelope.is_eip4844(), "replacement should be EIP-4844");

    let signed = envelope.as_eip4844().expect("should be EIP-4844");
    assert!(
        matches!(signed.tx(), TxEip4844Variant::TxEip4844WithSidecar(_)),
        "replacement should have sidecar attached",
    );
}
