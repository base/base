//! Shadow-builder validity predicate injection for forwarded transactions.

use alloy_primitives::U256;
use base_execution_txpool::{
    BasePooledTransaction, BuilderApiImpl, BuilderApiServer, TransactionValidity,
    ValidatedTransaction, ValidityOperator, ValidityPredicate,
};
use jsonrpsee::core::RpcResult;
use reth_transaction_pool::TransactionPool;

use crate::{BuilderApiExtensionConfig, BuilderMetrics};

/// Number of basis points representing a 100% sampling rate.
pub const MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS: u16 = 10_000;

/// Invalid shadow validity injection configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ShadowValidityConfigError {
    /// The configured sampling rate is zero or exceeds 100%.
    #[error("shadow validity sample rate must be between 1 and 10000 basis points")]
    InvalidSampleRate,
    /// Injection was enabled without enabling validity extensions.
    #[error("shadow validity injection requires experimental validity transactions to be enabled")]
    ValidityTransactionsDisabled,
}

/// Configuration for decorating forwarded transactions with shadow-only validity predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowValidityConfig {
    enabled: bool,
    sample_rate_basis_points: u16,
}

impl ShadowValidityConfig {
    /// Returns disabled shadow validity injection configuration.
    pub const fn disabled() -> Self {
        Self { enabled: false, sample_rate_basis_points: 0 }
    }

    /// Enables shadow validity injection at the supplied basis-point sampling rate.
    ///
    /// # Errors
    ///
    /// Returns an error when the rate is zero or greater than 100%.
    pub const fn enabled(sample_rate_basis_points: u16) -> Result<Self, ShadowValidityConfigError> {
        if sample_rate_basis_points == 0
            || sample_rate_basis_points > MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS
        {
            return Err(ShadowValidityConfigError::InvalidSampleRate);
        }
        Ok(Self { enabled: true, sample_rate_basis_points })
    }

    /// Returns whether injection is enabled.
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Returns the configured sampling rate in basis points.
    pub const fn sample_rate_basis_points(self) -> u16 {
        self.sample_rate_basis_points
    }

    fn inject(&self, tx: &mut ValidatedTransaction<TransactionValidity>) -> InjectionOutcome {
        if !self.enabled {
            return InjectionOutcome::Disabled;
        }
        if !tx.extensions.validity.is_empty() {
            return InjectionOutcome::ExistingValidity;
        }
        // EIP-1559 transactions use the 0x02 EIP-2718 type byte. The inner RPC handler performs
        // full decoding and rejects malformed transactions after this inexpensive eligibility
        // check.
        if tx.raw.first() != Some(&0x02) {
            return InjectionOutcome::UnsupportedType;
        }

        if !self.samples(&tx.raw) {
            return InjectionOutcome::NotSampled;
        }

        tx.extensions.validity.push(ValidityPredicate::Balance {
            address: tx.sender,
            op: ValidityOperator::GreaterThan,
            value: U256::ZERO,
        });
        InjectionOutcome::Injected
    }

    fn samples(&self, raw: &[u8]) -> bool {
        // Signed EIP-1559 transactions end with the signature's `s` scalar. Its low bytes provide
        // stable per-transaction entropy without hashing bytes that the inner handler hashes after
        // decoding anyway.
        let sample = raw
            .iter()
            .rev()
            .take(size_of::<u64>())
            .fold(0_u64, |sample, byte| (sample << 8) | u64::from(*byte));
        sample % u64::from(MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS)
            < u64::from(self.sample_rate_basis_points)
    }
}

impl Default for ShadowValidityConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Builder API that decorates sampled transactions before normal validated insertion.
#[derive(Debug)]
pub struct ShadowValidityBuilderApi<P> {
    inner: BuilderApiImpl<P, TransactionValidity>,
    config: ShadowValidityConfig,
}

