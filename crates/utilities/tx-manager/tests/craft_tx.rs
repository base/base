//! Integration tests for [`SimpleTxManager`] transaction construction with Anvil.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::{Decodable2718, eip4844::Blob};
use alloy_network::{EthereumWallet, TxSigner};
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, Bytes, Signature, TxKind, U256};
use alloy_provider::RootProvider;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_tx_manager::{
    FeeCalculator, FeeOverride, GasPriceCaps, SimpleTxManager, TxCandidate, TxManager,
    TxManagerConfig, TxManagerError,
};

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`]
/// configured with the given [`TxManagerConfig`].
async fn setup_with_config(
    config: TxManagerConfig,
) -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let chain_id = anvil.chain_id();
    let manager = SimpleTxManager::new(provider, wallet, config, chain_id)
        .await
        .expect("should create manager");
    (manager, anvil)
}

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`]
/// with the default [`TxManagerConfig`].
async fn setup() -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    setup_with_config(TxManagerConfig::default()).await
}

/// Decodes raw RLP-encoded transaction bytes into the inner [`TxEip1559`].
///
/// Panics if the bytes are not a valid EIP-2718 envelope or the
/// transaction type is not EIP-1559.
fn decode_eip1559(raw: &Bytes) -> TxEip1559 {
    let envelope =
        TxEnvelope::decode_2718(&mut raw.as_ref()).expect("should decode as valid TxEnvelope");
    match envelope {
        TxEnvelope::Eip1559(signed) => signed.strip_signature(),
        other => panic!("expected EIP-1559, got {other:?}"),
    }
}

#[tokio::test]
async fn craft_tx_produces_valid_signed_eip1559_transaction() {
    let (manager, anvil) = setup().await;

    let to = Address::with_last_byte(0x42);
    let value = U256::from(1_000_000_000u64);
    let candidate = TxCandidate {
        to: Some(to),
        value,
        gas_limit: 0, // auto-estimate
        ..Default::default()
    };

    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let tx = decode_eip1559(&prepared.raw_tx);

    assert_eq!(tx.to, TxKind::Call(to));
    assert_eq!(tx.value, value);
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
    // PreparedTx fees must match the fees encoded in the signed transaction.
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

    // Use a gas_limit well above the 21,000 intrinsic gas for a simple
    // value transfer.  estimate_gas returns ~21,000, so the
    // `candidate.gas_limit.max(estimated)` floor logic must produce
    // 100,000 — not the estimate.
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 100_000,
        ..Default::default()
    };

    let prepared =
        manager.craft_tx(&candidate, None).await.expect("should craft tx with explicit gas");
    let tx = decode_eip1559(&prepared.raw_tx);

    // The decoded gas_limit must equal the caller's explicit value,
    // proving it was used as a floor above the provider estimate.
    assert_eq!(tx.gas_limit, 100_000);
}

#[tokio::test]
async fn craft_tx_rejects_blob_transactions() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        blobs: vec![Blob::default()],
        ..Default::default()
    };

    let err = manager.craft_tx(&candidate, None).await.expect_err("should reject blob tx");
    match &err {
        TxManagerError::Unsupported(msg) => {
            assert!(
                msg.contains("blob transactions are not yet supported"),
                "expected blob rejection message, got: {msg}",
            );
        }
        other => panic!("expected TxManagerError::Unsupported, got {other:?}"),
    }
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
    let tx = decode_eip1559(&prepared.raw_tx);

    assert_eq!(tx.to, TxKind::Create);
}

#[tokio::test]
async fn suggest_gas_price_caps_returns_valid_estimates() {
    let (manager, _anvil) = setup().await;

    let caps: GasPriceCaps =
        manager.suggest_gas_price_caps().await.expect("should return gas price caps");

    // On an Anvil instance, tip and fee cap should be non-zero.
    assert!(caps.gas_tip_cap > 0, "tip_cap should be non-zero");
    // gas_fee_cap = tip + 2 * base_fee, which is always > tip alone.
    assert!(caps.gas_fee_cap > caps.gas_tip_cap, "fee_cap should exceed tip_cap");
    // raw_gas_fee_cap (from provider values before enforcing minimums) should
    // be <= gas_fee_cap (after enforcing minimums) and non-zero on Anvil.
    assert!(caps.raw_gas_fee_cap > 0, "raw_gas_fee_cap should be non-zero");
    assert!(
        caps.raw_gas_fee_cap <= caps.gas_fee_cap,
        "raw_gas_fee_cap should be <= gas_fee_cap after enforcing minimums",
    );
    // Blob fee cap should be None for non-blob transactions.
    assert!(caps.blob_fee_cap.is_none(), "blob_fee_cap should be None");
}

