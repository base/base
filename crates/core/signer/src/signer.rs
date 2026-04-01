//! Remote signer implementation that delegates signing to an external signer sidecar.

use std::time::Duration;

use alloy_consensus::{SignableTransaction, TxEnvelope};
use alloy_eips::Decodable2718;
use alloy_network::{TransactionBuilder, TxSigner};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind};
use alloy_rpc_types_eth::TransactionRequest;
use async_trait::async_trait;
use base_alloy_rpc_jsonrpsee::EthSignerApiClient;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use tracing::debug;
use url::Url;

use crate::RemoteSignerError;

/// A remote transaction signer that delegates signing to an external signer sidecar
/// via the `eth_signTransaction` JSON-RPC method.
///
/// Implements alloy's [`TxSigner<Signature>`] trait, allowing it to be used with
/// [`alloy_network::EthereumWallet`] for seamless integration with the standard
/// signing pipeline:
///
/// ```rust,ignore
/// let signer = RemoteSigner::new(endpoint, address).unwrap();
/// let wallet = EthereumWallet::from(signer);
/// ```
#[derive(Debug)]
pub struct RemoteSigner {
    /// The jsonrpsee HTTP client used to communicate with the signer.
    pub client: HttpClient,
    /// The address of the account managed by the remote signer.
    pub address: Address,
}

impl RemoteSigner {
    /// Creates a new [`RemoteSigner`] with a default HTTP client.
    pub fn new(endpoint: Url, address: Address) -> Result<Self, RemoteSignerError> {
        let client = HttpClientBuilder::default()
            .request_timeout(Duration::from_secs(5))
            .build(endpoint.as_str())
            .map_err(RemoteSignerError::Client)?;
        Ok(Self { client, address })
    }

    /// Creates a new [`RemoteSigner`] with a pre-configured [`HttpClient`].
    ///
    /// Use this constructor when you need custom HTTP client settings
    /// (e.g. custom TLS configuration, timeouts, or middleware).
    pub const fn with_client(client: HttpClient, address: Address) -> Self {
        Self { client, address }
    }

    /// Builds a [`TransactionRequest`] from a [`SignableTransaction`] for submission
    /// to the remote signer's `eth_signTransaction` endpoint.
    pub fn build_tx_request(&self, tx: &dyn SignableTransaction<Signature>) -> TransactionRequest {
        let mut request = TransactionRequest::default()
            .with_nonce(tx.nonce())
            .with_gas_limit(tx.gas_limit())
            .with_value(tx.value())
            .with_input(tx.input().clone());

        request.set_from(self.address);

        match tx.kind() {
            TxKind::Call(addr) => request.set_to(addr),
            TxKind::Create => request = request.into_create(),
        }

        if let Some(chain_id) = tx.chain_id() {
            request = request.with_chain_id(chain_id);
        }

        if tx.is_dynamic_fee() {
            request = request.with_max_fee_per_gas(tx.max_fee_per_gas());
        } else if let Some(gas_price) = tx.gas_price() {
            request = request.with_gas_price(gas_price);
        }

        if let Some(max_priority_fee) = tx.max_priority_fee_per_gas() {
            request = request.with_max_priority_fee_per_gas(max_priority_fee);
        }

        if let Some(access_list) = tx.access_list() {
            request = request.with_access_list(access_list.clone());
        }

        if let Some(max_blob_fee) = tx.max_fee_per_blob_gas() {
            request.max_fee_per_blob_gas = Some(max_blob_fee);
        }

        if let Some(blob_hashes) = tx.blob_versioned_hashes()
            && !blob_hashes.is_empty()
        {
            request.blob_versioned_hashes = Some(blob_hashes.to_vec());
        }

        if let Some(auth_list) = tx.authorization_list()
            && !auth_list.is_empty()
        {
            request.authorization_list = Some(auth_list.to_vec());
        }

        request.transaction_type = Some(tx.ty());

        request
    }

