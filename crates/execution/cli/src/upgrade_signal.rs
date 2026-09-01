//! Execution-node upgrade signal schedule application.

use std::sync::Arc;

use alloy_consensus::BlockHeader;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    PackedProtocolVersion, UpgradeSignalApplySummary, UpgradeSignalConfig, UpgradeSignalDefaults,
    UpgradeSignalMetricLayer, UpgradeSignalMetrics, UpgradeSignalMonitor, UpgradeSignalPollOutcome,
    UpgradeSignalRefresher, UpgradeSignalRuntimeApplier, UpgradeSignalSchedule,
};
use jsonrpsee::{RpcModule, core::RpcResult, types::ErrorObject};
use reth_chainspec::{EthChainSpec, ForkFilter, ForkId, Head};
use reth_discv5::NetworkStackId;
use reth_ethereum_forks::EnrForkIdEntry;
use reth_network::{NetworkHandle, NetworkPrimitives};
use reth_network_p2p::sync::NetworkSyncUpdater;
use reth_provider::{BlockNumReader, HeaderProvider};
use reth_rpc_server_types::RethRpcModule;
use tokio::sync::Notify;
use tracing::{info, warn};
use url::Url;

/// Configuration for execution-node upgrade signal schedule reads.
#[derive(Debug, Clone)]
pub struct ExecutionUpgradeSignalConfig {
    /// Shared upgrade signal schedule read configuration.
    pub signal_config: UpgradeSignalConfig,
    /// L1 RPC URL used to read the upgrade signal contract.
    pub l1_rpc: Url,
}

/// Applies contract-backed upgrade signal schedules to execution node configuration.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionUpgradeSignal;

impl ExecutionUpgradeSignal {
    /// Applies the configured L1 upgrade signal to the chain spec before startup.
    pub async fn apply_initial_signal_to_chain_spec(
        config: &ExecutionUpgradeSignalConfig,
        chain_spec: &mut BaseChainSpec,
    ) -> eyre::Result<()> {
        let reader = config.signal_config.reader(config.l1_rpc.clone())?;
        let Some(schedule) = config
            .signal_config
            .read_startup_schedule(
                &reader,
                "execution startup",
                &[UpgradeSignalMetricLayer::Execution],
                UpgradeSignalDefaults::STARTUP_SCHEDULE_RETRY_INTERVAL,
            )
            .await?
        else {
            return Ok(());
        };

        Self::apply_schedule_to_chain_spec(chain_spec, &schedule)?;

        Ok(())
    }

    /// Applies a contract-backed upgrade activation schedule to an execution chain spec.
    pub fn apply_schedule_to_chain_spec(
        chain_spec: &mut BaseChainSpec,
        schedule: &UpgradeSignalSchedule,
    ) -> eyre::Result<usize> {
        let chain_id = chain_spec.chain().id();
        let summary =
            UpgradeSignalRuntimeApplier::apply_schedule_to_sink(chain_id, schedule, chain_spec)?;
        summary.log("execution chain spec");

        Ok(summary.applied_upgrades)
    }

    /// Rebuilds the runtime-aware P2P [`ForkFilter`](reth_chainspec::ForkFilter) for `head` and
    /// installs it on the network, returning the freshly advertised [`ForkId`].
    ///
    /// reth builds its `ForkFilter` once at startup and only advances its head, so a node that adopts
    /// an L1-signalled fork schedule at runtime keeps advertising the fork id it cached before the
    /// schedule changed. It then looks stale to fresh peers once the fork activates and gets
    /// partitioned even though it enforces the new rules. Re-deriving the filter from the updated
    /// chain spec and installing it through [`RuntimeForkFilterNetwork::install_fork_filter`] keeps
    /// the node's advertised fork identity — across both the devp2p session layer and the discovery
    /// ENR — aligned with the rules it now enforces.
    ///
    /// Returns an error if the network install fails to refresh the advertised discovery entry, so
    /// the caller can leave its baseline unadvanced and retry rather than record a stale install.
    pub fn install_runtime_fork_filter<Net: RuntimeForkFilterNetwork>(
        chain_spec: &BaseChainSpec,
        head: Head,
        network: &Net,
    ) -> eyre::Result<ForkId> {
        let fork_filter = chain_spec.fork_filter(head);
        let fork_id = fork_filter.current();
        network.install_fork_filter(fork_filter)?;
        Ok(fork_id)
    }

