//! The set of L2 chains the proof program is allowed to prove.

use base_common_chains::ChainConfig;

use crate::errors::OracleProviderError;

/// Chain-support policy for the proof path.
///
/// Proving is Base-only. Every supported chain has a compiled [`ChainConfig`] that supplies the
/// static derivation parameters the guest executes with, and its contract-backed upgrade
/// activations are committed separately by [`ScheduleId`](crate::ScheduleId). A chain ID outside
/// that set has no trusted configuration to execute against, so it is rejected rather than
/// executed against the node-served configuration.
#[derive(Debug)]
pub struct SupportedChain;

impl SupportedChain {
    /// Returns the compiled chain configuration for `chain_id`.
    ///
    /// # Errors
    /// [`OracleProviderError::UnknownChainId`] when `chain_id` is not a built-in Base chain.
    pub fn config(chain_id: u64) -> Result<&'static ChainConfig, OracleProviderError> {
        ChainConfig::by_chain_id(chain_id).ok_or(OracleProviderError::UnknownChainId(chain_id))
    }

    /// Returns whether `chain_id` names a built-in Base chain.
    pub const fn is_supported(chain_id: u64) -> bool {
        ChainConfig::by_chain_id(chain_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain reachable through `ChainConfig::all()` but not through `by_chain_id` would be
    /// configured yet unprovable, so the two views have to agree.
    #[test]
    fn every_built_in_chain_is_provable() {
        for chain_config in ChainConfig::all() {
            let resolved = SupportedChain::config(chain_config.chain_id)
                .expect("built-in chain should resolve a compiled config");

            assert_eq!(resolved.chain_id, chain_config.chain_id);
            assert!(SupportedChain::is_supported(chain_config.chain_id));
        }
    }

    #[test]
    fn rejects_chain_outside_the_built_in_set() {
        const UNSUPPORTED_CHAIN_ID: u64 = 999_999_999;

        assert!(!SupportedChain::is_supported(UNSUPPORTED_CHAIN_ID));
        assert!(matches!(
            SupportedChain::config(UNSUPPORTED_CHAIN_ID),
            Err(OracleProviderError::UnknownChainId(UNSUPPORTED_CHAIN_ID))
        ));
    }
}
