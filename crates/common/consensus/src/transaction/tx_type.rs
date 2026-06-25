//! Contains the transaction type identifier for Base chains.

use core::fmt::Display;

use alloy_consensus::InMemorySize;

use crate::transaction::envelope::OpTxType;

/// Identifier for a deposit transaction
pub const DEPOSIT_TX_TYPE_ID: u8 = 126; // 0x7E

/// Identifier for an [EIP-8130] Account Abstraction transaction.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
pub const EIP8130_TX_TYPE_ID: u8 = 123; // 0x7B

/// Canonical user-facing rejection message for EIP-8130 transactions submitted before Cobalt.
///
/// Used by `base-execution-rpc` when an EIP-8130 transaction is submitted before
/// the Cobalt fork is active at the latest block timestamp.
pub const EIP8130_REJECTION_MSG: &str = "EIP-8130 (account abstraction) transactions are gated behind Cobalt; \
     eth_sendRawTransaction does not accept transaction type 0x7B before Cobalt";

#[allow(clippy::derivable_impls)]
impl Default for OpTxType {
    fn default() -> Self {
        Self::Legacy
    }
}

impl Display for OpTxType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Legacy => write!(f, "legacy"),
            Self::Eip2930 => write!(f, "eip2930"),
            Self::Eip1559 => write!(f, "eip1559"),
            Self::Eip7702 => write!(f, "eip7702"),
            Self::Deposit => write!(f, "deposit"),
            Self::Eip8130 => write!(f, "eip8130"),
        }
    }
}

impl OpTxType {
    /// List of all variants.
    pub const ALL: [Self; 6] =
        [Self::Legacy, Self::Eip2930, Self::Eip1559, Self::Eip7702, Self::Eip8130, Self::Deposit];

    /// Returns `true` if the type is [`OpTxType::Deposit`].
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit)
    }

    /// Returns `true` if the type is [`OpTxType::Eip8130`].
    pub const fn is_eip8130(&self) -> bool {
        matches!(self, Self::Eip8130)
    }
}

impl InMemorySize for OpTxType {
    #[inline]
    fn size(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use alloy_rlp::{Decodable, Encodable};

    use super::*;

    #[test]
    fn test_all_tx_types() {
        assert_eq!(OpTxType::ALL.len(), 6);
        let all = vec![
            OpTxType::Legacy,
            OpTxType::Eip2930,
            OpTxType::Eip1559,
            OpTxType::Eip7702,
            OpTxType::Eip8130,
            OpTxType::Deposit,
        ];
        assert_eq!(OpTxType::ALL.to_vec(), all);
    }

    #[test]
    fn tx_type_roundtrip() {
        for &tx_type in &OpTxType::ALL {
            let mut buf = Vec::new();
            tx_type.encode(&mut buf);
            let decoded = OpTxType::decode(&mut &buf[..]).unwrap();
            assert_eq!(tx_type, decoded);
        }
    }
}
