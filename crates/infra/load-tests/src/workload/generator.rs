//! Weighted transaction workload generation from configured payloads.

use std::sync::Arc;

use alloy_primitives::{B256, U256};
use alloy_rpc_types::TransactionRequest;
use tracing::instrument;

use super::{
    AerodromeClPayload, B20TransferPayload, CalldataPayload, DoubleCounterPayload, Erc20Payload,
    OsakaPayload, Payload, PrecompilePayload, RevertProbePayload, SeededRng, StoragePayload,
    TransferPayload, UniswapV3Payload, chain_prep::ChainPrepContext,
};
use crate::{
    BaselineError,
    config::WorkloadConfig,
    runner::{TxConfig, TxType},
    utils::Result,
};

/// Selected payload plus whether it consumes the runner-supplied recipient.
#[derive(Debug, Clone)]
pub(crate) struct SelectedPayload {
    payload: Arc<dyn Payload>,
}

impl SelectedPayload {
    /// Builds a fixed payload for `tx_type`, bypassing weighted selection.
    ///
    /// Used for cohorts that always send one transaction shape, such as the validity
    /// priority-lead cohort's probe. The payload is not registered with any generator, so
    /// [`Payload::prepare`] and [`Payload::teardown`] never run for it.
    pub(crate) fn from_tx_type(tx_type: &TxType, b20_run_salt: Option<B256>) -> Self {
        Self { payload: WorkloadGenerator::payload_for(tx_type, b20_run_salt) }
    }

    /// Returns true when this payload uses the runner-supplied recipient address.
    pub(crate) fn uses_runner_recipient(&self) -> bool {
        self.payload.uses_runner_recipient()
    }

    /// Returns true when the runner recipient should be this sender's pair partner.
    pub(crate) fn uses_pair_recipient(&self) -> bool {
        self.payload.uses_pair_recipient()
    }
}

/// Generates transaction workloads from configured payloads.
pub struct WorkloadGenerator {
    config: WorkloadConfig,
    rng: SeededRng,
    payloads: Vec<(Arc<dyn Payload>, f64)>,
}

impl WorkloadGenerator {
    /// Creates a new workload generator.
    pub fn new(config: WorkloadConfig) -> Self {
        let seed = config.seed.unwrap_or(0);
        Self { config, rng: SeededRng::new(seed), payloads: Vec::new() }
    }

    /// Builds a generator from weighted transaction type configs.
    ///
    /// For B-20, installs a pending payload whose run salt is filled during
    /// [`Self::prepare_all`]. When `b20_run_salt` is `Some`, the salt is applied immediately
    /// (used for calibration after prepare).
    pub fn from_tx_configs(
        workload_config: WorkloadConfig,
        transactions: &[TxConfig],
        b20_run_salt: Option<B256>,
    ) -> Result<Self> {
        let mut generator = Self::new(workload_config);

        let total_weight: u32 = transactions.iter().map(|t| t.weight).sum();
        if total_weight == 0 {
            return Err(BaselineError::Config("total transaction weight must be > 0".into()));
        }

        for tx_config in transactions {
            let weight_pct = (tx_config.weight as f64 / total_weight as f64) * 100.0;
            generator
                .payloads
                .push((Self::payload_for(&tx_config.tx_type, b20_run_salt), weight_pct));
        }

        Ok(generator)
    }

