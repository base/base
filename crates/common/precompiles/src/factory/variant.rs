//! B-20 token variant address derivation.

use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::SolValue;

/// B-20 token variant encoded in the token address prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TokenVariant {
    /// B-20 token.
    B20 = 1,
    /// Stablecoin B-20 token.
    Stablecoin = 2,
    /// Security B-20 token.
    Security = 3,
}

impl TokenVariant {
    /// First byte of every B-20 address.
    pub const PREFIX_BYTE: u8 = 0xb2;

    /// Variant discriminant returned by `getTokenVariant` when address has no B-20 prefix.
    pub const NONE_DISCRIMINANT: u8 = 0;

    /// Variant discriminant for default B-20 tokens.
    pub const B20_DISCRIMINANT: u8 = Self::B20 as u8;

    /// Variant discriminant for stablecoin B-20 tokens.
    pub const STABLECOIN_DISCRIMINANT: u8 = Self::Stablecoin as u8;

    /// Variant discriminant for security B-20 tokens.
    pub const SECURITY_DISCRIMINANT: u8 = Self::Security as u8;

    /// Returns the supported token variant for `variant`, if any.
    pub const fn from_discriminant(variant: u8) -> Option<Self> {
        match variant {
            Self::B20_DISCRIMINANT => Some(Self::B20),
            Self::STABLECOIN_DISCRIMINANT => Some(Self::Stablecoin),
            Self::SECURITY_DISCRIMINANT => Some(Self::Security),
            _ => None,
        }
    }

    /// Returns whether `variant` is supported by this factory.
    pub const fn is_supported_discriminant(variant: u8) -> bool {
        Self::from_discriminant(variant).is_some()
    }

    /// Returns the token variant encoded in `address`, if it has a supported B-20 prefix.
    pub fn from_address(address: Address) -> Option<Self> {
        let bytes = address.as_slice();
        if bytes[0] != Self::PREFIX_BYTE || bytes[1..10] != [0u8; 9] {
            return None;
        }

        Self::from_discriminant(bytes[10])
    }

    /// Returns whether `address` has the structural B-20 token prefix.
    ///
    /// This intentionally does not validate the encoded variant discriminant.
    pub fn has_b20_prefix(address: Address) -> bool {
        let bytes = address.as_slice();
        bytes[0] == Self::PREFIX_BYTE && bytes[1..10] == [0u8; 9]
    }

    /// Returns this variant's ABI discriminant.
    pub const fn discriminant(self) -> u8 {
        self as u8
    }

    /// Builds this variant's B-20 address prefix for `decimals`.
    pub const fn address_prefix(self, decimals: u8) -> [u8; 12] {
        [Self::PREFIX_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, self.discriminant(), decimals]
    }

    /// Computes this variant's deterministic token address for `creator`, `decimals`, and `salt`.
    ///
    /// Returns the address and the lower 8 bytes of the hash as a `u64`.
    pub fn compute_address(self, creator: Address, decimals: u8, salt: B256) -> (Address, u64) {
        let hash = keccak256((creator, salt).abi_encode());

        let mut lower_bytes_buf = [0u8; 8];
        lower_bytes_buf.copy_from_slice(&hash[..8]);
        let lower_bytes = u64::from_be_bytes(lower_bytes_buf);

        let mut addr_bytes = [0u8; 20];
        addr_bytes[..12].copy_from_slice(&self.address_prefix(decimals));
        addr_bytes[12..].copy_from_slice(&hash[..8]);

        (Address::from(addr_bytes), lower_bytes)
    }

    /// Computes a deterministic B-20 token address for an ABI discriminant.
    pub fn compute_address_for_discriminant(
        creator: Address,
        variant: u8,
        decimals: u8,
        salt: B256,
    ) -> (Address, u64) {
        let hash = keccak256((creator, salt).abi_encode());

        let mut lower_bytes_buf = [0u8; 8];
        lower_bytes_buf.copy_from_slice(&hash[..8]);
        let lower_bytes = u64::from_be_bytes(lower_bytes_buf);

        let mut addr_bytes = [0u8; 20];
        addr_bytes[0] = Self::PREFIX_BYTE;
        addr_bytes[10] = variant;
        addr_bytes[11] = decimals;
        addr_bytes[12..].copy_from_slice(&hash[..8]);

        (Address::from(addr_bytes), lower_bytes)
    }

    /// Returns `true` when `address` has a supported B-20 token variant prefix.
    pub fn is_b20_address(address: Address) -> bool {
        Self::from_address(address).is_some()
    }

    /// Returns the decimals encoded in `address` when it has a supported B-20 prefix.
    pub fn decimals_of(address: Address) -> Option<u8> {
        Self::from_address(address)?;
        Some(address.as_slice()[11])
    }
}