#[tokio::test]
async fn prepare_produces_valid_signed_transaction() {
    let (manager, _anvil) = setup().await;

    let to = Address::with_last_byte(0x42);
    let candidate = TxCandidate {
        to: Some(to),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let prepared = manager.prepare(&candidate, None).await.expect("should prepare tx");
    let tx = decode_eip1559(&prepared.raw_tx);

    // Confirm the candidate's fields survive the retry wrapper.
    assert_eq!(tx.to, TxKind::Call(to));
    assert_eq!(tx.value, U256::from(1_000u64));
}

#[tokio::test]
async fn prepare_returns_channel_closed_when_manager_is_closed() {
    let (manager, _anvil) = setup().await;

    manager.close();

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::ZERO,
        ..Default::default()
    };

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

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    // Craft two transactions — nonces should be sequential.
    let prepared1 = manager.craft_tx(&candidate, None).await.expect("first tx");
    let prepared2 = manager.craft_tx(&candidate, None).await.expect("second tx");

    assert_eq!(decode_eip1559(&prepared1.raw_tx).nonce, 0);
    assert_eq!(decode_eip1559(&prepared2.raw_tx).nonce, 1);
}

#[tokio::test]
async fn new_rejects_chain_id_mismatch() {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    let config = TxManagerConfig::default();

    // Anvil uses chain_id 31337 by default; supply a wrong one.
    let wrong_chain_id = 999;
    let err = SimpleTxManager::new(provider, wallet, config, wrong_chain_id)
        .await
        .expect_err("should reject mismatched chain_id");

    match &err {
        TxManagerError::InvalidConfig(msg) => {
            assert!(
                msg.contains("chain_id mismatch"),
                "expected chain_id mismatch error, got: {msg}",
            );
        }
        other => panic!("expected TxManagerError::InvalidConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn craft_tx_preserves_calldata() {
    let (manager, _anvil) = setup().await;

    let calldata = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]);
    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        tx_data: calldata.clone(),
        value: U256::ZERO,
        gas_limit: 0,
        ..Default::default()
    };

    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx with calldata");
    let tx = decode_eip1559(&prepared.raw_tx);

    assert_eq!(tx.input, calldata, "calldata should be preserved in the decoded transaction");
}

#[tokio::test]
async fn suggest_gas_price_caps_enforces_min_tip_cap_and_min_basefee() {
    // Set minimums well above what Anvil returns (Anvil tip is ~1 gwei,
    // base fee ~1 gwei) so the .max() enforcement is exercised.
    let high_min_tip = 50_000_000_000u128; // 50 gwei
    let high_min_basefee = 100_000_000_000u128; // 100 gwei
    let config = TxManagerConfig {
        min_tip_cap: high_min_tip,
        min_basefee: high_min_basefee,
        ..TxManagerConfig::default()
    };

    let (manager, _anvil) = setup_with_config(config).await;

    let caps = manager.suggest_gas_price_caps().await.expect("should return caps");

    // Tip cap should be at least the configured minimum.
    assert!(
        caps.gas_tip_cap >= high_min_tip,
        "gas_tip_cap {} should be >= min_tip_cap {high_min_tip}",
        caps.gas_tip_cap,
    );
    // Fee cap should reflect the enforced minimum base fee:
    // gas_fee_cap = tip + 2 * base_fee >= high_min_tip + 2 * high_min_basefee.
    let expected_min_fee_cap = high_min_tip + 2 * high_min_basefee;
    assert!(
        caps.gas_fee_cap >= expected_min_fee_cap,
        "gas_fee_cap {} should be >= {expected_min_fee_cap} (min_tip + 2 * min_basefee)",
        caps.gas_fee_cap,
    );
    // raw_gas_fee_cap should be strictly less than gas_fee_cap since we
    // inflated both tip and base fee above the Anvil defaults.
    assert!(
        caps.raw_gas_fee_cap < caps.gas_fee_cap,
        "raw_gas_fee_cap {} should be < gas_fee_cap {} when minimums are enforced",
        caps.raw_gas_fee_cap,
        caps.gas_fee_cap,
    );
}

