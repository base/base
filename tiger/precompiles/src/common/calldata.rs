//! Calldata handling shared by the native precompile dispatchers: the selector, the validating
//! ABI decode and its error bytes, and the calldata gas.

use alloc::string::ToString;

use alloy_sol_types::SolInterface;

use crate::{BasePrecompileError, Result, StorageCtx};

/// Calldata helpers for native precompile dispatch.
#[derive(Debug, Clone, Copy)]
pub struct Calldata;

impl Calldata {
    /// Gas per 32-byte calldata word: `G_copy` (3) plus `G_memory` (3), the cost a Solidity
    /// predeploy would pay to read its calldata. Part of the gas-used commitment.
    pub const WORD_GAS: u64 = 6;

    /// Returns the calldata gas for `calldata`.
    pub const fn gas(calldata: &[u8]) -> u64 {
        (calldata.len() as u64).div_ceil(32).saturating_mul(Self::WORD_GAS)
    }

    /// Deducts the calldata gas for `calldata` from `ctx`.
    pub fn deduct_gas(ctx: StorageCtx<'_>, calldata: &[u8]) -> Result<()> {
        ctx.deduct_gas(Self::gas(calldata))
    }

    /// Returns the 4-byte selector. Shorter calldata is an unknown, all-zero selector.
    pub fn selector(calldata: &[u8]) -> Result<[u8; 4]> {
        let selector = calldata.first_chunk::<4>().copied();
        selector.ok_or(BasePrecompileError::UnknownFunctionSelector([0; 4]))
    }

    /// Decodes `calldata`, whose selector `C` declares, with alloy's validating decoder. A failure
    /// is `AbiDecodeFailed`, carrying the decoder's message (which reaches the wire before Cobalt).
    pub fn decode<C: SolInterface>(calldata: &[u8], selector: [u8; 4]) -> Result<C> {
        C::abi_decode_validate(calldata).map_err(|error| BasePrecompileError::AbiDecodeFailed {
            selector,
            error: error.to_string(),
        })
    }

    /// Decodes a call on interface `C`: an unknown selector, a decode failure, or the call.
    pub fn decode_call<C: SolInterface>(calldata: &[u8]) -> Result<C> {
        let selector = Self::selector(calldata)?;
        if !C::valid_selector(selector) {
            return Err(BasePrecompileError::UnknownFunctionSelector(selector));
        }
        Self::decode(calldata, selector)
    }
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall;

    use crate::{BasePrecompileError, Result};

    use super::Calldata;

    use crate::IPolicyRegistry;

    fn decode_policy_call(calldata: &[u8]) -> Result<IPolicyRegistry::IPolicyRegistryCalls> {
        Calldata::decode_call(calldata)
    }

    #[test]
    fn decode_precompile_call_rejects_short_calldata() {
        let err = decode_policy_call(&[1, 2, 3]).unwrap_err();

        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector([0u8; 4]));
    }

    #[test]
    fn decode_precompile_call_preserves_unknown_selector() {
        let err = decode_policy_call(&[1, 2, 3, 4]).unwrap_err();

        assert_eq!(err, BasePrecompileError::UnknownFunctionSelector([1, 2, 3, 4]));
    }

    #[test]
    fn decode_precompile_call_classifies_known_selector_decode_failure() {
        let err = decode_policy_call(&IPolicyRegistry::createPolicyCall::SELECTOR).unwrap_err();

        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector: IPolicyRegistry::createPolicyCall::SELECTOR,
                ..
            }
        ));
    }

    #[test]
    fn decode_precompile_call_decodes_known_call() {
        let calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();
        let call = decode_policy_call(&calldata).unwrap();

        assert!(matches!(call, IPolicyRegistry::IPolicyRegistryCalls::policyExists(_)));
    }

    #[test]
    fn decode_precompile_call_rejects_dirty_padding_bytes() {
        // policyExists(uint64 policyId) encodes policyId as a right-aligned 32-byte word.
        // Injecting 0xFF into the high-padding byte triggers abi_decode_validate's canonical
        // check, confirming the validating decoder is used.
        let mut calldata = IPolicyRegistry::policyExistsCall { policyId: 0 }.abi_encode();
        calldata[4] = 0xFF;
        let err = decode_policy_call(&calldata).unwrap_err();

        assert!(matches!(
            err,
            BasePrecompileError::AbiDecodeFailed {
                selector: IPolicyRegistry::policyExistsCall::SELECTOR,
                ..
            }
        ));
    }
}
