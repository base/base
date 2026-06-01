//! B-20 token variant address derivation.

use alloy_primitives::{Address, B256, keccak256};
use alloy_sol_types::SolValue;

use crate::{ActivationFeature, IB20Factory};

/// B-20 token variant encoded in token address byte `[10]`.
///
/// Discriminant values match the `B20Variant` ABI enum ordinals directly
/// (DEFAULT=0, STABLECOIN=1, SECURITY=2), so `uint8(variant)` in Solidity
/// equals the byte written at address position `[10]` with no offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum B20Variant {
    /// Default B-20 token.
    B20 = 0,
    /// Stablecoin B-20 token.
    Stablecoin = 1,
    /// Security B-20 token.
    Security = 2,
}

impl B20Variant {
    /// First byte of every B-20 address.
    pub const PREFIX_BYTE: u8 = 0xb2;

    /// Variant discriminant for default B-20 tokens.
    pub const B20_DISCRIMINANT: u8 = Self::B20 as u8;

    /// Variant discriminant for stablecoin B-20 tokens.
    pub const STABLECOIN_DISCRIMINANT: u8 = Self::Stablecoin as u8;

    /// Variant discriminant for security B-20 tokens.
    pub const SECURITY_DISCRIMINANT: u8 = Self::Security as u8;

    /// Returns the currently supported creation-parameter version for this variant.
    ///
    /// Each variant owns its version independently so that one variant advancing to v2
    /// does not affect the others.
    pub const fn supported_version(self) -> u8 {
        match self {
            Self::B20 | Self::Stablecoin | Self::Security => 1,
        }
    }

    /// Returns the supported token variant for `variant`, if any.
    pub const fn from_discriminant(variant: u8) -> Option<Self> {
        match variant {
            Self::B20_DISCRIMINANT => Some(Self::B20),
            Self::STABLECOIN_DISCRIMINANT => Some(Self::Stablecoin),
            Self::SECURITY_DISCRIMINANT => Some(Self::Security),
            _ => None,
        }
    }

    /// Returns the supported token variant for an ABI enum value, or `None` for unknown variants.
    pub const fn from_abi(variant: IB20Factory::B20Variant) -> Option<Self> {
        match variant {
            IB20Factory::B20Variant::DEFAULT => Some(Self::B20),
            IB20Factory::B20Variant::STABLECOIN => Some(Self::Stablecoin),
            IB20Factory::B20Variant::SECURITY => Some(Self::Security),
            IB20Factory::B20Variant::__Invalid => None,
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

    /// Returns this variant as the generated ABI enum.
    pub const fn abi(self) -> IB20Factory::B20Variant {
        match self {
            Self::B20 => IB20Factory::B20Variant::DEFAULT,
            Self::Stablecoin => IB20Factory::B20Variant::STABLECOIN,
            Self::Security => IB20Factory::B20Variant::SECURITY,
        }
    }

    /// Returns this variant's fixed decimal precision.
    pub const fn decimals(self) -> u8 {
        match self {
            Self::B20 => 18,
            Self::Stablecoin | Self::Security => 6,
        }
    }

    /// Returns the activation feature that controls creation of this variant.
    pub const fn activation_feature(self) -> ActivationFeature {
        match self {
            Self::B20 => ActivationFeature::B20Token,
            Self::Stablecoin => ActivationFeature::B20Stablecoin,
            Self::Security => ActivationFeature::B20Security,
        }
    }

    /// Builds this variant's B-20 address prefix.
    pub const fn address_prefix(self) -> [u8; 11] {
        [Self::PREFIX_BYTE, 0, 0, 0, 0, 0, 0, 0, 0, 0, self.discriminant()]
    }

    /// Computes this variant's deterministic token address for `creator` and `salt`.
    ///
    /// Returns the address and the 9-byte hash tail embedded in the address.
    pub fn compute_address(self, creator: Address, salt: B256) -> (Address, [u8; 9]) {
        let hash = keccak256((creator, salt).abi_encode());

        let mut tail = [0u8; 9];
        tail.copy_from_slice(&hash[..9]);

        let mut addr_bytes = [0u8; 20];
        addr_bytes[..11].copy_from_slice(&self.address_prefix());
        addr_bytes[11..].copy_from_slice(&tail);

        (Address::from(addr_bytes), tail)
    }

    /// Computes a deterministic B-20 token address for an ABI discriminant.
    pub fn compute_address_for_discriminant(
        creator: Address,
        variant: u8,
        salt: B256,
    ) -> (Address, [u8; 9]) {
        let hash = keccak256((creator, salt).abi_encode());

        let mut tail = [0u8; 9];
        tail.copy_from_slice(&hash[..9]);

        let mut addr_bytes = [0u8; 20];
        addr_bytes[0] = Self::PREFIX_BYTE;
        addr_bytes[10] = variant;
        addr_bytes[11..].copy_from_slice(&tail);

        (Address::from(addr_bytes), tail)
    }

    /// Returns `true` when `address` has a supported B-20 token variant prefix.
    pub fn is_b20_address(address: Address) -> bool {
        Self::from_address(address).is_some()
    }

    /// Returns the variant discriminant encoded in `address`, if supported.
    pub fn variant_of(address: Address) -> Option<u8> {
        Self::from_address(address)?;
        Some(address.as_slice()[10])
    }

    /// Returns the fixed decimals for the variant encoded in `address`.
    pub fn decimals_of(address: Address) -> Option<u8> {
        Some(Self::from_address(address)?.decimals())
    }
}
