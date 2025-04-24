//! Sourced from: https://github.com/Layr-Labs/eigensdk-rs/blob/dev/crates/signer/src/web3_signer.rs
//! Replace with Web3Signer from Alloy when it's released.
use alloy_consensus::{transaction::RlpEcdsaDecodableTx, SignableTransaction, TxLegacy};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_rpc_client::{ClientBuilder, ReqwestClient};
use alloy_signer::Signature;
use async_trait::async_trait;
use serde::Serialize;
use url::Url;

/// A signer that sends an RPC request to sign a transaction remotely
/// Implements `eth_signTransaction` method of Consensys Web3 Signer
/// Reference: https://docs.web3signer.consensys.io/reference/api/json-rpc#eth_signtransaction
#[derive(Debug)]
pub struct Web3Signer {
    /// Client used to send an RPC request
    pub client: ReqwestClient,
    /// Address of the account that intends to sign a transaction.
    /// It must match the `from` field in the transaction.
    pub address: Address,
}

/// Parameters for the `eth_signTransaction` method
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct SignTransactionParams {
    from: String,
    to: TxKind,
    value: U256,
    gas: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    gas_price: Option<String>,
    nonce: String,
    data: String,
}

impl Web3Signer {
    pub fn new(address: Address, url: Url) -> Self {
        Web3Signer { client: ClientBuilder::default().http(url), address }
    }
}

#[async_trait]
impl TxSigner<Signature> for Web3Signer {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        let params = SignTransactionParams {
            from: self.address.to_string(),
            to: tx.to().into(),
            value: tx.value(),
            gas: format!("0x{:x}", tx.gas_limit()),
            gas_price: tx.gas_price().map(|price| format!("0x{price:x}")),
            nonce: format!("0x{:x}", tx.nonce()),
            data: Bytes::copy_from_slice(tx.input()).to_string(),
        };

        let request = self
            .client
            .request::<Vec<SignTransactionParams>, Bytes>("eth_signTransaction", vec![params]);
        let rlp_encoded_signed_tx = request.await.map_err(alloy_signer::Error::other)?;

        let signed_tx = TxLegacy::rlp_decode_signed(&mut rlp_encoded_signed_tx.as_ref())
            .map_err(alloy_signer::Error::other)?;

        Ok(*signed_tx.signature())
    }
}
