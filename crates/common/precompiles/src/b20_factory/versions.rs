//! Version manager for the B-20 token factory precompile.
//!
//! This module is the single owner of fork routing: which version is active at a given hardfork
//! ([`FactoryVersions::from_base_upgrade`]), which concrete implementation backs a version
//! ([`FactoryVersion::implementation`]), and which wire surface it decodes against
//! ([`FactoryVersion::abi`]). Centralizing fork routing here keeps hardfork logic auditable and off
//! the execution path, and lets the dispatcher route calls without ever matching on the version
//! itself.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_common_genesis::BaseUpgrade;
use base_precompile_storage::{BasePrecompileError, Result};

use crate::{Factory, FactoryV1, IB20Factory, IB20FactoryV1};

/// An activated version of the B-20 token factory precompile logic.
///
/// Each variant maps to an immutable implementation via [`Self::implementation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryVersion {
    /// Introduced at Beryl, the factory's activation fork.
    V1,
}

impl FactoryVersion {
    /// Returns the immutable logic implementation for this version.
    pub fn implementation<'l>(self) -> &'l dyn Factory {
        static V1: FactoryV1 = FactoryV1;
        match self {
            Self::V1 => &V1,
        }
    }

    /// Returns the wire (ABI) surface frozen for this version.
    pub const fn abi(self) -> FactoryAbi {
        match self {
            Self::V1 => FactoryAbi::V1,
        }
    }
}

/// A frozen wire (ABI) surface of the `B20Factory` precompile. Reached only through
/// [`FactoryVersion::abi`].
///
/// Deliberately has no `from_base_upgrade`: a second resolver over the same fork ladder would be
/// two maps that must agree, which is the failure mode this module exists to prevent. With one
/// resolver, adding a version is a compile error in both [`FactoryVersion::implementation`] and
/// [`FactoryVersion::abi`] until someone fills them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryAbi {
    /// Wire surface activated at Beryl.
    V1,
}

impl FactoryAbi {
    /// Returns whether `selector` was dialable on this wire surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20FactoryV1::IB20FactoryCalls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this wire surface via alloy's `abi_decode_validate`, discarding
    /// the decoded call.
    pub fn abi_decode_validate(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => IB20FactoryV1::IB20FactoryCalls::abi_decode_validate(calldata).map(|_| ()),
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes `calldata` into a routable call, gated on this wire surface.
    ///
    /// Always the *validating* decode. `dispatch` unwraps `B20Variant::from_abi` on the decoded
    /// call with `expect`, so a non-validating decode here would turn an out-of-range variant byte
    /// from a revert into a node panic.
    pub fn decode(self, calldata: &[u8]) -> Result<IB20Factory::IB20FactoryCalls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        match self {
            // Canonical aliases the V1 surface, so the frozen decode already yields the canonical
            // call type. A later surface adds its own arm, re-decoding against canonical.
            Self::V1 => {
                IB20FactoryV1::IB20FactoryCalls::abi_decode_validate(calldata).map_err(|error| {
                    BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
                })
            }
        }
    }
}

/// Resolver that selects the factory version active at a given hardfork.
///
/// The version is resolved once per call from the block's active upgrade; there
/// is only ever one active version at a time.
#[derive(Debug, Default, Clone, Copy)]
pub struct FactoryVersions;

