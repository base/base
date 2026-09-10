//! Sequencer transaction ingress settings.

use base_execution_txpool::DEFAULT_MAX_VALIDITY_PREDICATES;

use crate::txpool::{ShadowValidityConfig, ShadowValidityConfigError};

/// Builder RPC configuration for experimental validity-bearing transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuilderApiConfig {
    /// Whether the builder accepts non-empty experimental validity metadata.
    pub accept_experimental_validity_transactions: bool,
    /// Maximum number of validity predicates accepted per transaction.
    pub max_validity_predicates: usize,
    /// Shadow-only validity injection configuration.
    pub shadow_validity: ShadowValidityConfig,
}

impl BuilderApiConfig {
    /// Creates a builder RPC configuration.
    pub const fn new(
        accept_experimental_validity_transactions: bool,
        max_validity_predicates: usize,
    ) -> Self {
        Self {
            accept_experimental_validity_transactions,
            max_validity_predicates,
            shadow_validity: ShadowValidityConfig::disabled(),
        }
    }

    /// Enables the supplied shadow validity injection configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if injection is enabled while validity extensions are disabled.
    pub const fn with_shadow_validity(
        mut self,
        shadow_validity: ShadowValidityConfig,
    ) -> Result<Self, ShadowValidityConfigError> {
        if shadow_validity.is_enabled() && !self.accept_experimental_validity_transactions {
            return Err(ShadowValidityConfigError::ValidityTransactionsDisabled);
        }
        self.shadow_validity = shadow_validity;
        Ok(self)
    }
}

impl Default for BuilderApiConfig {
    fn default() -> Self {
        Self::new(false, DEFAULT_MAX_VALIDITY_PREDICATES)
    }
}
