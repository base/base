//! Zero-copy, validating ABI decode for native precompile calls.
//!
//! Alloy's owned decode copies every argument. A `bytes[]` whose element offsets all alias one
//! M-byte tail then costs N x M heap from ~(32N + M) bytes of calldata, unmetered because gas is
//! charged on physical calldata length. Decoding to the borrowed [`SolCall::Token`] keeps each
//! argument as a slice into the source calldata, so aliased offsets cost fat-pointers, not copies.
//!
//! Removal (`alloy-aliasing`): once alloy's owned decode stops copying aliased offsets, delete this
//! module and revert every `alloy-aliasing` call site to plain owned `abi_decode_validate`.

use alloy_sol_types::{SolCall, SolType, abi};

/// Zero-copy, validating ABI decode for a [`SolCall`]'s arguments.
///
/// Blanket-implemented for every `SolCall`, so any call with an amplifiable argument can share one
/// tested decode instead of hand-rolling it and reaching into raw tokens.
pub trait BorrowedCallDecode: SolCall {
    /// Decodes `args` (calldata past the 4-byte selector) into the borrowed argument token, or
    /// `None` if it fails to decode or validate.
    ///
    /// Accepts exactly the set alloy's owned `abi_decode_validate` accepts: that path runs the same
    /// `decode_sequence` and `valid_token` (via `type_check`) checks, then copies — this stops
    /// before the copy. `None` therefore means the owned decode would also reject, so callers fall
    /// through to it and its authoritative, consensus-frozen error bytes.
    fn decode_args_borrowed(args: &[u8]) -> Option<Self::Token<'_>> {
        let token = abi::decode_sequence::<Self::Token<'_>>(args).ok()?;
        <Self::Parameters<'_> as SolType>::valid_token(&token).then_some(token)
    }
}

impl<C: SolCall> BorrowedCallDecode for C {}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec, vec::Vec};

    use alloy_primitives::{Address, B256, Bytes};
    use alloy_sol_types::{SolCall, SolValue};

    use super::BorrowedCallDecode;
    use crate::IB20Factory;

    /// The borrowed decode accepts a call iff alloy's owned decode does — the consensus-safety
    /// invariant every caller relies on. `createB20` carries the `bytes[]` shape the trait guards.
    #[test]
    fn borrowed_accept_set_matches_owned_oracle() {
        let params: Bytes = IB20Factory::B20AssetCreateParams {
            version: 1,
            name: String::from("Name"),
            symbol: String::from("SYM"),
            initialAdmin: Address::repeat_byte(0xAB),
            decimals: 6,
        }
        .abi_encode()
        .into();
        let valid = IB20Factory::createB20Call {
            variant: IB20Factory::B20Variant::ASSET,
            salt: B256::repeat_byte(0x11),
            params,
            initCalls: vec![Bytes::from_static(&[0xaa, 0xbb, 0xcc, 0xdd])],
        }
        .abi_encode();

        let mut trailing_garbage = valid.clone();
        trailing_garbage.extend_from_slice(&[0u8; 16]);

        let mut truncated_args = valid.clone();
        truncated_args.truncate(valid.len() - 1);

        let rows: Vec<(&str, Vec<u8>)> = vec![
            ("valid", valid),
            ("trailing garbage", trailing_garbage),
            ("truncated args", truncated_args),
            ("selector only", IB20Factory::createB20Call::SELECTOR.to_vec()),
        ];

        for (name, calldata) in rows {
            let owned_accepts = IB20Factory::createB20Call::abi_decode_validate(&calldata).is_ok();
            let borrowed_accepts =
                IB20Factory::createB20Call::decode_args_borrowed(&calldata[4..]).is_some();
            assert_eq!(
                borrowed_accepts, owned_accepts,
                "row `{name}`: borrowed decode disagrees with the owned oracle",
            );
        }
    }
}