impl FactoryVersions {
    /// Returns the version active at `upgrade`, or `None` before the introduction
    /// fork (Beryl), where the factory precompile is not installed at all.
    pub fn from_base_upgrade(upgrade: BaseUpgrade) -> Option<FactoryVersion> {
        if upgrade >= BaseUpgrade::Beryl { Some(FactoryVersion::V1) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_sol_types::{SolCall, SolInterface};
    use base_common_genesis::BaseUpgrade;
    use base_precompile_storage::BasePrecompileError;

    use crate::{FactoryAbi, FactoryVersion, FactoryVersions, IB20Factory, IB20FactoryV1};

    #[test]
    fn resolves_none_before_beryl() {
        assert_eq!(FactoryVersions::from_base_upgrade(BaseUpgrade::Azul), None);
    }

    #[test]
    fn resolves_v1_from_beryl() {
        assert_eq!(
            FactoryVersions::from_base_upgrade(BaseUpgrade::Beryl),
            Some(FactoryVersion::V1)
        );
    }

    /// The logic axis and the wire axis meet only here. Driven from the fork ladder so the whole
    /// chain (upgrade -> version -> surface) is pinned, not just the inner lookup.
    #[test]
    fn each_fork_resolves_to_its_wire_surface() {
        assert_eq!(FactoryVersion::V1.abi(), FactoryAbi::V1);

        let beryl = FactoryVersions::from_base_upgrade(BaseUpgrade::Beryl).unwrap();
        let cobalt = FactoryVersions::from_base_upgrade(BaseUpgrade::Cobalt).unwrap();
        assert_eq!(beryl.abi(), FactoryAbi::V1);
        assert_eq!(cobalt.abi(), FactoryAbi::V1);
    }

    /// `SolInterface::NAME` lands in consensus data: the short-calldata branch of
    /// `abi_decode_validate` builds its error from it, and `AbiDecodeFailed` puts that string on
    /// the wire. Renaming the frozen interface would change historical revert payloads.
    #[test]
    fn surface_interface_name_is_frozen() {
        assert_eq!(IB20FactoryV1::IB20FactoryCalls::NAME, "IB20FactoryCalls");
    }

    fn valid_calldata() -> alloc::vec::Vec<u8> {
        IB20Factory::isB20Call { token: Address::repeat_byte(0x42) }.abi_encode()
    }

    /// `FactoryAbi::decode` replaced `decode_precompile_call!` on the execution path. The macro
    /// decodes first and consults `valid_selector` only on the error path; `decode` checks the
    /// selector first. The orderings must be observationally identical, because every one of these
    /// outcomes is consensus data.
    #[test]
    fn decode_preserves_legacy_macro_semantics() {
        // Short calldata: no selector to report.
        for truncated in [[].as_slice(), &[0x01], &[0x01, 0x02, 0x03]] {
            assert_eq!(
                FactoryAbi::V1.decode(truncated).unwrap_err(),
                BasePrecompileError::UnknownFunctionSelector([0u8; 4])
            );
        }

        // Unknown selector, whatever the payload length.
        let unknown = [0xde, 0xad, 0xbe, 0xef];
        assert_eq!(
            FactoryAbi::V1.decode(&unknown).unwrap_err(),
            BasePrecompileError::UnknownFunctionSelector(unknown)
        );
        let mut unknown_padded = unknown.to_vec();
        unknown_padded.extend_from_slice(&[0u8; 32]);
        assert_eq!(
            FactoryAbi::V1.decode(&unknown_padded).unwrap_err(),
            BasePrecompileError::UnknownFunctionSelector(unknown)
        );

        // Known selector, calldata truncated below MIN_DATA_LENGTH: AbiDecodeFailed, not unknown.
        let selector = IB20Factory::isB20Call::SELECTOR;
        let err = FactoryAbi::V1.decode(&selector).unwrap_err();
        let BasePrecompileError::AbiDecodeFailed { selector: got, error } = err else {
            panic!("truncated args on a known selector must be AbiDecodeFailed, got {err:?}");
        };
        assert_eq!(got, selector);
        // The interface name is embedded in the short-calldata error, and thus on the wire.
        assert!(error.contains("IB20FactoryCalls"), "unexpected decoder message: {error}");

        // Known selector, dirty high-order padding: rejected rather than silently truncated.
        let mut dirty = valid_calldata();
        dirty[4] = 0xff;
        assert!(matches!(
            FactoryAbi::V1.decode(&dirty).unwrap_err(),
            BasePrecompileError::AbiDecodeFailed { .. }
        ));

        // Well-formed call decodes.
        assert!(FactoryAbi::V1.decode(&valid_calldata()).is_ok());
    }

    /// Every selector on the frozen surface must be dialable through the axis, and nothing else.
    #[test]
    fn valid_selector_matches_the_frozen_surface() {
        for selector in IB20FactoryV1::IB20FactoryCalls::selectors() {
            assert!(FactoryAbi::V1.valid_selector(selector));
        }
        assert!(!FactoryAbi::V1.valid_selector([0xde, 0xad, 0xbe, 0xef]));
    }
}