    /// Verifies that the signed envelope's transaction content matches the original
    /// transaction by comparing signature hashes.
    ///
    /// This provides defense-in-depth beyond signature verification, ensuring
    /// the remote signer did not alter the transaction content.
    pub fn verify_envelope_content(
        envelope: &TxEnvelope,
        expected_hash: &B256,
    ) -> Result<(), RemoteSignerError> {
        let received = envelope.signature_hash();
        if received != *expected_hash {
            return Err(RemoteSignerError::ContentMismatch { expected: *expected_hash, received });
        }
        Ok(())
    }

    /// Verifies that the signature recovers to the expected signer address.
    pub fn verify_signature(
        &self,
        signature: &Signature,
        hash: &B256,
    ) -> Result<(), RemoteSignerError> {
        let recovered = signature
            .recover_address_from_prehash(hash)
            .map_err(|e| RemoteSignerError::Recovery(e.to_string()))?;
        if recovered != self.address {
            return Err(RemoteSignerError::SignerMismatch { expected: self.address, recovered });
        }
        Ok(())
    }
}

#[async_trait]
impl TxSigner<Signature> for RemoteSigner {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        let request = self.build_tx_request(tx);

        debug!(
            address = %self.address,
            nonce = ?request.nonce,
            chain_id = ?request.chain_id,
            tx_type = ?request.transaction_type,
            "signing transaction via remote signer",
        );

        let bytes: Bytes = EthSignerApiClient::sign_transaction(&self.client, request)
            .await
            .map_err(|e| alloy_signer::Error::other(RemoteSignerError::Rpc(e)))?;

        let envelope = TxEnvelope::decode_2718(&mut bytes.as_ref())
            .map_err(|e| alloy_signer::Error::other(RemoteSignerError::Decode(e.to_string())))?;

        let hash = tx.signature_hash();
        Self::verify_envelope_content(&envelope, &hash).map_err(alloy_signer::Error::other)?;

        let signature = *envelope.signature();
        self.verify_signature(&signature, &hash).map_err(alloy_signer::Error::other)?;

        Ok(signature)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{TxEip1559, TxLegacy};
    use alloy_network::EthereumWallet;
    use alloy_node_bindings::Anvil;
    use alloy_primitives::U256;
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    #[test]
    fn address_returns_configured_address() {
        let address = Address::repeat_byte(0x42);
        let signer = test_signer(address);
        assert_eq!(signer.address(), address);
    }

    fn test_signer(address: Address) -> RemoteSigner {
        RemoteSigner::new(Url::parse("http://127.0.0.1:1").unwrap(), address).unwrap()
    }

