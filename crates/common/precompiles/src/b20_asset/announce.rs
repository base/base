//! The announcement entity for the asset B-20 precompile.
//!
//! `announce(bytes[] internalCalls,string,string,string)` has two decoded forms. [`BorrowedAnnounce`]
//! is the zero-copy fast path: its `bytes[]` entries are slices into the calldata, so an aliased
//! payload cannot amplify into owned copies (Cantina #16). [`IB20Asset::announceCall`] is the owned
//! safety net. [`AnnounceCall`] is the version-agnostic view over both, so [`crate::B20AssetToken`]'s
//! `run_announce` never has to know which decode produced the call. A new representation later is a
//! new impl of the trait, not a change to the runner. The internal-call loop lives in the dispatcher,
//! where re-dispatching sub-calls belongs.

use alloc::string::String;

use alloy_sol_types::{SolCall, SolType, abi};

use crate::{AssetVersion, IB20Asset};

/// The version-agnostic view of a decoded `announce` that `run_announce` consumes.
///
/// Covers both the borrowed fast-path token ([`BorrowedAnnounce`]) and the owned
/// [`IB20Asset::announceCall`] safety net.
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

/// A borrowed `announce` decode. Every `bytes[]` entry is a slice into the original calldata
/// (`PackedSeqToken(&[u8])`), so an aliased payload cannot amplify into owned copies (Cantina #16).
pub(crate) struct BorrowedAnnounce<'a>(pub <IB20Asset::announceCall as SolCall>::Token<'a>);

impl<'a> BorrowedAnnounce<'a> {
    /// Tries to interpret `calldata` as an `announce` borrowed-decode dialable at `version`.
    ///
    /// Returns `Some` when the leading 4 bytes are the `announce` selector, the surface active at
    /// `version` still declares it dialable, and the rest borrowed-decodes cleanly. Otherwise
    /// returns `None` so the caller can fall through to the generic decode path. That fall-through
    /// stays cheap because a rejected payload never reaches owned materialization.
    /// `valid_selector` future-proofs a fork that drops `announce`.
    pub(crate) fn try_from_calldata(calldata: &'a [u8], version: AssetVersion) -> Option<Self> {
        let selector = calldata.first_chunk::<4>().copied()?;
        if selector != IB20Asset::announceCall::SELECTOR {
            return None;
        }
        if !version.abi().asset.valid_selector(selector) {
            return None;
        }
        Self::decode(&calldata[4..]).ok()
    }

    /// Decodes `announce`'s parameters as slices into `rest`, never as owned copies. `rest` is the
    /// calldata with the 4-byte selector stripped.
    ///
    /// This mirrors alloy's owned `abi_decode_validate` (`decode_sequence` then `type_check`) and
    /// omits only the infallible `detokenize`, so the accept-set matches the owned path.
    /// `type_check` is required, not optional: `string` validation rejects non-UTF-8, and skipping
    /// it would accept an `id`/`description`/`uri` the owned path rejects. The caller's fall-through
    /// to the owned decoder cannot catch an accept-side divergence.
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
        // `type_check` in `decode` already validated UTF-8 for every `string` token, so this
        // conversion is total in practice. Panicking on the impossible case beats the silent
        // U+FFFD substitution `from_utf8_lossy` would perform if that invariant ever broke; that
        // silent divergence from the owned `detokenize` path would be a consensus fork.
        core::str::from_utf8(self.0.1.as_slice()).expect("type_check validated UTF-8").to_owned()
    }

    fn description(&self) -> String {
        core::str::from_utf8(self.0.2.as_slice()).expect("type_check validated UTF-8").to_owned()
    }

    fn uri(&self) -> String {
        core::str::from_utf8(self.0.3.as_slice()).expect("type_check validated UTF-8").to_owned()
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