    /// Builds the payload for one transaction type.
    ///
    /// For B-20, a `None` salt installs a pending payload whose run salt is filled during
    /// [`Self::prepare_all`]; `Some` applies the salt immediately.
    pub fn payload_for(tx_type: &TxType, b20_run_salt: Option<B256>) -> Arc<dyn Payload> {
        match tx_type {
            TxType::Transfer => Arc::new(TransferPayload::default()),
            TxType::Eip8430Transfer => Arc::new(super::Eip8430TransferPayload),
            TxType::Calldata { max_size, repeat_count } => {
                Arc::new(CalldataPayload::new(*max_size).with_repeat_count(*repeat_count))
            }
            TxType::Erc20 { contract } => {
                Arc::new(Erc20Payload::new(*contract, U256::from(1000), U256::from(10000)))
            }
            TxType::Storage { contract, slots_per_tx } => {
                Arc::new(StoragePayload::new(*contract, *slots_per_tx))
            }
            TxType::DoubleCounter { contract } => Arc::new(DoubleCounterPayload::new(*contract)),
            TxType::RevertProbe { contract, gas_limit } => {
                Arc::new(RevertProbePayload::new(*contract, *gas_limit))
            }
            TxType::Precompile { target, blake2f_rounds, iterations, looper_contract } => {
                Arc::new(PrecompilePayload::with_options(
                    target.clone(),
                    *blake2f_rounds,
                    *iterations,
                    *looper_contract,
                ))
            }
            TxType::B20 => Arc::new(b20_run_salt.map_or_else(
                || B20TransferPayload::pending(U256::from(1), U256::from(1)),
                |run_salt| B20TransferPayload::new(run_salt, U256::from(1), U256::from(1)),
            )),
            TxType::Osaka { target } => Arc::new(OsakaPayload::new(target.clone())),
            TxType::UniswapV3 {
                native,
                router,
                token_in,
                token_out,
                fee,
                min_amount,
                max_amount,
                reverse_min_amount,
                reverse_max_amount,
            } => Arc::new(
                UniswapV3Payload::new(
                    *router,
                    *token_in,
                    *token_out,
                    *fee,
                    *min_amount,
                    *max_amount,
                    Some((*reverse_min_amount, *reverse_max_amount)),
                )
                .with_native(*native),
            ),
            TxType::AerodromeCl {
                router,
                token_in,
                token_out,
                tick_spacing,
                min_amount,
                max_amount,
                reverse_min_amount,
                reverse_max_amount,
            } => Arc::new(AerodromeClPayload::new(
                *router,
                *token_in,
                *token_out,
                *tick_spacing,
                *min_amount,
                *max_amount,
                Some((*reverse_min_amount, *reverse_max_amount)),
            )),
        }
    }

    /// Runs [`Payload::prepare`] for every configured payload.
    pub async fn prepare_all(&self, ctx: &mut ChainPrepContext<'_>) -> Result<()> {
        for (payload, _) in &self.payloads {
            payload.prepare(ctx).await?;
        }
        Ok(())
    }

    /// Runs [`Payload::teardown`] for every configured payload.
    pub async fn teardown_all(&self, ctx: &ChainPrepContext<'_>) -> Result<()> {
        for (payload, _) in &self.payloads {
            payload.teardown(ctx).await?;
        }
        Ok(())
    }

    /// Returns configured payload names in installation order (for tests).
    pub fn payload_names(&self) -> Vec<&'static str> {
        self.payloads.iter().map(|(payload, _)| payload.name()).collect()
    }

    /// Adds a payload type to the generator.
    pub fn with_payload(mut self, payload: impl Payload + 'static, share_pct: f64) -> Self {
        self.payloads.push((Arc::new(payload), share_pct));
        self
    }

    /// Generates a transaction payload with caller-provided addresses.
    #[instrument(skip(self))]
    pub fn generate_payload(
        &mut self,
        from: alloy_primitives::Address,
        to: alloy_primitives::Address,
    ) -> Result<TransactionRequest> {
        let payload = self.select_payload()?;
        Ok(self.generate_selected_payload(&payload, from, to))
    }

    /// Selects a payload according to configured weights.
    pub(crate) fn select_payload(&mut self) -> Result<SelectedPayload> {
        if self.payloads.is_empty() {
            return Err(BaselineError::Workload("no payloads configured".into()));
        }

        let total: f64 = self.payloads.iter().map(|(_, share)| share).sum();
        let mut target: f64 = self.rng.gen_range(0.0..total);

        for (payload, share) in &self.payloads {
            target -= share;
            if target <= 0.0 {
                return Ok(SelectedPayload { payload: Arc::clone(payload) });
            }
        }

        Ok(SelectedPayload {
            payload: Arc::clone(&self.payloads.last().expect("non-empty checked above").0),
        })
    }

    /// Generates a transaction request for a preselected payload.
    pub(crate) fn generate_selected_payload(
        &mut self,
        selected: &SelectedPayload,
        from: alloy_primitives::Address,
        to: alloy_primitives::Address,
    ) -> TransactionRequest {
        selected.payload.generate(&mut self.rng, from, to)
    }
}

