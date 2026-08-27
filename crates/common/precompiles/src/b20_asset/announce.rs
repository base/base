//! The announcement entity for the asset B-20 precompile.
//!
//! `announce(bytes[] internalCalls,string,string,string)` can be decoded two ways: **borrowed** —
//! the zero-copy fast path [`BorrowedAnnounce`], whose `bytes[]` entries are slices into the original
//! calldata so an aliased payload can never amplify into owned copies (Cantina #16) — and **owned**,
//! the [`IB20Asset::announceCall`] safety net. [`AnnounceCall`] is the version-agnostic view over
//! both, so the dispatcher's `run_announce` never has to know which produced it. Adding another
//! representation later is a new impl, not a change to the runner. The internal-call *loop* itself
//! lives in the dispatcher, where re-dispatching sub-calls is a routing responsibility.

use alloc::string::String;

use alloy_sol_types::{SolCall, SolType, abi};

use crate::IB20Asset;

/// The version-agnostic view of a decoded `announce` that `B20AssetToken::run_announce` needs.
///
/// Abstracts over the borrowed fast-path token ([`BorrowedAnnounce`]) and the owned
/// [`IB20Asset::announceCall`] safety net so the runner never has to know which produced it.
pub(crate) trait AnnounceCall {
    /// The announcement id, materialized as an owned `String`.
    fn id(&self) -> String;
    /// The announcement description, materialized as an owned `String`.
    fn description(&self) -> String;
    /// The announcement uri, materialized as an owned `String`.
    fn uri(&self) -> String;
    /// Number of internal calls, for metrics.
    fn internal_call_count(&self) -> usize;
    /// Summed byte length of the internal calls, for metrics.
    fn internal_call_bytes(&self) -> usize;
    /// Visits each internal call's raw bytes in order, short-circuiting on the first error.
    fn try_for_each_internal_call(
        &self,
        f: impl FnMut(&[u8]) -> base_precompile_storage::Result<()>,
    ) -> base_precompile_storage::Result<()>;
}

/// A borrowed `announce` decode: every `bytes[]` entry is a slice into the original calldata
/// (`PackedSeqToken(&[u8])`), so an aliased payload cannot amplify into owned copies (Cantina #16).
pub(crate) struct BorrowedAnnounce<'a>(pub <IB20Asset::announceCall as SolCall>::Token<'a>);

impl<'a> BorrowedAnnounce<'a> {
    /// Decodes `announce`'s parameters **borrowed** — each `bytes` is a slice into `rest`, not an
    /// owned copy. `rest` is the calldata with the 4-byte selector already stripped.
    ///
    /// This mirrors alloy's owned `abi_decode_validate` (`decode_sequence` then `type_check`) and
    /// omits only the infallible `detokenize`, so it accepts and rejects exactly the same inputs.
    /// Running `type_check` is mandatory, not optional: `string` validation rejects non-UTF-8, so
    /// skipping it would accept an `id`/`description`/`uri` the owned path rejects — an accept-side
    /// divergence the caller's fall-through to the owned decoder could not catch.
    pub(crate) fn decode(rest: &'a [u8]) -> core::result::Result<Self, ()> {
        let token = abi::decode_sequence::<<IB20Asset::announceCall as SolCall>::Token<'a>>(rest)
            .map_err(|_| ())?;
        <<IB20Asset::announceCall as SolCall>::Parameters<'a> as SolType>::type_check(&token)
            .map_err(|_| ())?;
        Ok(Self(token))
    }
}

impl AnnounceCall for BorrowedAnnounce<'_> {
    fn id(&self) -> String {
        String::from_utf8_lossy(self.0.1.as_slice()).into_owned()
    }

    fn description(&self) -> String {
        String::from_utf8_lossy(self.0.2.as_slice()).into_owned()
    }

    fn uri(&self) -> String {
        String::from_utf8_lossy(self.0.3.as_slice()).into_owned()
    }

    fn internal_call_count(&self) -> usize {
        self.0.0.0.len()
    }

    fn internal_call_bytes(&self) -> usize {
        self.0.0.0.iter().map(|call| call.as_slice().len()).sum()
    }

    fn try_for_each_internal_call(
        &self,
        mut f: impl FnMut(&[u8]) -> base_precompile_storage::Result<()>,
    ) -> base_precompile_storage::Result<()> {
        for call in &self.0.0.0 {
            f(call.as_slice())?;
        }
        Ok(())
    }
}

impl AnnounceCall for IB20Asset::announceCall {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn uri(&self) -> String {
        self.uri.clone()
    }

    fn internal_call_count(&self) -> usize {
        self.internalCalls.len()
    }

    fn internal_call_bytes(&self) -> usize {
        self.internalCalls.iter().map(|call| call.len()).sum()
    }

    fn try_for_each_internal_call(
        &self,
        mut f: impl FnMut(&[u8]) -> base_precompile_storage::Result<()>,
    ) -> base_precompile_storage::Result<()> {
        for call in &self.internalCalls {
            f(call.as_ref())?;
        }
        Ok(())
    }
}