impl<P> ShadowValidityBuilderApi<P> {
    /// Creates a builder API using validated configuration.
    pub const fn new(pool: P, config: BuilderApiExtensionConfig) -> Self {
        Self {
            inner: BuilderApiImpl::with_extensions(
                pool,
                config.accept_experimental_validity_transactions,
                config.max_validity_predicates,
            ),
            config: config.shadow_validity,
        }
    }
}

#[async_trait::async_trait]
impl<P> BuilderApiServer<TransactionValidity> for ShadowValidityBuilderApi<P>
where
    P: TransactionPool<Transaction = BasePooledTransaction> + Send + Sync + 'static,
{
    async fn insert_validated_transaction(
        &self,
        mut tx: ValidatedTransaction<TransactionValidity>,
    ) -> RpcResult<()> {
        let outcome = self.config.inject(&mut tx);
        if self.config.is_enabled() {
            BuilderMetrics::shadow_validity_injection_total(outcome.label()).increment(1);
        }
        self.inner.insert_validated_transaction(tx).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InjectionOutcome {
    Disabled,
    ExistingValidity,
    UnsupportedType,
    NotSampled,
    Injected,
}

impl InjectionOutcome {
    const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ExistingValidity => "existing_validity",
            Self::UnsupportedType => "unsupported_type",
            Self::NotSampled => "not_sampled",
            Self::Injected => "injected",
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes};

    use super::*;

    fn transaction(raw: Bytes) -> ValidatedTransaction<TransactionValidity> {
        ValidatedTransaction {
            sender: Address::repeat_byte(0x11),
            raw,
            extensions: TransactionValidity::default(),
        }
    }

    #[test]
    fn injects_sender_balance_predicate_without_changing_transaction() {
        let config = ShadowValidityConfig::enabled(MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS).unwrap();
        let mut tx = transaction(Bytes::from_static(&[0x02, 0x01, 0x02]));
        let original = tx.clone();

        assert_eq!(config.inject(&mut tx), InjectionOutcome::Injected);
        assert_eq!(tx.sender, original.sender);
        assert_eq!(tx.raw, original.raw);
        assert_eq!(
            tx.extensions.validity,
            vec![ValidityPredicate::Balance {
                address: original.sender,
                op: ValidityOperator::GreaterThan,
                value: U256::ZERO,
            }]
        );
    }

    #[test]
    fn preserves_existing_validity() {
        let config = ShadowValidityConfig::enabled(MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS).unwrap();
        let predicate =
            ValidityPredicate::BlockNumber { op: ValidityOperator::GreaterThan, value: U256::ZERO };
        let mut existing = transaction(Bytes::from_static(&[0x02, 0x01]));
        existing.extensions.validity.push(predicate.clone());
        assert_eq!(config.inject(&mut existing), InjectionOutcome::ExistingValidity);
        assert_eq!(existing.extensions.validity, vec![predicate]);
    }

    #[test]
    fn only_samples_eip1559_transactions_deterministically() {
        let config = ShadowValidityConfig::enabled(1).unwrap();
        let mut unsupported = transaction(Bytes::from_static(&[0x01, 0x01]));
        assert_eq!(config.inject(&mut unsupported), InjectionOutcome::UnsupportedType);

        let raw = Bytes::from_static(&[0x02, 0x55]);
        let first = config.inject(&mut transaction(raw.clone()));
        let second = config.inject(&mut transaction(raw));
        assert_eq!(first, second);
    }

    #[test]
    fn configuration_rejects_unsafe_combinations() {
        assert!(ShadowValidityConfig::enabled(0).is_err());
        assert!(ShadowValidityConfig::enabled(MAX_SHADOW_VALIDITY_SAMPLE_RATE_BPS + 1).is_err());
        let shadow = ShadowValidityConfig::enabled(1).unwrap();
        assert!(BuilderApiExtensionConfig::new(false, 1).with_shadow_validity(shadow).is_err());
        assert!(BuilderApiExtensionConfig::new(true, 1).with_shadow_validity(shadow).is_ok());
    }
}
