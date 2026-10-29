//! Contract fixtures a snapshot benchmark deploys before its load test starts.
//!
//! Workload YAMLs name fixtures with placeholders, because the deploying account is a fresh
//! random funder whose addresses cannot be known when the YAML is written. `__DOUBLE_COUNTER__`
//! stands for a [`DoubleCounter`] whose slot 0 (`count1`) a separate mutator account increments
//! on an interval while measured transactions call `increment2()` on slot 1, so validity
//! predicates gated on slot 0 parity park half the senders and swap the halves at every tick.

use std::time::Duration;

use alloy_primitives::{Address, address};
use alloy_signer_local::PrivateKeySigner;
use base_load_tests::TestConfig;
use eyre::{Result, WrapErr};
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};
use url::Url;

/// Placeholder a workload YAML uses for the `DoubleCounter` address.
pub const DOUBLE_COUNTER_PLACEHOLDER: &str = "__DOUBLE_COUNTER__";

/// Address substituted for [`DOUBLE_COUNTER_PLACEHOLDER`] so the typed configuration parses and
/// validates before the fixture exists. It is never submitted to the chain.
pub const DOUBLE_COUNTER_SENTINEL: Address = address!("d0c0d0c0d0c0d0c0d0c0d0c0d0c0d0c0d0c0d0c0");

/// Wei the funder gives the mutator account: enough for tens of thousands of 100k-gas calls at
/// the pinned-elasticity base fee.
#[cfg(feature = "upgrade-signal")]
const MUTATOR_FUNDING_WEI: u128 = 1_000_000_000_000_000_000;

/// Upper bound on waiting for the funder's prefund block before deploying.
#[cfg(feature = "upgrade-signal")]
const FUNDER_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound on waiting for a fixture deployment or funding receipt. A snapshot block arrives
/// every 200 ms or 2 s, so an unmined transaction after this long is a failure, not a delay.
#[cfg(feature = "upgrade-signal")]
const FIXTURE_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// A workload YAML whose fixture placeholders are still unresolved.
#[derive(Debug, Clone)]
pub struct PendingFixtures {
    raw_yaml: String,
}

impl PendingFixtures {
    /// Reads a workload YAML. Returns the configuration parsed with sentinel addresses, plus the
    /// pending fixtures when any placeholder was present.
    pub fn load(path: &std::path::Path) -> Result<(TestConfig, Option<Self>)> {
        let raw_yaml = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read config file {}", path.display()))?;
        if !raw_yaml.contains(DOUBLE_COUNTER_PLACEHOLDER) {
            let config = TestConfig::from_yaml(&raw_yaml)
                .wrap_err("failed to load benchmark load-test configuration")?;
            return Ok((config, None));
        }
        let pending = Self { raw_yaml };
        let config = pending
            .render(DOUBLE_COUNTER_SENTINEL)
            .wrap_err("failed to load benchmark load-test configuration with fixture sentinels")?;
        Ok((config, Some(pending)))
    }

    /// Parses the YAML with `double_counter` substituted for its placeholder.
    pub fn render(&self, double_counter: Address) -> Result<TestConfig> {
        let rendered =
            self.raw_yaml.replace(DOUBLE_COUNTER_PLACEHOLDER, &format!("{double_counter:#x}"));
        Ok(TestConfig::from_yaml(&rendered)?)
    }

    /// Deploys every pending fixture from `funder` through `rpc`, funds the slot mutator, and
    /// returns the configuration with real addresses.
    pub async fn deploy(
        &self,
        rpc: &Url,
        funder: &PrivateKeySigner,
    ) -> Result<(TestConfig, DeployedFixtures)> {
        let deployed = deploy_double_counter(rpc, funder).await?;
        let config = self.render(deployed.double_counter)?;
        Ok((config, deployed))
    }
}

/// Fixtures deployed for one benchmark run.
#[derive(Debug, Clone)]
pub struct DeployedFixtures {
    /// Address of the deployed `DoubleCounter`.
    pub double_counter: Address,
    /// Funded account the slot mutator sends from, kept apart from the funder so the load
    /// tester's funding transactions never race it for nonces.
    pub mutator: PrivateKeySigner,
}

impl DeployedFixtures {
    /// Starts a task that calls `DoubleCounter.increment()` every `interval` until the returned
    /// handle is dropped, flipping slot 0 so parity-gated validity transactions swap between
    /// matching and resting. Each call waits at most one `interval` for its receipt, so a dropped
    /// transaction is logged and retried instead of stalling the mutator for the rest of the run.
    /// Returns `None` when `interval` is zero.
    pub fn start_mutator(&self, rpc: &Url, interval: Duration) -> Option<AbortOnDropHandle<()>> {
        if interval.is_zero() {
            return None;
        }
        let rpc = rpc.clone();
        let contract = self.double_counter;
        let signer = self.mutator.clone();
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            let mut flips = 0_u64;
            loop {
                match increment_double_counter(&rpc, &signer, contract, interval).await {
                    Ok(()) => {
                        flips += 1;
                        info!(target: "base_bench", %contract, flips, "double counter slot 0 flipped");
                    }
                    Err(error) => {
                        warn!(target: "base_bench", %contract, %error, "double counter mutation failed; retrying after interval");
                    }
                }
                tokio::time::sleep(interval).await;
            }
        })))
    }
}

