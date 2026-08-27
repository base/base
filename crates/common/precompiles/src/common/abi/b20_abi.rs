//! The frozen-wire-surface selector for the shared B-20 token interface.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;
use base_precompile_storage::{BasePrecompileError, Result};

use super::{IB20, IB20V1, IB20V2};

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
    ///
    /// Owned decode (twice at `V1`). Safe without a borrowed fast path like `announce`'s
    /// (Cantina #16): no call on the shared `IB20` surface has a dynamically-sized-element array.
    /// The arrays this surface carries (`batchMint`'s `address[]`/`uint256[]`,
    /// `pause`/`unpause`'s `PausableFeature[]`) are all static-element, decoded from inline words
    /// with no per-element offset. No element offset can alias a shared tail, and every decode
    /// takes time linear in the calldata length.
    pub fn decode(self, calldata: &[u8]) -> Result<IB20::IB20Calls> {
        let Some(selector) = calldata.first_chunk::<4>().copied() else {
            return Err(BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
        };
        if !self.valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        match self {
            Self::V1 => self.abi_decode_validate(calldata, selector)?,
            Self::V2 => {}
        }

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