#[tokio::test]
async fn new_rejects_invalid_config() {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);

    let invalid_config = TxManagerConfig { num_confirmations: 0, ..TxManagerConfig::default() };

    let err = SimpleTxManager::new(provider, wallet, invalid_config, anvil.chain_id())
        .await
        .expect_err("should reject invalid config");

    match &err {
        TxManagerError::InvalidConfig(msg) => {
            assert!(
                msg.contains("num_confirmations"),
                "expected num_confirmations validation error, got: {msg}",
            );
        }
        other => panic!("expected TxManagerError::InvalidConfig, got {other:?}"),
    }
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

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        ..Default::default()
    };

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

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        blobs: vec![Blob::default()],
        ..Default::default()
    };

    let err = manager.prepare(&candidate, None).await.expect_err("should reject blob tx");

    assert!(
        matches!(err, TxManagerError::Unsupported(_)),
        "expected TxManagerError::Unsupported, got {err:?}",
    );
}

/// A signer that always fails, used to test nonce rollback on sign failure.
///
/// Delegates `address()` to a real signer so the `EthereumWallet` routes
/// signing requests correctly, then returns an error in `sign_transaction`.
struct FailingSigner {
    /// The address this signer claims to own.
    address: Address,
}

#[async_trait]
impl TxSigner<Signature> for FailingSigner {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        _tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        Err(alloy_signer::Error::other("deliberately failing signer"))
    }
}

/// Helper: spawns an Anvil instance and returns a [`SimpleTxManager`]
/// whose wallet uses a [`FailingSigner`], causing all signing attempts
/// to fail. The `FailingSigner` claims the same address as Anvil's first
/// account so the rest of the pipeline (gas estimation, nonce) works.
async fn setup_with_failing_signer() -> (SimpleTxManager, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let url = anvil.endpoint_url();
    let provider = RootProvider::new_http(url);
    let address = anvil.addresses()[0];
    let wallet = EthereumWallet::from(FailingSigner { address });
    let chain_id = anvil.chain_id();
    let manager = SimpleTxManager::new(provider, wallet, TxManagerConfig::default(), chain_id)
        .await
        .expect("should create manager with failing signer");
    (manager, anvil)
}

#[tokio::test]
async fn accessors_return_expected_values() {
    let (manager, anvil) = setup().await;

    assert_eq!(manager.chain_id(), anvil.chain_id());
    assert_eq!(
        manager.sender_address(),
        anvil.addresses()[0],
        "sender_address should match the first Anvil account",
    );
    // config() returns the default config used in setup().
    assert_eq!(manager.config().num_confirmations, TxManagerConfig::default().num_confirmations);
    // provider() and wallet() are opaque, but we can verify they are accessible.
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
    let (failing_manager, _anvil) = setup_with_failing_signer().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1u64),
        gas_limit: 0,
        ..Default::default()
    };

    // First call: signing fails, nonce should be rolled back.
    let err = failing_manager.craft_tx(&candidate, None).await.expect_err("should fail to sign");
    assert!(matches!(err, TxManagerError::Sign(_)), "expected TxManagerError::Sign, got {err:?}",);

    // Query the same NonceManager directly. If rollback worked the
    // cached nonce is still 0; if it didn't, it would have advanced
    // to 1 (the nonce after 0 was "consumed").
    let guard = failing_manager.nonce_manager().next_nonce().await.expect("should reserve nonce");
    assert_eq!(guard.nonce(), 0, "nonce should be 0 — rollback must have restored it");
    // Drop the guard so the nonce is consumed (prevents test pollution).
    drop(guard);
}