    fn default_test_tx() -> TxEip1559 {
        TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::ZERO),
            value: U256::from(100),
            ..Default::default()
        }
    }

    #[test]
    fn build_tx_request_maps_eip1559_fields() {
        let from = Address::repeat_byte(0x01);
        let to = Address::repeat_byte(0x02);
        let signer = test_signer(from);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 42,
            gas_limit: 21_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(to),
            value: U256::from(1000),
            ..Default::default()
        };

        let request = signer.build_tx_request(&tx);

        assert_eq!(request.from, Some(from));
        assert_eq!(request.nonce, Some(42));
        assert_eq!(request.gas, Some(21_000));
        assert_eq!(request.max_fee_per_gas, Some(100));
        assert_eq!(request.max_priority_fee_per_gas, Some(10));
        assert_eq!(request.value, Some(U256::from(1000)));
        assert_eq!(request.chain_id, Some(1));
        assert_eq!(request.transaction_type, Some(2));
    }

    #[test]
    fn build_tx_request_handles_create() {
        let from = Address::repeat_byte(0x01);
        let signer = test_signer(from);
        let tx = TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Create,
            ..Default::default()
        };

        let request = signer.build_tx_request(&tx);

        assert_eq!(request.to, Some(TxKind::Create));
    }

    #[test]
    fn build_tx_request_handles_legacy_gas_price() {
        let from = Address::repeat_byte(0x01);
        let to = Address::repeat_byte(0x02);
        let signer = test_signer(from);
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 5,
            gas_limit: 21_000,
            gas_price: 50,
            to: TxKind::Call(to),
            value: U256::from(500),
            ..Default::default()
        };

        let request = signer.build_tx_request(&tx);

        assert_eq!(request.gas_price, Some(50));
        assert_eq!(request.nonce, Some(5));
        assert_eq!(request.transaction_type, Some(0));
    }

    #[tokio::test]
    async fn sign_transaction_roundtrip_with_anvil() {
        let anvil = Anvil::new().spawn();
        let address = anvil.addresses()[0];
        let signer = RemoteSigner::new(anvil.endpoint_url(), address).unwrap();

        let mut tx = TxEip1559 {
            chain_id: anvil.chain_id(),
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::from(100),
            ..Default::default()
        };

        let sig = signer.sign_transaction(&mut tx).await.expect("signing should succeed");

        // Verify the signature recovers to the expected address.
        let hash = tx.signature_hash();
        let recovered = sig.recover_address_from_prehash(&hash).expect("should recover address");
        assert_eq!(recovered, address);
    }

    #[test]
    fn ethereum_wallet_from_remote_signer() {
        let signer = test_signer(Address::repeat_byte(0x01));

        // Verify that RemoteSigner can be used with EthereumWallet.
        let _wallet = EthereumWallet::from(signer);
    }

    #[test]
    fn verify_signature_rejects_mismatch() {
        let real_signer = PrivateKeySigner::random();
        let wrong_address = Address::repeat_byte(0x42);
        let remote = test_signer(wrong_address);

        let hash = B256::repeat_byte(0x01);
        let sig = real_signer.sign_hash_sync(&hash).unwrap();

        let err = remote.verify_signature(&sig, &hash).unwrap_err();
        assert!(matches!(
            err,
            RemoteSignerError::SignerMismatch { expected, recovered }
                if expected == wrong_address && recovered == real_signer.address()
        ));
    }

    #[test]
    fn verify_signature_accepts_match() {
        let real_signer = PrivateKeySigner::random();
        let remote = test_signer(real_signer.address());

        let hash = B256::repeat_byte(0x01);
        let sig = real_signer.sign_hash_sync(&hash).unwrap();

        remote.verify_signature(&sig, &hash).unwrap();
    }

    #[tokio::test]
    async fn sign_transaction_transport_failure() {
        // test_signer points at 127.0.0.1:1 which is not running.
        let signer = test_signer(Address::repeat_byte(0x01));
        let mut tx = default_test_tx();

        signer.sign_transaction(&mut tx).await.unwrap_err();
    }

    #[test]
    fn verify_envelope_content_rejects_mismatch() {
        let key = PrivateKeySigner::random();
        let tx = default_test_tx();

        let sig = key.sign_hash_sync(&tx.signature_hash()).unwrap();
        let signed = tx.into_signed(sig);
        let envelope = TxEnvelope::from(signed);

        let wrong_hash = B256::repeat_byte(0xff);
        let err = RemoteSigner::verify_envelope_content(&envelope, &wrong_hash).unwrap_err();
        assert!(matches!(err, RemoteSignerError::ContentMismatch { .. }));
    }

    #[test]
    fn verify_envelope_content_accepts_match() {
        let key = PrivateKeySigner::random();
        let tx = default_test_tx();

        let expected_hash = tx.signature_hash();
        let sig = key.sign_hash_sync(&expected_hash).unwrap();
        let signed = tx.into_signed(sig);
        let envelope = TxEnvelope::from(signed);

        RemoteSigner::verify_envelope_content(&envelope, &expected_hash).unwrap();
    }
}
