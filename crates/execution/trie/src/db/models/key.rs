//! V2 proof storage table keys.

use alloy_primitives::{B256, BlockNumber};
use reth_db::{
    DatabaseError,
    table::{Decode, Encode},
};
use serde::{Deserialize, Serialize};

pub(super) const NIBBLE_SUBKEY_LEN: usize = 65;

/// Key for V2 storage changesets grouped by block and hashed address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct BlockNumberHashedAddress(pub (BlockNumber, B256));

impl Encode for BlockNumberHashedAddress {
    type Encoded = [u8; 40];

    fn encode(self) -> Self::Encoded {
        let mut buf = [0u8; 40];
        buf[..8].copy_from_slice(&self.0.0.to_be_bytes());
        buf[8..].copy_from_slice(self.0.1.as_slice());
        buf
    }
}

impl Decode for BlockNumberHashedAddress {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != 40 {
            return Err(DatabaseError::Decode);
        }
        let block_number =
            u64::from_be_bytes(value[..8].try_into().map_err(|_| DatabaseError::Decode)?);
        let hashed_address = B256::from_slice(&value[8..]);
        Ok(Self((block_number, hashed_address)))
    }
}
