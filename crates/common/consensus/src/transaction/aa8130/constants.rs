//! Constants for the [EIP-8130] Account Abstraction transaction type.
//!
//! [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130

use alloy_primitives::U256;

/// Container for [EIP-8130] protocol constants.
///
/// All constants are exposed as associated `pub const` items so the public API
/// is type-anchored (per repo convention: "the public API exports types, not loose
/// functions").
///
/// Spec status (as of writing): EIP-8130 is in Draft. Several numeric constants
/// are marked TBD in the spec; concrete values used here are project choices
/// that can be renumbered when the spec finalizes.
/// for rationale.
///
/// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
#[derive(Debug)]
pub struct Aa8130Constants;

impl Aa8130Constants {
    /// [EIP-2718] transaction type byte for AA transactions (`AA_TX_TYPE`).
    ///
    /// Spec value: TBD. We use `0x7D`, picked to live in the high "OP-style"
    /// type-byte range adjacent to (but distinct from) the deposit type `0x7E`,
    /// and to be easy to renumber once the EIP finalizes.
    ///
    /// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
    pub const AA_TX_TYPE: u8 = 0x7D;

    /// Magic prefix byte for payer signature domain separation (`AA_PAYER_TYPE`).
    ///
    /// Used in the payer signature preimage:
    /// `keccak256(AA_PAYER_TYPE || rlp([...fields through calls...]))`.
    ///
    /// Spec value: TBD. We use `0xFA`, distinct from any registered EIP-2718
    /// transaction type byte to prevent cross-domain reuse.
    pub const AA_PAYER_TYPE: u8 = 0xFA;

    /// Base intrinsic gas cost for any AA transaction (`AA_BASE_COST`).
    pub const AA_BASE_COST: u64 = 15_000;

    /// Sentinel `nonce_key` value selecting nonce-free mode (`NONCE_KEY_MAX`).
    ///
    /// When `nonce_key == NONCE_KEY_MAX`, no nonce state is read or written
    /// and replay protection relies on `expiry` (which must be non-zero).
    pub const NONCE_KEY_MAX: U256 = U256::MAX;

    /// Owner scope bit: ERC-1271 `verifySignature()` context.
    pub const SCOPE_SIGNATURE: u8 = 0x01;

    /// Owner scope bit: `sender_auth` validation context.
    pub const SCOPE_SENDER: u8 = 0x02;

    /// Owner scope bit: `payer_auth` validation context.
    pub const SCOPE_PAYER: u8 = 0x04;

    /// Owner scope bit: config change `auth` context.
    pub const SCOPE_CONFIG: u8 = 0x08;

    /// Unrestricted scope value (owner is valid in all contexts).
    pub const SCOPE_UNRESTRICTED: u8 = 0x00;

    /// [EIP-7702]-style delegation indicator code prefix.
    ///
    /// A delegated account's code is exactly `DELEGATION_INDICATOR_PREFIX || target`
    /// where `target` is a 20-byte address.
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    pub const DELEGATION_INDICATOR_PREFIX: [u8; 3] = [0xef, 0x01, 0x00];

    /// Total length in bytes of an [EIP-7702] delegation indicator
    /// (`DELEGATION_INDICATOR_PREFIX || target`).
    ///
    /// [EIP-7702]: https://eips.ethereum.org/EIPS/eip-7702
    pub const DELEGATION_INDICATOR_SIZE: usize = 23;

    /// `account_changes` entry type byte: account creation.
    pub const ACCOUNT_CHANGE_TYPE_CREATE: u8 = 0x00;

    /// `account_changes` entry type byte: owner config change.
    pub const ACCOUNT_CHANGE_TYPE_CONFIG: u8 = 0x01;

    /// `account_changes` entry type byte: code delegation.
    pub const ACCOUNT_CHANGE_TYPE_DELEGATION: u8 = 0x02;

    /// `owner_change` operation byte: authorize a new owner.
    pub const OWNER_CHANGE_AUTHORIZE: u8 = 0x01;

    /// `owner_change` operation byte: revoke an existing owner.
    pub const OWNER_CHANGE_REVOKE: u8 = 0x02;
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_TX_TYPE: u8 = 0x00;
    const EIP2930_TX_TYPE: u8 = 0x01;
    const EIP1559_TX_TYPE: u8 = 0x02;
    const EIP7702_TX_TYPE: u8 = 0x04;
    const DEPOSIT_TX_TYPE: u8 = 0x7E;

    #[test]
    fn type_bytes_are_distinct() {
        assert_ne!(Aa8130Constants::AA_TX_TYPE, Aa8130Constants::AA_PAYER_TYPE);
        assert_ne!(Aa8130Constants::AA_TX_TYPE, LEGACY_TX_TYPE);
        assert_ne!(Aa8130Constants::AA_TX_TYPE, EIP2930_TX_TYPE);
        assert_ne!(Aa8130Constants::AA_TX_TYPE, EIP1559_TX_TYPE);
        assert_ne!(Aa8130Constants::AA_TX_TYPE, EIP7702_TX_TYPE);
        assert_ne!(Aa8130Constants::AA_TX_TYPE, DEPOSIT_TX_TYPE);
    }

    #[test]
    fn scope_bits_are_orthogonal() {
        let bits = [
            Aa8130Constants::SCOPE_SIGNATURE,
            Aa8130Constants::SCOPE_SENDER,
            Aa8130Constants::SCOPE_PAYER,
            Aa8130Constants::SCOPE_CONFIG,
        ];
        let mut acc: u8 = 0;
        for b in bits {
            assert_eq!(b.count_ones(), 1, "scope bit must be a single bit");
            assert_eq!(acc & b, 0, "scope bits must be orthogonal");
            acc |= b;
        }
        assert_eq!(Aa8130Constants::SCOPE_UNRESTRICTED, 0);
    }

    #[test]
    fn delegation_indicator_size_matches_prefix_plus_address() {
        assert_eq!(
            Aa8130Constants::DELEGATION_INDICATOR_SIZE,
            Aa8130Constants::DELEGATION_INDICATOR_PREFIX.len() + 20
        );
    }
}