impl std::fmt::Debug for WorkloadGenerator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkloadGenerator")
            .field("config", &self.config)
            .field("payloads_count", &self.payloads.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256};

    use super::*;
    use crate::runner::{TxConfig, TxType};

    #[test]
    fn from_tx_configs_maps_types_to_payload_names() {
        let configs = vec![
            TxConfig { weight: 1, tx_type: TxType::Transfer },
            TxConfig { weight: 1, tx_type: TxType::Calldata { max_size: 64, repeat_count: 1 } },
            TxConfig { weight: 1, tx_type: TxType::Erc20 { contract: Address::repeat_byte(0x11) } },
            TxConfig { weight: 1, tx_type: TxType::B20 },
            TxConfig {
                weight: 1,
                tx_type: TxType::RevertProbe {
                    contract: Address::repeat_byte(0x12),
                    gas_limit: 2_000_000,
                },
            },
            TxConfig {
                weight: 1,
                tx_type: TxType::UniswapV3 {
                    native: false,
                    router: Address::repeat_byte(0x20),
                    token_in: Address::repeat_byte(0x21),
                    token_out: Address::repeat_byte(0x22),
                    fee: 500,
                    min_amount: U256::from(1),
                    max_amount: U256::from(1),
                    reverse_min_amount: U256::from(1),
                    reverse_max_amount: U256::from(1),
                },
            },
            TxConfig {
                weight: 1,
                tx_type: TxType::AerodromeCl {
                    router: Address::repeat_byte(0x30),
                    token_in: Address::repeat_byte(0x31),
                    token_out: Address::repeat_byte(0x32),
                    tick_spacing: 100,
                    min_amount: U256::from(1),
                    max_amount: U256::from(1),
                    reverse_min_amount: U256::from(1),
                    reverse_max_amount: U256::from(1),
                },
            },
        ];

        let generator = WorkloadGenerator::from_tx_configs(
            WorkloadConfig::new("test").with_seed(1),
            &configs,
            Some(B256::repeat_byte(0xab)),
        )
        .expect("valid configs");

        assert_eq!(
            generator.payload_names(),
            vec![
                "transfer",
                "calldata",
                "erc20",
                "b20",
                "revert_probe",
                "uniswap_v3",
                "aerodrome_cl"
            ]
        );
    }

    #[test]
    fn fixed_payload_generates_through_the_shared_generator() {
        let contract = Address::repeat_byte(0x12);
        let mut generator = WorkloadGenerator::from_tx_configs(
            WorkloadConfig::new("test").with_seed(1),
            &[TxConfig { weight: 1, tx_type: TxType::Transfer }],
            None,
        )
        .expect("valid configs");
        let probe = SelectedPayload::from_tx_type(
            &TxType::RevertProbe { contract, gas_limit: 777_000 },
            None,
        );

        assert!(!probe.uses_runner_recipient());
        let request = generator.generate_selected_payload(
            &probe,
            Address::repeat_byte(0x01),
            Address::repeat_byte(0x02),
        );
        assert_eq!(request.to, Some(contract.into()));
        assert_eq!(request.gas, Some(777_000));
        assert_eq!(generator.payload_names(), vec!["transfer"], "fixed payload is not installed");
    }

    #[test]
    fn from_tx_configs_rejects_zero_total_weight() {
        let err = WorkloadGenerator::from_tx_configs(
            WorkloadConfig::new("test"),
            &[TxConfig { weight: 0, tx_type: TxType::Transfer }],
            None,
        )
        .expect_err("zero weight");
        assert!(err.to_string().contains("total transaction weight"));
    }

    #[test]
    fn b20_pending_payload_is_installed_without_salt() {
        let generator = WorkloadGenerator::from_tx_configs(
            WorkloadConfig::new("test").with_seed(1),
            &[TxConfig { weight: 1, tx_type: TxType::B20 }],
            None,
        )
        .expect("pending b20 ok");
        assert_eq!(generator.payload_names(), vec!["b20"]);
    }
}
