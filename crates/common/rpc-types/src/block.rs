//! Block and header RPC response types.

use alloy_consensus::{BlockHeader, Header as ConsensusHeader};
use alloy_network_primitives::HeaderResponse;
use alloy_primitives::{Address, B64, B256, BlockHash, Bloom, Bytes, U256};
use alloy_rpc_types_eth::{Block, Header};
use base_common_consensus::{BaseHeader, TIMESTAMP_MILLIS_PER_SECOND, TimestampMillisPartError};

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
    /// Full Unix timestamp in milliseconds for post-Beryl Base blocks.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub timestamp_ms: Option<u64>,
    /// Sub-second millisecond component of the block timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub timestamp_millis_part: Option<u16>,
}

impl<H> BaseHeaderResponse<H> {
    /// Creates a header response without Base millisecond timestamp extensions.
    pub const fn new(inner: H) -> Self {
        Self { inner, timestamp_ms: None, timestamp_millis_part: None }
    }

    /// Creates a header response with a computed Base millisecond timestamp extension.
    pub fn with_timestamp_millis_part(
        inner: H,
        timestamp_millis_part: u16,
    ) -> Result<Self, TimestampMillisPartError>
    where
        H: BlockHeader,
    {
        BaseHeader::validate_timestamp_millis_part(timestamp_millis_part)?;

        let timestamp_ms = inner
            .timestamp()
            .checked_mul(u64::from(TIMESTAMP_MILLIS_PER_SECOND))
            .and_then(|timestamp| timestamp.checked_add(u64::from(timestamp_millis_part)))
            .ok_or(TimestampMillisPartError::TimestampOverflow)?;

        Ok(Self {
            inner,
            timestamp_ms: Some(timestamp_ms),
            timestamp_millis_part: Some(timestamp_millis_part),
        })
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

#[cfg(test)]
mod tests {
    use alloy_consensus::{BlockHeader, Header as ConsensusHeader};
    use alloy_primitives::U256;
    use alloy_rpc_types_eth::Header;
    use serde_json::json;

    use super::BaseHeaderResponse;

    #[test]
    fn base_header_response_serializes_timestamp_millis_fields() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let response = BaseHeaderResponse::with_timestamp_millis_part(inner, 200).unwrap();
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["timestamp"], json!("0x2a"));
        assert_eq!(value["timestampMs"], json!("0xa4d8"));
        assert_eq!(value["timestampMillisPart"], json!("0xc8"));
    }

    #[test]
    fn base_header_response_omits_timestamp_millis_fields_when_absent() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let value = serde_json::to_value(BaseHeaderResponse::new(inner)).unwrap();

        assert!(value.get("timestampMs").is_none());
        assert!(value.get("timestampMillisPart").is_none());
    }

    #[test]
    fn base_header_response_rejects_invalid_timestamp_millis_part() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let error = BaseHeaderResponse::with_timestamp_millis_part(inner, 100).unwrap_err();

        assert_eq!(error, base_common_consensus::TimestampMillisPartError::InvalidPart(100));
    }

    #[test]
    fn base_header_response_keeps_standard_timestamp_seconds() {
        let inner = Header::new(ConsensusHeader { timestamp: 42, ..Default::default() });
        let response = BaseHeaderResponse::with_timestamp_millis_part(inner, 200).unwrap();

        assert_eq!(response.timestamp(), 42);
        assert_ne!(U256::from(response.timestamp()), U256::from(42_200_u64));
    }
}