    /// A fork id that folds in the entire runtime schedule, used as a change signal for runtime
    /// schedule updates.
    ///
    /// It is [`BaseChainSpec::fork_id`] evaluated at a far-future head, so every scheduled fork is
    /// active regardless of the node's current head. It therefore changes exactly when the runtime
    /// schedule changes, not as the chain advances between forks. Unlike
    /// [`BaseChainSpec::latest_fork_id`] it never panics on a spec whose newest fork is still
    /// unscheduled (`Never`) — the normal state of a running node before an upgrade.
    pub fn schedule_fork_id(chain_spec: &BaseChainSpec) -> ForkId {
        chain_spec.fork_id(&Head { number: u64::MAX, timestamp: u64::MAX, ..Default::default() })
    }

    /// Reinstalls the P2P fork filter if the runtime schedule changed since it was last installed,
    /// returning the newly advertised [`ForkId`] (or `None` when the schedule is unchanged).
    ///
    /// This is the per-poll routine the runtime monitor runs after every schedule read. A runtime
    /// schedule update lands via the auto-apply path or the manual `admin_refreshUpgradeSignal` RPC;
    /// both mutate the same runtime registry that [`Self::schedule_fork_id`] reads, so a single
    /// check covers both. `schedule_id` tracks the last installed schedule and is advanced only when
    /// a new filter is installed successfully, so the network message is sent once per change rather
    /// than every block, and a failed install leaves the baseline unadvanced so the next poll
    /// retries instead of advertising a stale fork id. A `None` `schedule_id` has no last-installed
    /// baseline yet and therefore always installs, forcing the initial install after startup.
    pub fn refresh_advertised_fork_filter<Net: RuntimeForkFilterNetwork>(
        chain_spec: &BaseChainSpec,
        head: Head,
        network: &Net,
        schedule_id: &mut Option<ForkId>,
    ) -> Option<ForkId> {
        let current = Self::schedule_fork_id(chain_spec);
        if *schedule_id == Some(current) {
            return None;
        }

        match Self::install_runtime_fork_filter(chain_spec, head, network) {
            Ok(fork_id) => {
                *schedule_id = Some(current);
                Some(fork_id)
            }
            // Leave `schedule_id` unadvanced so the next poll retries; the advertised discovery fork
            // id is still stale, so recording this schedule as installed would suppress that retry.
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to install runtime P2P fork filter; leaving schedule baseline unadvanced to retry on next poll"
                );
                None
            }
        }
    }

    /// Reconciles the advertised P2P fork filter with the current runtime schedule, reading the head
    /// only when the schedule actually changed.
    ///
    /// Shared by the monitor's per-poll tick and the immediate wake-up an `admin_refreshUpgradeSignal`
    /// call triggers, so a manual refresh reinstalls the filter right away instead of lagging by up
    /// to the finalized poll interval. A `None` `schedule_id` forces an install, so the first call
    /// after startup always advertises a filter derived from the live registry — even if a schedule
    /// change landed (via the admin RPC, which comes up before this task) before the baseline was
    /// taken.
    pub fn reconcile_advertised_fork_filter<Provider, Net>(
        chain_spec: &BaseChainSpec,
        provider: &Provider,
        network: &Net,
        schedule_id: &mut Option<ForkId>,
    ) where
        Provider: BlockNumReader + HeaderProvider,
        Net: RuntimeForkFilterNetwork,
    {
        // The cheap schedule-fork-id compare guards the provider read so an unchanged schedule never
        // reads the head or logs a spurious head-read failure. `None` skips the guard and installs.
        if *schedule_id == Some(Self::schedule_fork_id(chain_spec)) {
            return;
        }

        match Self::current_head(provider) {
            Ok(head) => {
                if let Some(fork_id) =
                    Self::refresh_advertised_fork_filter(chain_spec, head, network, schedule_id)
                {
                    info!(
                        target: "upgrade_signal",
                        fork_id = ?fork_id,
                        "reinstalled P2P fork filter after runtime schedule change"
                    );
                }
            }
            Err(error) => warn!(
                target: "upgrade_signal",
                error = %error,
                "failed to rebuild P2P fork filter after runtime schedule change; will retry on next poll"
            ),
        }
    }

    /// Reads the node's current canonical head, used as the reference point for rebuilding the P2P
    /// fork filter after a runtime schedule change.
    pub fn current_head<Provider: BlockNumReader + HeaderProvider>(
        provider: &Provider,
    ) -> eyre::Result<Head> {
        let number = provider.best_block_number()?;
        let header = provider
            .sealed_header(number)?
            .ok_or_else(|| eyre::eyre!("missing header for head block {number}"))?;

        // Only `number` and `timestamp` affect the Base (timestamp-gated) fork filter; reth keeps
        // the installed filter's head current via `set_head` as new blocks arrive.
        Ok(Head {
            number,
            hash: header.hash(),
            timestamp: header.timestamp(),
            ..Default::default()
        })
    }

    /// Refreshes the runtime upgrade signal schedule for a running execution node.
    pub async fn refresh_runtime_upgrade_signal(
        refresher: &UpgradeSignalRefresher,
    ) -> RpcResult<UpgradeSignalApplySummary> {
        match refresher.read_schedule().await {
            Ok(schedule) => match refresher.apply(&schedule) {
                Ok(summary) => {
                    UpgradeSignalMetrics::record_apply_success(refresher.metrics_layer, &schedule);
                    Ok(summary)
                }
                Err(error) => {
                    UpgradeSignalMetrics::record_apply_failure(refresher.metrics_layer, &schedule);
                    warn!(
                        target: "upgrade_signal",
                        error = %error,
                        "failed to validate execution runtime upgrade signal"
                    );
                    Err(ErrorObject::owned(-32005, "failed to validate upgrade signal", None::<()>))
                }
            },
            Err(error) => {
                warn!(
                    target: "upgrade_signal",
                    error = %error,
                    "failed to refresh execution runtime upgrade signal"
                );
                Err(ErrorObject::owned(-32003, "failed to refresh upgrade signal", None::<()>))
            }
        }
    }

    /// Registers the execution admin RPC method for runtime upgrade signal refreshes.
    ///
    /// A successful apply wakes `filter_refresh` so the monitor task reconciles the advertised P2P
    /// fork filter immediately, instead of leaving it stale until the next L1 poll (up to the
    /// finalized poll interval, ~15 minutes with the default `Finalized` block tag).
    pub fn register_runtime_refresh_rpc(
        ctx: &mut BaseRpcContext<'_>,
        config: ExecutionUpgradeSignalConfig,
        filter_refresh: Arc<Notify>,
    ) -> eyre::Result<()> {
        if !config.signal_config.mode.allows_runtime_admin() {
            return Ok(());
        }

        let chain_id = ctx.config().chain.chain().id();
        let reader = config.signal_config.reader(config.l1_rpc)?;
        let refresher = UpgradeSignalRefresher::new(
            config.signal_config,
            reader,
            chain_id,
            UpgradeSignalMetricLayer::Execution,
        );
        let mut module = RpcModule::new(refresher);
        module
            .register_async_method("admin_refreshUpgradeSignal", move |_, refresher, _| {
                let filter_refresh = Arc::clone(&filter_refresh);
                async move {
                    let result = Self::refresh_runtime_upgrade_signal(&refresher).await;
                    if result.is_ok() {
                        // The registry is already mutated (apply returned Ok); wake the monitor to
                        // reinstall the fork filter against the new schedule right away.
                        filter_refresh.notify_one();
                    }
                    result
                }
            })
            .map_err(|error| eyre::eyre!(error))?;
        ctx.modules.merge_if_module_configured(RethRpcModule::Admin, module)?;

        Ok(())
    }
}