#[cfg(feature = "upgrade-signal")]
async fn deploy_double_counter(rpc: &Url, funder: &PrivateKeySigner) -> Result<DeployedFixtures> {
    use alloy_network::EthereumWallet;
    use alloy_primitives::U256;
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_rpc_types_eth::TransactionRequest;
    use base_test_utils::DoubleCounter;

    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funder.clone()))
        .connect_http(rpc.clone());
    wait_for_funder(&provider, funder.address()).await?;

    let deployment = DoubleCounter::deploy_builder(&provider)
        .send()
        .await
        .wrap_err("failed to submit the DoubleCounter fixture deployment")?
        .with_timeout(Some(FIXTURE_RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .wrap_err("DoubleCounter fixture deployment was not included")?;
    let double_counter = deployment
        .contract_address
        .ok_or_else(|| eyre::eyre!("DoubleCounter fixture deployment receipt has no address"))?;

    let mutator = PrivateKeySigner::random();
    let funding =
        TransactionRequest::default().to(mutator.address()).value(U256::from(MUTATOR_FUNDING_WEI));
    provider
        .send_transaction(funding)
        .await
        .wrap_err("failed to submit DoubleCounter mutator funding")?
        .with_timeout(Some(FIXTURE_RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .wrap_err("DoubleCounter mutator funding was not included")?;
    info!(
        target: "base_bench",
        %double_counter,
        mutator = %mutator.address(),
        "deployed DoubleCounter fixture and funded its slot mutator"
    );
    Ok(DeployedFixtures { double_counter, mutator })
}

#[cfg(not(feature = "upgrade-signal"))]
async fn deploy_double_counter(_rpc: &Url, _funder: &PrivateKeySigner) -> Result<DeployedFixtures> {
    eyre::bail!(
        "workload references {DOUBLE_COUNTER_PLACEHOLDER}, but this base-bench was built without \
         the `upgrade-signal` feature that embeds the DoubleCounter artifact"
    )
}

#[cfg(feature = "upgrade-signal")]
async fn increment_double_counter(
    rpc: &Url,
    signer: &PrivateKeySigner,
    contract: Address,
    receipt_timeout: Duration,
) -> Result<()> {
    use alloy_network::EthereumWallet;
    use alloy_provider::ProviderBuilder;
    use base_test_utils::DoubleCounter;

    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect_http(rpc.clone());
    DoubleCounter::new(contract, &provider)
        .increment()
        .send()
        .await
        .wrap_err("failed to submit DoubleCounter.increment()")?
        .with_timeout(Some(receipt_timeout))
        .get_receipt()
        .await
        .wrap_err("DoubleCounter.increment() was not included")?;
    Ok(())
}

#[cfg(not(feature = "upgrade-signal"))]
async fn increment_double_counter(
    _rpc: &Url,
    _signer: &PrivateKeySigner,
    _contract: Address,
    _receipt_timeout: Duration,
) -> Result<()> {
    eyre::bail!("DoubleCounter bindings require the `upgrade-signal` feature")
}

/// Waits until the funder's prefund has landed in a local block, so the deployment is not
/// rejected for insufficient funds while the first descendant is still being built.
#[cfg(feature = "upgrade-signal")]
async fn wait_for_funder<P: alloy_provider::Provider>(provider: &P, funder: Address) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        let balance =
            provider.get_balance(funder).await.wrap_err("failed to read funder balance")?;
        if !balance.is_zero() {
            return Ok(());
        }
        eyre::ensure!(
            started.elapsed() < FUNDER_READY_TIMEOUT,
            "benchmark funder {funder} was not funded within {FUNDER_READY_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use base_load_tests::{TxTypeConfig, ValidityPredicateConfig};

    use super::*;

    const WORKLOAD: &str = r#"
transaction_submission_rpcs: ["http://127.0.0.1:7545"]
query_rpc: "http://127.0.0.1:7545"
chain_id: 8453
sender_count: 4
in_flight_per_sender: 2
block_time: "200ms"
funding_amount: "1000000000000000000"
fresh_recipient_ratio: 0.0
validity:
  ratio: 1.0
  predicates:
    - {type: storage, address: "__DOUBLE_COUNTER__", slot: {kind: fixed, value: "0x0"}, mask: "0x1", op: "=", value: sender_parity}
    - {type: block_number, op: "<", offset: "30"}
transactions:
  - {weight: 100, type: double_counter, contract: "__DOUBLE_COUNTER__"}
"#;

    fn write(contents: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), contents).unwrap();
        file
    }

    #[test]
    fn placeholder_parses_with_sentinel_and_renders_with_deployed_address() {
        let file = write(WORKLOAD);
        let (config, pending) = PendingFixtures::load(file.path()).unwrap();
        let pending = pending.expect("placeholder must leave fixtures pending");
        assert!(matches!(
            config.transactions[0].tx_type,
            TxTypeConfig::DoubleCounter { contract } if contract == DOUBLE_COUNTER_SENTINEL
        ));

        let deployed = address!("00000000000000000000000000000000000000aa");
        let rendered = pending.render(deployed).unwrap();
        assert!(matches!(
            rendered.transactions[0].tx_type,
            TxTypeConfig::DoubleCounter { contract } if contract == deployed
        ));
        match &rendered.validity.predicates[0] {
            ValidityPredicateConfig::Storage { address, .. } => {
                assert!(matches!(
                    address.to_template().unwrap(),
                    base_load_tests::PredicateAddress::Fixed(fixed) if fixed == deployed
                ));
            }
            other => panic!("expected storage predicate, got {other:?}"),
        }
        rendered.validate().unwrap();
    }

    #[test]
    fn workload_without_placeholder_has_no_pending_fixtures() {
        let file = write(
            &WORKLOAD.replace("__DOUBLE_COUNTER__", "0x00000000000000000000000000000000000000bb"),
        );
        let (_, pending) = PendingFixtures::load(file.path()).unwrap();
        assert!(pending.is_none());
    }
}
