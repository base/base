//! Protocol constants for the development-only native BLS transaction type.

use alloy_primitives::{Address, B256, address, b256};

use crate::{BLS_TRANSFER_TX_TYPE_ID, BLS_TX_TYPE_ID};

/// Protocol constants for the development-only native BLS transaction type ([`crate::TxBls`]).
///
/// The type is a prototype that only the development chain ([`Self::CHAIN_ID`]) may execute or
/// admit. Envelope parsing is chain-agnostic; execution and admission own the chain gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlsConstants;

impl BlsConstants {
    /// EIP-2718 transaction type byte.
    pub const TX_TYPE: u8 = BLS_TX_TYPE_ID;

    /// EIP-2718 transaction type byte of a native BLS transfer ([`crate::TxBlsTransfer`]).
    pub const TRANSFER_TX_TYPE: u8 = BLS_TRANSFER_TX_TYPE_ID;

    /// Fixed gas limit of every [`crate::TxBlsTransfer`], equal to a plain ETH transfer.
    pub const TRANSFER_GAS_LIMIT: u64 = 21_000;

    /// The only chain ID on which the native BLS transaction type may execute.
    pub const CHAIN_ID: u64 = 84_538_453;

    /// Native registry address every [`crate::TxBls`] projects as its call target.
    /// Outside the `0x4200…` predeploy range, which genesis fills with proxy code.
    pub const REGISTRY_ADDRESS: Address = address!("0x43000000000000000000000000000000000000b1");

    /// UTF-8 domain tag hashed into [`Self::DOMAIN`].
    pub const DOMAIN_TAG: &'static str = "base-native-bls:v1:eth-transfer";

    /// `keccak256(DOMAIN_TAG)`, the first 32 bytes of every transfer signing digest, both for
    /// [`crate::BlsTransfer`] batch records and for [`crate::TxBlsTransfer`].
    pub const DOMAIN: B256 =
        b256!("0x835368e0f686eb6dcb505d223e8e32872c58ba04ef2d946a7ae78ab07a13d651");

    /// CFRG ciphersuite used to sign inner transfer digests (signatures in G1, keys in G2).
    pub const SIGNATURE_DST: &'static [u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";

    /// CFRG ciphersuite used for the proof of possession over the compressed public key.
    pub const POP_DST: &'static [u8] = b"BLS_POP_BLS12381G1_XMD:SHA-256_SSWU_RO_POP_";

    /// Payload kind byte for a public key registration.
    pub const KIND_REGISTRATION: u8 = 0;

    /// Payload kind byte for an aggregated transfer batch.
    pub const KIND_BATCH: u8 = 1;

    /// Length of a compressed G2 public key.
    pub const PUBKEY_LEN: usize = 96;

    /// Length of a compressed G1 signature (proof of possession or aggregate signature).
    pub const SIGNATURE_LEN: usize = 48;

    /// Length of one encoded transfer record: `sender20 | nonce8 | recipient20 | value12`.
    pub const TRANSFER_LEN: usize = 60;

    /// Maximum number of transfer records in one batch.
    pub const MAX_TRANSFERS: usize = 32;

    /// Exact input length of a registration payload: `kind | pubkey96 | pop48`.
    pub const REGISTRATION_INPUT_LEN: usize = 1 + Self::PUBKEY_LEN + Self::SIGNATURE_LEN;

    /// Big-endian BLS12-381 base field modulus `p`. Every canonical compressed coordinate is
    /// strictly below it.
    pub const FIELD_MODULUS: [u8; 48] = alloy_primitives::hex!(
        "1a0111ea397fe69a4b1ba7b6434bacd764774b84f38512bf6730d2a0f6b0f6241eabfffeb153ffffb9feffffffffaaab"
    );

    /// Returns the exact input length of a batch payload carrying `count` transfers.
    pub const fn batch_input_len(count: usize) -> usize {
        2 + count * Self::TRANSFER_LEN + Self::SIGNATURE_LEN
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, keccak256};

    use super::*;

    #[test]
    fn domain_is_keccak_of_tag() {
        assert_eq!(BlsConstants::DOMAIN, keccak256(BlsConstants::DOMAIN_TAG.as_bytes()));
    }

    #[test]
    fn field_modulus_matches_bls12_381() {
        // p = (x - 1)^2 (x^4 - x^2 + 1) / 3 + x with x = -0xd201000000010000.
        let x = U256::from(0xd201_0000_0001_0000_u64);
        let x2 = x * x;
        let x4 = x2 * x2;
        // With x negative, (x - 1)^2 = (|x| + 1)^2 and the trailing `+ x` subtracts |x|.
        let p = alloy_primitives::U512::from((x + U256::from(1)) * (x + U256::from(1)))
            * alloy_primitives::U512::from(x4 - x2 + U256::from(1))
            / alloy_primitives::U512::from(3)
            - alloy_primitives::U512::from(x);
        assert_eq!(&p.to_be_bytes::<64>()[16..], &BlsConstants::FIELD_MODULUS);
    }
}
