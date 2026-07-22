//! The per-block terms-resolution seam.

use base_execution_payer::PriceSnapshot;

use crate::error::PayerTermsError;

/// Resolves the payer service's currently-quotable terms as a per-block
/// [`PriceSnapshot`].
///
/// This is the seam between the RPC surface and chain state: the concrete
/// implementation (wired with the node/builder) decodes the on-chain payer
/// config and `SLOAD`s each slot-backed token's price against head/pending
/// state (see `base-execution-payer`'s `storage`-feature `price_snapshot`
/// reader). Keeping it a trait lets the handlers unit-test with a fixed
/// snapshot and
/// avoids a dependency on the node's state-provider stack.
pub trait PayerTerms: Send + Sync {
    /// Resolves the terms quotable at the current head/pending state.
    fn price_snapshot(&self) -> Result<PriceSnapshot, PayerTermsError>;
}