/// A live P2P network handle that can adopt a runtime fork-filter change across every layer that
/// advertises the node's fork identity.
///
/// reth's [`NetworkSyncUpdater::set_fork_filter`] refreshes the devp2p (eth-handshake) fork id and,
/// via its internal `update_fork_id`, only the `eth` discovery ENR entry. A Base node advertises its
/// discovery fork id under the `opel` ENR key, which reth never refreshes at runtime, and discovery
/// prefers the `opel` entry over the `eth` fallback. Updating only the session layer therefore
/// leaves peers reading the fork id the node cached at startup, so implementors must also refresh
/// the `opel` discovery entry.
pub trait RuntimeForkFilterNetwork {
    /// Installs `fork_filter` on the devp2p session layer and refreshes every advertised discovery
    /// ENR fork-id entry, so the node's advertised fork identity matches the rules it now enforces.
    ///
    /// Returns an error if refreshing the `opel` discovery entry fails. Callers must not advance
    /// their last-installed baseline on an error: the `opel` fork id Base discovery reads first is
    /// still stale, so the install must be retried rather than recorded as done.
    fn install_fork_filter(&self, fork_filter: ForkFilter) -> eyre::Result<()>;
}

impl<N: NetworkPrimitives> RuntimeForkFilterNetwork for NetworkHandle<N> {
    fn install_fork_filter(&self, fork_filter: ForkFilter) -> eyre::Result<()> {
        let fork_id = fork_filter.current();

        // Updates the devp2p sessions and reth's built-in `eth` discovery ENR entry.
        NetworkSyncUpdater::set_fork_filter(self, fork_filter);

        // reth's `update_fork_id` only refreshes the `eth` ENR key, but Base advertises its
        // discovery fork id under `opel` (and discovery reads it first), so refresh `opel` on both
        // discovery backends. Any node this crate runs is an OP-stack chain, hence the fixed key.
        let entry = EnrForkIdEntry::from(fork_id);

        // discv4 first: it is infallible (fire-and-forget over a channel), so it still advertises
        // the new `opel` fork id even if the fallible discv5 write below fails and short-circuits.
        if let Some(discv4) = self.discv4() {
            discv4.set_eip868_rlp(NetworkStackId::OPEL.to_vec(), entry.clone());
        }
        if let Some(discv5) = self.discv5() {
            // NB: reth's `encode_and_set_eip868_in_local_enr` double-encodes — it rlp-encodes the
            // value, then the inner `enr_insert` rlp-encodes those bytes again, producing an entry
            // `get_fork_id` cannot decode. Insert the typed entry directly so it is encoded exactly
            // once, matching how the startup ENR (`add_value_rlp`) and discv4 (above) store it.
            let opel = core::str::from_utf8(NetworkStackId::OPEL)
                .expect("opel network stack id is valid utf-8");
            // `enr_insert` can fail (e.g. the ENR exceeds the EIP-778 size limit, or signing the
            // bumped record fails), leaving the advertised `opel` fork id stale. Propagate so the
            // caller retries on the next poll instead of recording a stale install as complete.
            discv5.with_discv5(|discv5| discv5.enr_insert(opel, &entry)).map_err(|error| {
                eyre::eyre!("failed to refresh opel discv5 fork id in local ENR: {error}")
            })?;
        }

        Ok(())
    }
}