/// When fee overrides are above network fees, the [`PreparedTx`] must use the
/// overrides (since `craft_tx` takes `max(network_fee, override)`).
#[tokio::test]
async fn craft_tx_with_fee_overrides_uses_overrides_when_above_network() {
    let (manager, _anvil) = setup().await;

    // Learn current network fees so we can set overrides above them.
    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");
    let override_tip = caps.gas_tip_cap + 50_000_000_000; // +50 gwei
    let override_fee_cap = caps.gas_fee_cap + 100_000_000_000; // +100 gwei

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let overrides = FeeOverride::new(override_tip, override_fee_cap);
    let prepared = manager
        .craft_tx(&candidate, Some(overrides))
        .await
        .expect("should craft tx with fee overrides");

    // PreparedTx must reflect the overrides since they exceed network fees.
    assert_eq!(
        prepared.gas_tip_cap, override_tip,
        "gas_tip_cap should equal the override when it exceeds the network fee",
    );
    assert_eq!(
        prepared.gas_fee_cap, override_fee_cap,
        "gas_fee_cap should equal the override when it exceeds the network fee",
    );

    // The signed transaction must also carry the override fees.
    let tx = decode_eip1559(&prepared.raw_tx);
    assert_eq!(tx.max_priority_fee_per_gas, override_tip);
    assert_eq!(tx.max_fee_per_gas, override_fee_cap);
}

/// When fee overrides are below network fees, the [`PreparedTx`] must use the
/// network fees (since `craft_tx` takes `max(network_fee, override)`).
#[tokio::test]
async fn craft_tx_with_fee_overrides_uses_network_when_overrides_below() {
    let (manager, _anvil) = setup().await;

    // Learn current network fees.
    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");

    // Set overrides well below network fees (1 wei each).
    let override_tip = 1u128;
    let override_fee_cap = 1u128;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let overrides = FeeOverride::new(override_tip, override_fee_cap);
    let prepared = manager
        .craft_tx(&candidate, Some(overrides))
        .await
        .expect("should craft tx with low fee overrides");

    // PreparedTx must use network fees since they exceed the overrides.
    assert_eq!(
        prepared.gas_tip_cap, caps.gas_tip_cap,
        "gas_tip_cap should use network fee, not the low override",
    );
    assert_eq!(
        prepared.gas_fee_cap, caps.gas_fee_cap,
        "gas_fee_cap should use network fee, not the low override",
    );
}

/// Verifies that the [`PreparedTx`] fee fields are always consistent with the
/// fees encoded in the signed transaction — the core invariant that
/// [`PreparedTx`] is designed to guarantee.
#[tokio::test]
async fn prepared_tx_fees_match_decoded_transaction_with_overrides() {
    let (manager, _anvil) = setup().await;

    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");
    let override_tip = caps.gas_tip_cap + 10_000_000_000;
    let override_fee_cap = caps.gas_fee_cap + 20_000_000_000;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let overrides = FeeOverride::new(override_tip, override_fee_cap);
    let prepared = manager.craft_tx(&candidate, Some(overrides)).await.expect("should craft tx");
    let tx = decode_eip1559(&prepared.raw_tx);

    assert_eq!(
        prepared.gas_tip_cap, tx.max_priority_fee_per_gas,
        "PreparedTx gas_tip_cap must match decoded max_priority_fee_per_gas",
    );
    assert_eq!(
        prepared.gas_fee_cap, tx.max_fee_per_gas,
        "PreparedTx gas_fee_cap must match decoded max_fee_per_gas",
    );
}

#[test]
fn simple_tx_manager_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SimpleTxManager>();
}

// ── increase_gas_price ──────────────────────────────────────────────

#[tokio::test]
async fn increase_gas_price_bumps_above_threshold() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

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

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

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
async fn increase_gas_price_preserves_none_blob_fee_cap() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");

    let bumped = manager
        .increase_gas_price(&candidate, caps.gas_tip_cap, caps.gas_fee_cap, None)
        .await
        .expect("should compute bumped fees");

    assert!(
        bumped.blob_fee_cap.is_none(),
        "blob_fee_cap should remain None when old_blob_fee_cap is None",
    );
}

#[tokio::test]
async fn increase_gas_price_bumps_blob_fee_cap() {
    let (manager, _anvil) = setup().await;

    let candidate = TxCandidate {
        to: Some(Address::with_last_byte(0x42)),
        value: U256::from(1_000u64),
        gas_limit: 0,
        ..Default::default()
    };

    let caps = manager.suggest_gas_price_caps().await.expect("should get caps");

    let old_blob_fee_cap = 1_000_000_000u128; // 1 gwei
    let bumped = manager
        .increase_gas_price(&candidate, caps.gas_tip_cap, caps.gas_fee_cap, Some(old_blob_fee_cap))
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
