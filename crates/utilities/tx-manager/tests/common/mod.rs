//! Shared test helpers for tx-manager integration tests.
//!
//! Each integration test compiles this module independently, so not every
//! binary uses every item.
#![allow(dead_code, unreachable_pub)]

use std::sync::Arc;

use alloy_consensus::SignableTransaction;
use alloy_network::{EthereumWallet, TxSigner};
use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, B256, Bytes, Signature, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use base_tx_manager::{NoopTxMetrics, SendState, SimpleTxManager, TxCandidate, TxManagerConfig};

pub const TEST_RECIPIENT: Address = Address::with_last_byte(0x42);
pub const SAFE_ABORT_DEPTH: u64 = 3;

/// Spawns an Anvil instance and returns the provider, default wallet, and
/// instance handle.
pub fn setup_anvil() -> (RootProvider, EthereumWallet, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let provider = RootProvider::new_http(anvil.endpoint_url());
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let wallet = EthereumWallet::from(signer);
    (provider, wallet, anvil)
}

/// Creates a [`SimpleTxManager`] backed by a fresh Anvil instance.
pub async fn setup_with_config(
    config: TxManagerConfig,
) -> (SimpleTxManager<RootProvider>, alloy_node_bindings::AnvilInstance) {
    let (provider, wallet, anvil) = setup_anvil();
    let manager = SimpleTxManager::from_wallet(
        provider,
        wallet,
        config,
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect("should create manager");
    (manager, anvil)
}

/// Force-mine a block on Anvil so receipts are committed before queries.
pub async fn mine_block(provider: &RootProvider) {
    provider
        .raw_request::<(), String>("evm_mine".into(), ())
        .await
        .expect("evm_mine should succeed");
}

/// Value-transfer candidate with the given amount.
pub fn value_transfer(value: u64) -> TxCandidate {
    TxCandidate { to: Some(TEST_RECIPIENT), value: U256::from(value), ..Default::default() }
}

/// Simple value-transfer candidate reused across tests.
pub fn simple_tx_candidate() -> TxCandidate {
    value_transfer(1_000)
}

/// Publishes a simple value-transfer tx and returns the hash, send state,
/// and raw transaction bytes.
pub async fn publish_simple_tx(
    manager: &SimpleTxManager<RootProvider>,
) -> (B256, SendState, Bytes) {
    let candidate = simple_tx_candidate();
    let prepared = manager.craft_tx(&candidate, None).await.expect("should craft tx");
    let send_state = SendState::new(SAFE_ABORT_DEPTH).expect("should create send state");
    let raw_tx = prepared.raw_tx;
    let tx_hash = manager.publish_tx(&send_state, &raw_tx, None).await.expect("should publish tx");
    (tx_hash, send_state, raw_tx)
}

/// A signer that always fails, for testing error paths.
pub struct FailingSigner {
    pub address: Address,
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

/// Creates a [`SimpleTxManager`] whose wallet always fails to sign.
pub async fn setup_with_failing_signer(
    config: TxManagerConfig,
) -> (SimpleTxManager<RootProvider>, alloy_node_bindings::AnvilInstance) {
    let (provider, _, anvil) = setup_anvil();
    let wallet = EthereumWallet::from(FailingSigner { address: anvil.addresses()[0] });
    let manager = SimpleTxManager::from_wallet(
        provider,
        wallet,
        config,
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect("should create manager with failing signer");
    (manager, anvil)
}