/// Execution-node extension that registers runtime admin refresh and optional live metrics.
#[derive(Debug)]
pub struct ExecutionUpgradeSignalRuntimeExtension {
    /// Extension configuration.
    pub config: ExecutionUpgradeSignalConfig,
}

impl ExecutionUpgradeSignalRuntimeExtension {
    /// Creates a new execution upgrade signal runtime extension.
    pub const fn new(config: ExecutionUpgradeSignalConfig) -> Self {
        Self { config }
    }
}

impl BaseNodeExtension for ExecutionUpgradeSignalRuntimeExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        let config = self.config;

        // Wakes the monitor task to reconcile the advertised P2P fork filter the moment an
        // `admin_refreshUpgradeSignal` call commits a schedule change, rather than waiting for the
        // next L1 poll. Created unconditionally; only signalled when runtime admin is enabled.
        let filter_refresh = Arc::new(Notify::new());

        let hooks = if config.signal_config.mode.allows_runtime_admin() {
            let rpc_config = config.clone();
            let filter_refresh = Arc::clone(&filter_refresh);
            hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
                ExecutionUpgradeSignal::register_runtime_refresh_rpc(
                    ctx,
                    rpc_config,
                    filter_refresh,
                )
            })
        } else {
            hooks
        };

        hooks.add_node_started_hook(move |ctx| {
            let poll_interval = config.signal_config.l1_block_tag.poll_interval();
            let reader = config.signal_config.reader(config.l1_rpc.clone())?;
            // Live updates are re-applied automatically, matching the manual
            // `admin_refreshUpgradeSignal` path.
            let auto_refresher = config.signal_config.mode.allows_runtime_admin().then(|| {
                UpgradeSignalRefresher::new(
                    config.signal_config.clone(),
                    reader.clone(),
                    ctx.chain_spec().chain().id(),
                    UpgradeSignalMetricLayer::Execution,
                )
            });
            let mut monitor = UpgradeSignalMonitor::new(UpgradeSignalMetricLayer::Execution);

            // Captured for the P2P fork-filter fix: after a runtime schedule change the node must
            // re-derive its advertised fork id from the updated chain spec and install it on the
            // live network, or it partitions from fresh peers at activation.
            let network = ctx.network.clone();
            let provider = ctx.provider.clone();
            let chain_spec = ctx.chain_spec();
            let executor = ctx.task_executor;

            // Spawned as a critical task so a fail-closed panic propagates to reth's TaskManager and
            // exits the process non-zero, instead of being silently swallowed by a plain spawn.
            executor.spawn_critical_with_graceful_shutdown_signal(
                "upgrade-signal-monitor",
                |signal| {
                    Box::pin(async move {
                        let mut interval = tokio::time::interval(poll_interval);
                        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        let mut signal = Box::pin(signal);

                        // Baseline for detecting runtime schedule changes. Advanced only when the
                        // fork filter is reinstalled, so a single check covers both the auto-apply
                        // path and the manual admin RPC without re-installing on every block. `None`
                        // has no installed baseline yet, so the forced initial reconcile below always
                        // installs — closing the race where an admin refresh mutates the schedule
                        // (the RPC comes up before this task) before the baseline would be taken.
                        let mut schedule_id: Option<ForkId> = None;

                        // Force an initial install so the advertised filter reflects the live
                        // registry at startup, independent of the first L1 poll.
                        ExecutionUpgradeSignal::reconcile_advertised_fork_filter(
                            chain_spec.as_ref(),
                            &provider,
                            &network,
                            &mut schedule_id,
                        );

                        loop {
                            tokio::select! {
                                _ = &mut signal => break,
                                // An admin refresh committed a schedule change; reconcile at once
                                // rather than waiting for the next L1 poll.
                                _ = filter_refresh.notified() => {
                                    ExecutionUpgradeSignal::reconcile_advertised_fork_filter(
                                        chain_spec.as_ref(),
                                        &provider,
                                        &network,
                                        &mut schedule_id,
                                    );
                                }
                                _ = interval.tick() => {
                                    let outcome = tokio::select! {
                                        _ = &mut signal => break,
                                        outcome = monitor
                                            .poll_and_apply(&reader, auto_refresher.as_ref()) =>
                                            outcome,
                                    };
                                    // Fail closed: a scheduled upgrade this node cannot support is
                                    // activating imminently. Panic so the node exits loudly rather
                                    // than fork off the network at activation.
                                    if let UpgradeSignalPollOutcome::HaltNode {
                                        upgrade_id,
                                        activation_timestamp,
                                        minimum_protocol_version,
                                        node_protocol_version,
                                    } = outcome
                                    {
                                        panic!(
                                            "upgrade signal fail-closed: upgrade {} activates at {} and requires node protocol version {}, but this binary supports {}; upgrade this node to a supported version",
                                            upgrade_id.contract_id(),
                                            activation_timestamp,
                                            PackedProtocolVersion::new(minimum_protocol_version),
                                            PackedProtocolVersion::new(node_protocol_version),
                                        );
                                    }

                                    // Adopt any runtime schedule change into the live P2P fork
                                    // filter so this node advertises the fork id it now enforces
                                    // instead of the one cached at startup.
                                    ExecutionUpgradeSignal::reconcile_advertised_fork_filter(
                                        chain_spec.as_ref(),
                                        &provider,
                                        &network,
                                        &mut schedule_id,
                                    );
                                }
                            }
                        }
                    })
                },
            );

            info!(target: "upgrade_signal", "execution upgrade signal metrics observer spawned");
            Ok(())
        })
    }
}

