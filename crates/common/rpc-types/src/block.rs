//! Block and header RPC response types.

use core::ops::{Deref, DerefMut};

use alloy_consensus::{BlockHeader, Header as ConsensusHeader};
use alloy_network_primitives::HeaderResponse;
use alloy_primitives::{Address, B64, B256, BlockHash, Bloom, Bytes, U256};
use alloy_rpc_types_eth::{Block, Header};

use crate::Transaction;

/// Base block RPC response type.
pub type BaseBlockResponse<T = Transaction> = Block<T, BaseHeaderResponse>;

/// Base header RPC response type.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseHeaderResponse<H = Header> {
    /// Standard Ethereum header response fields.
    #[serde(flatten)]
    pub inner: H,
    /// Full Unix timestamp in milliseconds when sub-second timing is available.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub timestamp_ms: Option<u64>,
}

impl<H> BaseHeaderResponse<H> {
    /// Creates a header response without Base millisecond timestamp extensions.
    pub const fn new(inner: H) -> Self {
        Self { inner, timestamp_ms: None }
    }

    /// Creates a header response with an explicit millisecond timestamp.
    pub const fn with_timestamp_ms(inner: H, timestamp_ms: Option<u64>) -> Self {
        Self { inner, timestamp_ms }
    }

    /// Consumes the response and returns the wrapped Ethereum header response.
    pub fn into_inner(self) -> H {
        self.inner
    }
}

impl<H> From<H> for BaseHeaderResponse<H> {
    fn from(inner: H) -> Self {
        Self::new(inner)
    }
}

impl<H> Deref for BaseHeaderResponse<H> {
    type Target = H;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<H> DerefMut for BaseHeaderResponse<H> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<H> AsRef<H> for BaseHeaderResponse<H> {
    fn as_ref(&self) -> &H {
        &self.inner
    }
}

impl AsRef<ConsensusHeader> for BaseHeaderResponse<Header<ConsensusHeader>> {
    fn as_ref(&self) -> &ConsensusHeader {
        self.inner.as_ref()
    }
}

impl<H: BlockHeader> BlockHeader for BaseHeaderResponse<H> {
    fn parent_hash(&self) -> B256 {
        self.inner.parent_hash()
    }

    fn ommers_hash(&self) -> B256 {
        self.inner.ommers_hash()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn state_root(&self) -> B256 {
        self.inner.state_root()
    }

    fn transactions_root(&self) -> B256 {
        self.inner.transactions_root()
    }

    fn receipts_root(&self) -> B256 {
        self.inner.receipts_root()
    }

    fn withdrawals_root(&self) -> Option<B256> {
        self.inner.withdrawals_root()
    }

    fn logs_bloom(&self) -> Bloom {
        self.inner.logs_bloom()
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn number(&self) -> u64 {
        self.inner.number()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn mix_hash(&self) -> Option<B256> {
        self.inner.mix_hash()
    }

    fn nonce(&self) -> Option<B64> {
        self.inner.nonce()
    }

    fn base_fee_per_gas(&self) -> Option<u64> {
        self.inner.base_fee_per_gas()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn excess_blob_gas(&self) -> Option<u64> {
        self.inner.excess_blob_gas()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn requests_hash(&self) -> Option<B256> {
        self.inner.requests_hash()
    }

    fn block_access_list_hash(&self) -> Option<B256> {
        self.inner.block_access_list_hash()
    }

    fn slot_number(&self) -> Option<u64> {
        self.inner.slot_number()
    }

    fn extra_data(&self) -> &Bytes {
        self.inner.extra_data()
    }
}

impl<H: HeaderResponse> HeaderResponse for BaseHeaderResponse<H> {
    fn hash(&self) -> BlockHash {
        self.inner.hash()
    }
}

#[cfg(feature = "reth")]
impl<T: alloy_consensus::Sealable> reth_rpc_convert::FromConsensusHeader<T>
    for BaseHeaderResponse<Header<T>>
{
    fn from_consensus_header(
        header: reth_primitives_traits::SealedHeader<T>,
        block_size: usize,
    ) -> Self {
        Self::new(Header::from_consensus(header.into(), None, Some(U256::from(block_size))))
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header as ConsensusHeader;
    use alloy_rpc_types_eth::Header;
    use serde_json::json;

    use super::BaseHeaderResponse;

    #[test]
    fn base_header_response_serializes_timestamp_ms() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let response = BaseHeaderResponse::with_timestamp_ms(inner, Some(42_200));
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["timestamp"], json!("0x2a"));
        assert_eq!(value["timestampMs"], json!("0xa4d8"));
    }

    #[test]
    fn base_header_response_omits_timestamp_ms_when_absent() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let value = serde_json::to_value(BaseHeaderResponse::new(inner.clone())).unwrap();

        assert!(value.get("timestampMs").is_none());

        let decoded: BaseHeaderResponse = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.inner, inner);
        assert_eq!(decoded.timestamp_ms, None);
    }

    #[test]
    fn base_header_response_preserves_header_field_access() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, number: 7, ..Default::default() });
        let response = BaseHeaderResponse::with_timestamp_ms(inner, Some(42_200));

        assert_eq!(response.timestamp, 42);
        assert_eq!(response.number, 7);
    }

    #[test]
    fn base_header_response_round_trips_through_json() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let original = BaseHeaderResponse::with_timestamp_ms(inner, Some(42_200));
        let json = serde_json::to_value(&original).unwrap();
        let decoded: BaseHeaderResponse = serde_json::from_value(json).unwrap();

        assert_eq!(original, decoded);
    }
}
