//! Shared helpers for tx-manager integration tests.

use std::sync::Arc;

use alloy_node_bindings::Anvil;
use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_signer_local::PrivateKeySigner;
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxCandidate, TxManagerConfig};

const TEST_RECIPIENT: Address = Address::with_last_byte(0x42);

/// Creates a manager backed by a fresh Anvil instance.
pub async fn setup_with_config(
    config: TxManagerConfig,
) -> (SimpleTxManager, RootProvider, alloy_node_bindings::AnvilInstance) {
    let anvil = Anvil::new().spawn();
    let provider = RootProvider::new_http(anvil.endpoint_url());
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let manager = SimpleTxManager::new(
        provider.clone(),
        SignerConfig::local(signer),
        config,
        anvil.chain_id(),
        Arc::new(NoopTxMetrics),
    )
    .await
    .expect("should create manager");
    (manager, provider, anvil)
}

/// Mines one block and waits for the RPC response.
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
