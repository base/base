//! Wire (ABI) surfaces for the shared B-20 token interface, one per hardfork that moved them.
//!
//! The latest surface is always named `IB20` in its `vN` module, then re-exported here as both
//! [`IB20`] (canonical) and `IB20VN`. Older forks keep the same Rust name inside their module so
//! truncated-calldata revert bytes stay stable, and are re-exported as [`IB20V1`], [`IB20V2`], etc.
//!
//! Token variants compose this surface with their own extension ABI. Asset does so via
//! [`crate::AssetAbiPair`]; stablecoin still decodes against canonical [`IB20`] directly until it
//! adopts the same composite shape.
//!
//! A fork that changes the common wire adds `abi/vN.rs` and retargets the canonical alias below.
//! Token versions then map onto the new [`B20Abi`] variant through their own `abi()` join — there
//! is no independent `B20Abi::from_base_upgrade`.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_precompile_storage::{BasePrecompileError, Result};

mod v1;
pub use v1::IB20 as IB20V1;

mod v2;
pub use v2::{IB20, IB20 as IB20V2};

/// A frozen wire (ABI) surface of the shared B-20 token interface.
///
/// Reached only through a token version's wire join (e.g. [`crate::AssetVersion::abi`]). There is
/// deliberately no `from_base_upgrade` here: a second resolver over the fork ladder would be two
/// maps that must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum B20Abi {
    /// Wire surface activated at Beryl with the first native B-20 tokens.
    V1,
    /// Wire surface activated at Cobalt.
    V2,
}

impl B20Abi {
    /// Returns whether `selector` was dialable on this wire surface.
    pub fn valid_selector(self, selector: [u8; 4]) -> bool {
        match self {
            Self::V1 => IB20V1::IB20Calls::valid_selector(selector),
            Self::V2 => IB20V2::IB20Calls::valid_selector(selector),
        }
    }

    /// Validates `calldata` against this wire surface, mapping failures to `AbiDecodeFailed`.
    pub fn abi_decode_validate(self, calldata: &[u8], selector: [u8; 4]) -> Result<()> {
        match self {
            Self::V1 => IB20V1::IB20Calls::abi_decode_validate(calldata).map(|_| ()),
            Self::V2 => IB20V2::IB20Calls::abi_decode_validate(calldata).map(|_| ()),
        }
        .map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes `calldata` into a routable call, gated on this wire surface.
    ///
    /// The frozen surface decides what is dialable and owns any error bytes; the canonical surface
    /// then produces the value the dispatcher matches on.
    pub fn decode(self, calldata: &[u8]) -> Result<IB20::IB20Calls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        self.abi_decode_validate(calldata, selector)?;

        IB20::IB20Calls::abi_decode_validate(calldata).map_err(|error| {
            BasePrecompileError::AbiDecodeFailed { selector, error: error.to_string() }
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use alloy_sol_types::{SolCall, SolInterface};
    use base_precompile_storage::BasePrecompileError;

    use super::{B20Abi, IB20, IB20V1, IB20V2};

    #[test]
    fn v1_surface_accepts_transfer() {
        let calldata =
            IB20::transferCall { to: Address::ZERO, amount: Default::default() }.abi_encode();
        assert!(matches!(B20Abi::V1.decode(&calldata), Ok(IB20::IB20Calls::transfer(_))));
    }

    #[test]
    fn v2_surface_accepts_transfer() {
        let calldata =
            IB20::transferCall { to: Address::ZERO, amount: Default::default() }.abi_encode();
        assert!(matches!(B20Abi::V2.decode(&calldata), Ok(IB20::IB20Calls::transfer(_))));
    }

    #[test]
    fn v1_surface_rejects_unknown_selector() {
        let calldata = [0xde, 0xad, 0xbe, 0xef];
        assert_eq!(
            B20Abi::V1.decode(&calldata),
            Err(BasePrecompileError::UnknownFunctionSelector(calldata))
        );
    }

    #[test]
    fn v1_surface_malformed_known_call_is_abi_decode_failed() {
        let calldata = IB20::transferCall::SELECTOR.to_vec();
        assert!(matches!(
            B20Abi::V1.decode(&calldata),
            Err(BasePrecompileError::AbiDecodeFailed {
                selector,
                ..
            }) if selector == IB20::transferCall::SELECTOR
        ));
    }

    #[test]
    fn frozen_and_canonical_names_match() {
        assert_eq!(IB20V1::IB20Calls::NAME, IB20::IB20Calls::NAME);
        assert_eq!(IB20V2::IB20Calls::NAME, IB20::IB20Calls::NAME);
        assert_eq!(IB20V1::IB20Calls::NAME, "IB20Calls");
    }

    /// The dispatcher re-decodes against the canonical surface after a frozen surface accepts, so
    /// every frozen selector must exist on canonical. Today V1 and V2 declare the same surface.
    #[test]
    fn v1_selectors_are_a_subset_of_v2() {
        for selector in IB20V1::IB20Calls::selectors() {
            assert!(
                B20Abi::V2.valid_selector(selector),
                "V1 selector {selector:?} missing from the V2 surface"
            );
        }
    }
}
