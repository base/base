//! Per-game resolution of the proposal checkpoint intervals.
//!
//! The `AggregateVerifier` switches to a shorter cadence at the Cobalt
//! activation block, so `BLOCK_INTERVAL` / `INTERMEDIATE_BLOCK_INTERVAL` are a
//! function of the block a game's range starts at, not process-wide constants.
//! Every consumer resolves them from the starting block of the game it is
//! about to create, reconstruct, or look up.

use std::sync::Arc;

use base_proof_contracts::{AggregateVerifierClient, DisputeGameFactoryClient, game_lookup_blocks};

use crate::error::ProposerError;

/// The checkpoint intervals a game starting at a given block is created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intervals {
    /// Number of L2 blocks covered by one proposal.
    pub block_interval: u64,
    /// Number of L2 blocks between intermediate output root checkpoints.
    pub intermediate_block_interval: u64,
}

impl Intervals {
    /// Computes the intermediate checkpoint block numbers for a game starting
    /// at `starting_block_number`, including the target block.
    pub fn intermediate_block_numbers(
        &self,
        starting_block_number: u64,
    ) -> Result<Vec<u64>, ProposerError> {
        game_lookup_blocks(
            starting_block_number,
            self.block_interval,
            self.intermediate_block_interval,
        )
        .map_err(|e| ProposerError::Config(e.to_string()))
    }
}

/// Resolves [`Intervals`] from the onchain `AggregateVerifier`.
///
/// Both the implementation address and the interval pair are read per lookup:
/// the factory's `gameImpls` entry can be swapped by an upgrade, and the pair
/// changes at the Cobalt activation block. Caching either one at startup makes
/// the proposer stop finding — and stop creating — games on the far side of a
/// boundary it never observes.
pub struct IntervalResolver {
    verifier_client: Arc<dyn AggregateVerifierClient>,
    factory_client: Arc<dyn DisputeGameFactoryClient>,
    game_type: u32,
}

impl std::fmt::Debug for IntervalResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IntervalResolver")
            .field("game_type", &self.game_type)
            .finish_non_exhaustive()
    }
}

impl IntervalResolver {
    /// Creates an interval resolver for `game_type`.
    pub const fn new(
        verifier_client: Arc<dyn AggregateVerifierClient>,
        factory_client: Arc<dyn DisputeGameFactoryClient>,
        game_type: u32,
    ) -> Self {
        Self { verifier_client, factory_client, game_type }
    }

    /// Returns the intervals the verifier applies to a game whose range starts
    /// at `starting_block`.
    ///
    /// ponytail: one `gameImpls` call plus one `intervalsForStartingBlock` call
    /// per lookup, no cache. At a 12s poll with at most a handful of targets per
    /// tick this is noise next to the rollup output fetches on the same path;
    /// add a short-TTL cache keyed on `(impl_address, starting_block)` if the L1
    /// read budget ever becomes the constraint.
    pub async fn for_starting_block(
        &self,
        starting_block: u64,
    ) -> Result<Intervals, ProposerError> {
        let impl_address = self
            .factory_client
            .game_impls(self.game_type)
            .await
            .map_err(|e| ProposerError::Contract(format!("gameImpls lookup failed: {e}")))?;
        if impl_address.is_zero() {
            return Err(ProposerError::Contract(format!(
                "no AggregateVerifier implementation registered for game type {}",
                self.game_type
            )));
        }

        let (block_interval, intermediate_block_interval) = self
            .verifier_client
            .read_intervals_for_starting_block(impl_address, starting_block)
            .await
            .map_err(|e| {
                ProposerError::Contract(format!(
                    "intervalsForStartingBlock({starting_block}) failed: {e}"
                ))
            })?;

        Ok(Intervals { block_interval, intermediate_block_interval })
    }
}
