//! Contains bedrock-specific L1 block info types.

use alloc::{format, string::ToString, vec::Vec};
use alloy_primitives::{Address, Bytes, B256, U256};

use crate::DecodeError;

/// Represents the fields within a Bedrock L1 block info transaction.
///
/// Bedrock Binary Format
// +---------+--------------------------+
// | Bytes   | Field                    |
// +---------+--------------------------+
// | 4       | Function signature       |
// | 32      | Number                   |
// | 32      | Time                     |
// | 32      | BaseFee                  |
// | 32      | BlockHash                |
// | 32      | SequenceNumber           |
// | 32      | BatcherHash              |
// | 32      | L1FeeOverhead            |
// | 32      | L1FeeScalar              |
// +---------+--------------------------+
#[derive(Debug, Clone, Hash, Eq, PartialEq, Default, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct L1BlockInfoBedrock {
    /// The current L1 origin block number
    pub number: u64,
    /// The current L1 origin block's timestamp
    pub time: u64,
    /// The current L1 origin block's basefee
    pub base_fee: u64,
    /// The current L1 origin block's hash
    pub block_hash: B256,
    /// The current sequence number
    pub sequence_number: u64,
    /// The address of the batch submitter
    pub batcher_address: Address,
    /// The fee overhead for L1 data
    pub l1_fee_overhead: U256,
    /// The fee scalar for L1 data
    pub l1_fee_scalar: U256,
}

impl L1BlockInfoBedrock {
    /// The length of an L1 info transaction in Bedrock.
    pub const L1_INFO_TX_LEN: usize = 4 + 32 * 8;

    /// The 4 byte selector of the
    /// "setL1BlockValues(uint64,uint64,uint256,bytes32,uint64,bytes32,uint256,uint256)" function
    pub const L1_INFO_TX_SELECTOR: [u8; 4] = [0x01, 0x5d, 0x8e, 0xb9];

    /// Encodes the [L1BlockInfoBedrock] object into Ethereum transaction calldata.
    pub fn encode_calldata(&self) -> Bytes {
        let mut buf = Vec::with_capacity(Self::L1_INFO_TX_LEN);
        buf.extend_from_slice(Self::L1_INFO_TX_SELECTOR.as_ref());
        buf.extend_from_slice(U256::from(self.number).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(U256::from(self.time).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(U256::from(self.base_fee).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.block_hash.as_slice());
        buf.extend_from_slice(U256::from(self.sequence_number).to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.batcher_address.into_word().as_slice());
        buf.extend_from_slice(self.l1_fee_overhead.to_be_bytes::<32>().as_slice());
        buf.extend_from_slice(self.l1_fee_scalar.to_be_bytes::<32>().as_slice());
        buf.into()
    }

    /// Decodes the [L1BlockInfoBedrock] object from ethereum transaction calldata.
    pub fn decode_calldata(r: &[u8]) -> Result<Self, DecodeError> {
        if r.len() != Self::L1_INFO_TX_LEN {
            return Err(DecodeError::InvalidLength(format!(
                "Invalid calldata length for Bedrock L1 info transaction, expected {}, got {}",
                Self::L1_INFO_TX_LEN,
                r.len()
            )));
        }

        let number = u64::from_be_bytes(
            r[28..36]
                .try_into()
                .map_err(|_| DecodeError::ParseError("Conversion error for number".to_string()))?,
        );
        let time = u64::from_be_bytes(
            r[60..68]
                .try_into()
                .map_err(|_| DecodeError::ParseError("Conversion error for time".to_string()))?,
        );
        let base_fee =
            u64::from_be_bytes(r[92..100].try_into().map_err(|_| {
                DecodeError::ParseError("Conversion error for base fee".to_string())
            })?);
        let block_hash = B256::from_slice(r[100..132].as_ref());
        let sequence_number = u64::from_be_bytes(r[156..164].try_into().map_err(|_| {
            DecodeError::ParseError("Conversion error for sequence number".to_string())
        })?);
        let batcher_address = Address::from_slice(r[176..196].as_ref());
        let l1_fee_overhead = U256::from_be_slice(r[196..228].as_ref());
        let l1_fee_scalar = U256::from_be_slice(r[228..260].as_ref());

        Ok(Self {
            number,
            time,
            base_fee,
            block_hash,
            sequence_number,
            batcher_address,
            l1_fee_overhead,
            l1_fee_scalar,
        })
    }
}
