//! Shared test fixtures for the registrar crate.

use std::time::SystemTime;

use alloy_consensus::{Eip658Value, Receipt, ReceiptEnvelope};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::TransactionReceipt;
use base_tx_manager::{SendHandle, TxCandidate, TxManager};
use hex_literal::hex;
use k256::ecdsa::SigningKey;

use crate::{InstanceHealthStatus, ProverClient, ProverInstance};

/// Well-known Hardhat / Anvil account #0 private key.
pub const HARDHAT_KEY_0: [u8; 32] =
    hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// Hardhat / Anvil account #1 private key.
pub const HARDHAT_KEY_1: [u8; 32] =
    hex!("59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");

/// Hardhat / Anvil account #2 private key.
pub const HARDHAT_KEY_2: [u8; 32] =
    hex!("5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a");

/// Prover instance endpoint #1 used in registrar tests.
pub const EP1: &str = "10.0.0.1:8000";

/// Prover instance endpoint #2 used in registrar tests.
pub const EP2: &str = "10.0.0.2:8000";

/// Prover instance endpoint #3 used in registrar tests.
pub const EP3: &str = "10.0.0.3:8000";

/// Placeholder registry contract address used in registrar test configs.
pub const TEST_REGISTRY_ADDRESS: Address = Address::repeat_byte(0x01);

/// Tx manager stub for tests that only need to satisfy generic bounds.
#[derive(Debug, Clone, Copy)]
pub struct NoopTxManager;

impl TxManager for NoopTxManager {
    async fn send(&self, _candidate: TxCandidate) -> base_tx_manager::SendResponse {
        unreachable!("NoopTxManager does not submit transactions")
    }

    async fn send_async(&self, _candidate: TxCandidate) -> SendHandle {
        unreachable!("NoopTxManager does not submit async transactions")
    }

    fn sender_address(&self) -> Address {
        Address::ZERO
    }
}

/// Derives the uncompressed 65-byte public key from a private key.
pub fn public_key_from_private(private_key: &[u8; 32]) -> Vec<u8> {
    let signing_key = SigningKey::from_slice(private_key).unwrap();
    signing_key.verifying_key().to_encoded_point(false).as_bytes().to_vec()
}

/// Derives the signer address for a private key.
pub fn signer_from_private_key(private_key: &[u8; 32]) -> Address {
    ProverClient::derive_address(&public_key_from_private(private_key)).unwrap()
}

/// Builds a test [`ProverInstance`] with a deterministic instance id.
pub fn prover_instance(
    host_port: &str,
    health_status: InstanceHealthStatus,
    launch_time: Option<SystemTime>,
) -> ProverInstance {
    ProverInstance {
        instance_id: format!("i-{host_port}"),
        endpoint: url::Url::parse(&format!("http://{host_port}")).unwrap(),
        health_status,
        launch_time,
    }
}

/// Builds a healthy test [`ProverInstance`] with no launch time.
pub fn healthy_prover_instance(host_port: &str) -> ProverInstance {
    prover_instance(host_port, InstanceHealthStatus::Healthy, None)
}

/// Builds a transaction receipt with the requested success status.
pub fn stub_receipt_with_status(success: bool) -> TransactionReceipt {
    let inner = ReceiptEnvelope::Legacy(
        Receipt { status: Eip658Value::Eip658(success), cumulative_gas_used: 21_000, logs: vec![] }
            .into(),
    );
    TransactionReceipt {
        inner,
        transaction_hash: B256::ZERO,
        transaction_index: Some(0),
        block_hash: Some(B256::ZERO),
        block_number: Some(1),
        gas_used: 21_000,
        effective_gas_price: 1_000_000_000,
        blob_gas_used: None,
        blob_gas_price: None,
        from: Address::ZERO,
        to: Some(Address::ZERO),
        contract_address: None,
    }
}