impl FromExtensionConfig for ExecutionUpgradeSignalRuntimeExtension {
    type Config = ExecutionUpgradeSignalConfig;

    fn from_config(config: Self::Config) -> Self {
        Self::new(config)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use base_common_genesis::{BaseUpgrade, RuntimeUpgradeRegistry, UpgradeActivation};
    use base_upgrade_signal::UpgradeSignalDefaults;
    use reth_chainspec::{ChainSpec, EthereumHardfork, ForkCondition};

    use super::*;

    fn runtime_refresher(chain_id: u64) -> UpgradeSignalRefresher {
        let config = UpgradeSignalConfig::new(Address::ZERO);
        let reader = config.reader("http://127.0.0.1:1".parse().unwrap()).unwrap();
        UpgradeSignalRefresher::new(config, reader, chain_id, UpgradeSignalMetricLayer::Execution)
    }

    fn versioned_schedule(
        upgrade_id: BaseUpgrade,
        activation_timestamp: u64,
    ) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            vec![base_upgrade_signal::UpgradeSignal {
                upgrade_id,
                activation_timestamp,
                protocol_version: UpgradeSignalDefaults::node_protocol_version(),
            }],
        )
    }

    fn schedule(signals: &[(BaseUpgrade, u64)]) -> UpgradeSignalSchedule {
        UpgradeSignalSchedule::new(
            1,
            signals
                .iter()
                .map(|(upgrade_id, activation_timestamp)| base_upgrade_signal::UpgradeSignal {
                    upgrade_id: *upgrade_id,
                    activation_timestamp: *activation_timestamp,
                    protocol_version: Default::default(),
                })
                .collect(),
        )
    }

    #[test]
    fn applies_positive_schedule_to_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Shanghai, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Canyon, ForkCondition::Never);
        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[(BaseUpgrade::Canyon, 40), (BaseUpgrade::Azul, 42)]),
        )
        .unwrap();

        assert_eq!(applied, 2);
        assert_eq!(chain_spec.fork(EthereumHardfork::Shanghai), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(BaseUpgrade::Canyon), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Timestamp(42));
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Timestamp(42));
    }

    #[test]
    fn zero_signal_clears_existing_chain_spec_forks() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(40));
        chain_spec.set_fork(BaseUpgrade::Canyon, ForkCondition::Timestamp(40));
        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Timestamp(42));
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Timestamp(42));

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[(BaseUpgrade::Azul, 0)]),
        )
        .unwrap();

        assert_eq!(applied, 0);
        assert_eq!(chain_spec.fork(EthereumHardfork::Shanghai), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(BaseUpgrade::Canyon), ForkCondition::Timestamp(40));
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }

    #[test]
    fn ignores_unsupported_signal_for_chain_spec() {
        let mut chain_spec = BaseChainSpec::devnet();

        chain_spec.set_fork(EthereumHardfork::Osaka, ForkCondition::Never);
        chain_spec.set_fork(BaseUpgrade::Azul, ForkCondition::Never);

        let applied = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[(BaseUpgrade::Delta, 42)]),
        )
        .unwrap();

        assert_eq!(applied, 0);
        assert_eq!(chain_spec.fork(EthereumHardfork::Osaka), ForkCondition::Never);
        assert_eq!(chain_spec.fork(BaseUpgrade::Azul), ForkCondition::Never);
    }

    #[test]
    fn rejects_beryl_schedule_without_activation_admin() {
        let mut chain_spec = BaseChainSpec::from(ChainSpec::default());

        let error = ExecutionUpgradeSignal::apply_schedule_to_chain_spec(
            &mut chain_spec,
            &schedule(&[(BaseUpgrade::Beryl, 42)]),
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing activation admin address"));
        assert_eq!(chain_spec.fork(BaseUpgrade::Beryl), ForkCondition::Never);
    }

    #[test]
    fn apply_applies_validated_schedule_to_registry() {
        let chain_id = 9_100_004;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let summary =
            runtime_refresher(chain_id).apply(&versioned_schedule(BaseUpgrade::Azul, 42)).unwrap();

        assert_eq!(summary.applied_upgrades, 1);
        assert_eq!(
            RuntimeUpgradeRegistry::activation(chain_id, BaseUpgrade::Azul),
            Some(UpgradeActivation::Timestamp(42))
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Records the last [`ForkFilter`] installed via [`RuntimeForkFilterNetwork::install_fork_filter`].
    ///
    /// Hand-rolled rather than `mockall::automock` because the double acts as a call log: it captures
    /// the installed filter and hands it back to the test for read-back across multiple calls (both
    /// "was anything installed" and "install the expected filter"). A recorder keeps that capture and
    /// read-back in one place, which the per-call expectation scripting of a generated mock does not.
    #[derive(Debug, Default)]
    struct RecordingNetwork {
        installed: std::sync::Mutex<Option<ForkFilter>>,
        /// When set, `install_fork_filter` fails without recording, reproducing a discovery ENR
        /// refresh failure so the baseline-retry behaviour can be exercised.
        fail: std::sync::atomic::AtomicBool,
    }

    impl RecordingNetwork {
        fn installed_opt(&self) -> Option<ForkFilter> {
            self.installed.lock().unwrap().clone()
        }

        fn installed(&self) -> ForkFilter {
            self.installed_opt().expect("install_fork_filter was never called")
        }

        fn set_failing(&self, fail: bool) {
            self.fail.store(fail, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl RuntimeForkFilterNetwork for RecordingNetwork {
        fn install_fork_filter(&self, fork_filter: ForkFilter) -> eyre::Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                eyre::bail!("simulated opel discovery ENR refresh failure");
            }
            *self.installed.lock().unwrap() = Some(fork_filter);
            Ok(())
        }
    }

    /// Regression test for the runtime fork-filter fix: the fork filter is reinstalled whenever the
    /// runtime schedule changes, is a no-op while it is unchanged, and is idempotent afterwards.
    #[test]
    fn refresh_advertised_fork_filter_tracks_runtime_schedule_changes() {
        use base_execution_chainspec::BaseChainSpecBuilder;
        use reth_chainspec::Chain;

        // A unique chain id keeps this test's runtime-registry mutation from racing the sibling
        // chain-spec tests, which read fork conditions through the same process-global registry
        // under the shared devnet chain id.
        let chain_id = 9_100_100;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        // `Default::default()` for the genesis is inferred from the builder argument, so this test
        // needs no `alloy-genesis` dependency of its own. A zero-timestamp genesis with only
        // Osaka/Azul (both `Never`) gives a clean fork ladder where scheduling Azul is a detectable
        // fork transition — unlike the devnet genesis, whose baked-in forks would absorb it.
        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Default::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .build();

        let head = Head { timestamp: 43, ..Default::default() };
        let network = RecordingNetwork::default();
        // Start from an installed baseline equal to the current (empty) schedule, so the change
        // detection below is exercised in isolation from the forced-initial-install behaviour.
        let mut schedule_id = Some(ExecutionUpgradeSignal::schedule_fork_id(&spec));

        // Nothing scheduled yet: the routine must not touch the network.
        assert_eq!(
            ExecutionUpgradeSignal::refresh_advertised_fork_filter(
                &spec,
                head,
                &network,
                &mut schedule_id,
            ),
            None
        );
        assert!(network.installed_opt().is_none());

        // Azul is scheduled at runtime via the L1 signal (mutating the runtime registry).
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);

        // The routine now reinstalls a filter identical to the one a freshly restarted node builds,
        // and advances the schedule baseline to the new schedule.
        let restarted = spec.fork_filter(head);
        let installed_id = ExecutionUpgradeSignal::refresh_advertised_fork_filter(
            &spec,
            head,
            &network,
            &mut schedule_id,
        )
        .expect("a runtime schedule change must reinstall the fork filter");
        let running = network.installed();

        assert_eq!(installed_id, restarted.current());
        assert_eq!(running.current(), restarted.current());
        assert!(running.validate(restarted.current()).is_ok());
        assert!(restarted.validate(running.current()).is_ok());
        assert_eq!(schedule_id, Some(ExecutionUpgradeSignal::schedule_fork_id(&spec)));

        // Idempotent: with no further schedule change, the routine stays a no-op.
        assert_eq!(
            ExecutionUpgradeSignal::refresh_advertised_fork_filter(
                &spec,
                head,
                &network,
                &mut schedule_id,
            ),
            None
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// A `None` baseline (no filter installed yet) forces an install even when the schedule is
    /// unchanged, so the monitor's startup reconcile always advertises a live filter. This closes
    /// the race where an admin refresh mutates the schedule before the monitor takes its baseline.
    #[test]
    fn refresh_advertised_fork_filter_forces_initial_install_when_uninstalled() {
        use base_execution_chainspec::BaseChainSpecBuilder;
        use reth_chainspec::Chain;

        let chain_id = 9_100_101;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Default::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .build();

        let head = Head { timestamp: 43, ..Default::default() };
        let network = RecordingNetwork::default();
        let mut schedule_id: Option<ForkId> = None;

        // Even with nothing scheduled, an uninstalled baseline installs the current filter and
        // advances the baseline to it.
        let installed_id = ExecutionUpgradeSignal::refresh_advertised_fork_filter(
            &spec,
            head,
            &network,
            &mut schedule_id,
        )
        .expect("a None baseline must force an initial install");

        assert_eq!(installed_id, spec.fork_filter(head).current());
        assert_eq!(schedule_id, Some(ExecutionUpgradeSignal::schedule_fork_id(&spec)));
        assert!(network.installed_opt().is_some());

        // A second call with the baseline now set is a no-op.
        assert_eq!(
            ExecutionUpgradeSignal::refresh_advertised_fork_filter(
                &spec,
                head,
                &network,
                &mut schedule_id,
            ),
            None
        );

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// A failed install (the advertised `opel` discovery fork id could not be refreshed) must leave
    /// the schedule baseline unadvanced, so the next poll retries instead of recording the stale
    /// install as complete and suppressing reconciliation until another schedule change.
    #[test]
    fn refresh_advertised_fork_filter_retries_after_failed_install() {
        use base_execution_chainspec::BaseChainSpecBuilder;
        use reth_chainspec::Chain;

        let chain_id = 9_100_102;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Default::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .build();

        let head = Head { timestamp: 43, ..Default::default() };
        let network = RecordingNetwork::default();
        let mut schedule_id = Some(ExecutionUpgradeSignal::schedule_fork_id(&spec));

        // A runtime schedule change lands, but the discovery ENR refresh fails.
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Azul, 42);
        network.set_failing(true);

        // The failed install advertises nothing new and, crucially, does not advance the baseline.
        assert_eq!(
            ExecutionUpgradeSignal::refresh_advertised_fork_filter(
                &spec,
                head,
                &network,
                &mut schedule_id,
            ),
            None
        );
        assert!(network.installed_opt().is_none());
        assert_ne!(schedule_id, Some(ExecutionUpgradeSignal::schedule_fork_id(&spec)));

        // The next poll retries because the baseline still reflects the pre-change schedule; this
        // time the install succeeds and the baseline advances to the new schedule.
        network.set_failing(false);
        let installed_id = ExecutionUpgradeSignal::refresh_advertised_fork_filter(
            &spec,
            head,
            &network,
            &mut schedule_id,
        )
        .expect("the retry after a failed install must reinstall the fork filter");

        assert_eq!(installed_id, spec.fork_filter(head).current());
        assert_eq!(schedule_id, Some(ExecutionUpgradeSignal::schedule_fork_id(&spec)));
        assert!(network.installed_opt().is_some());

        RuntimeUpgradeRegistry::clear_chain(chain_id);
    }

    /// Production-path regression for the `opel` discovery ENR refresh.
    ///
    /// [`RecordingNetwork`] only proves the routing; it cannot show that the real
    /// [`RuntimeForkFilterNetwork`] impl updates the discovery ENR fork-id entry Base advertises
    /// under. reth's own runtime path refreshes only the `eth` entry, so this stands up a live
    /// discv5-backed [`NetworkHandle`] whose startup ENR advertises an `opel` fork id, calls the
    /// production [`RuntimeForkFilterNetwork::install_fork_filter`], and asserts the advertised
    /// `opel` entry is refreshed to the new fork id. The read-back is synchronous: the discv5 handle
    /// writes the local ENR under a lock that `local_enr()` reads directly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_fork_filter_refreshes_opel_discovery_enr() {
        use std::net::Ipv4Addr;

        use reth_chainspec::ChainSpecBuilder;
        use reth_discv5::discv5::{ConfigBuilder as Discv5ConfigBuilder, ListenConfig};
        use reth_ethereum_forks::ForkHash;
        use reth_network::{EthNetworkPrimitives, NetworkConfigBuilder, NetworkManager};
        use reth_tasks::Runtime;

        // An unnamed chain id makes `NetworkStackId::id` return `None`, so reth's own fork keying
        // leaves the `opel` entry we seed below untouched. On a real Base node the chain is
        // optimism, so reth keys `opel` itself; seeding it explicitly here reproduces that startup
        // ENR shape without constructing a full optimism spec. The impl under test writes `opel`
        // unconditionally, so this still exercises the real production path.
        let chain_spec = std::sync::Arc::new(
            ChainSpecBuilder::default()
                .chain(reth_chainspec::Chain::from_id(9_100_200))
                .genesis(Default::default())
                .with_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(0))
                .build(),
        );

        // A distinct startup `opel` fork id, so the genesis-derived fork id installed below differs
        // from it and the refresh is observable.
        let startup_fork_id = ForkId { hash: ForkHash([0xff, 0xff, 0xff, 0xff]), next: u64::MAX };

        // Only discv5 is enabled, on ephemeral 127.0.0.1 ports so parallel/repeated runs never
        // collide. discv4/dns are disabled, so the discv4 branch of `install_fork_filter` is skipped.
        let config =
            NetworkConfigBuilder::<EthNetworkPrimitives>::with_rng_secret_key(Runtime::test())
                .with_unused_ports()
                .disable_discv4_discovery()
                .disable_dns_discovery()
                .discovery_v5(
                    reth_discv5::Config::builder((Ipv4Addr::LOCALHOST, 0).into())
                        .discv5_config(
                            Discv5ConfigBuilder::new(ListenConfig::Ipv4 {
                                ip: Ipv4Addr::LOCALHOST,
                                port: 0,
                            })
                            .build(),
                        )
                        .fork(NetworkStackId::OPEL, startup_fork_id),
                )
                .build_with_noop_provider(std::sync::Arc::clone(&chain_spec));

        // `NetworkManager::new` awaits `Discv5::start`, so `discv5()` is live once it returns. The
        // manager future is never polled: the `opel` write goes straight to the discv5 handle's ENR
        // lock, independent of the manager loop. `network` is held to keep discovery alive.
        let network = NetworkManager::new(config).await.expect("network manager should start");
        let handle = network.handle().clone();
        let discv5 = handle.discv5().expect("discv5 must be enabled");

        // The read path Base discovery actually uses: `get_fork_id` decodes the `opel` entry
        // (falling back to `eth` only if absent). Sanity: startup advertises the seeded fork id.
        let advertised_at_startup =
            discv5.get_fork_id(&discv5.local_enr()).expect("opel fork id present at startup");
        assert_eq!(advertised_at_startup, startup_fork_id);

        // Install a different (genesis-derived) fork filter through the production trait impl.
        let head = Head { timestamp: 1, ..Default::default() };
        let new_filter = chain_spec.fork_filter(head);
        let new_fork_id = new_filter.current();
        assert_ne!(new_fork_id, startup_fork_id, "test requires an observable fork-id change");

        handle.install_fork_filter(new_filter).expect("opel discv5 ENR refresh should succeed");

        // The advertised `opel` entry now reflects the newly enforced fork id.
        let discv5 = handle.discv5().expect("discv5 still enabled");
        let advertised_after_install =
            discv5.get_fork_id(&discv5.local_enr()).expect("opel fork id present after install");
        assert_eq!(
            advertised_after_install, new_fork_id,
            "install_fork_filter must refresh the advertised opel discovery fork id"
        );

        drop(network);
    }
}
