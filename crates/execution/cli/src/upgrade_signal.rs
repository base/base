//! Execution-node upgrade signal schedule application.

use alloy_consensus::BlockHeader;
use base_execution_chainspec::BaseChainSpec;
use base_node_runner::{BaseNodeExtension, BaseRpcContext, FromExtensionConfig, NodeHooks};
use base_upgrade_signal::{
    PackedProtocolVersion, UpgradeSignalApplySummary, UpgradeSignalConfig, UpgradeSignalDefaults,
    UpgradeSignalMetricLayer, UpgradeSignalMetrics, UpgradeSignalMonitor, UpgradeSignalPollOutcome,
    UpgradeSignalRefresher, UpgradeSignalRuntimeApplier, UpgradeSignalSchedule,
};
use jsonrpsee::{RpcModule, core::RpcResult, types::ErrorObject};
use reth_chainspec::{EthChainSpec, ForkId, Head};
use reth_network_p2p::sync::NetworkSyncUpdater;
use reth_provider::{BlockNumReader, HeaderProvider};
use reth_rpc_server_types::RethRpcModule;
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
    /// chain spec and installing it through [`NetworkSyncUpdater::set_fork_filter`] keeps the node's
    /// advertised fork identity aligned with the rules it now enforces.
    pub fn install_runtime_fork_filter<Net: NetworkSyncUpdater>(
        chain_spec: &BaseChainSpec,
        head: Head,
        network: &Net,
    ) -> ForkId {
        let fork_filter = chain_spec.fork_filter(head);
        let fork_id = fork_filter.current();
        network.set_fork_filter(fork_filter);
        fork_id
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
    /// a new filter is installed, so the network message is sent once per change rather than every
    /// block.
    pub fn refresh_advertised_fork_filter<Net: NetworkSyncUpdater>(
        chain_spec: &BaseChainSpec,
        head: Head,
        network: &Net,
        schedule_id: &mut ForkId,
    ) -> Option<ForkId> {
        let current = Self::schedule_fork_id(chain_spec);
        if current == *schedule_id {
            return None;
        }

        let fork_id = Self::install_runtime_fork_filter(chain_spec, head, network);
        *schedule_id = current;
        Some(fork_id)
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
    pub fn register_runtime_refresh_rpc(
        ctx: &mut BaseRpcContext<'_>,
        config: ExecutionUpgradeSignalConfig,
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
            .register_async_method("admin_refreshUpgradeSignal", |_, refresher, _| async move {
                Self::refresh_runtime_upgrade_signal(&refresher).await
            })
            .map_err(|error| eyre::eyre!(error))?;
        ctx.modules.merge_if_module_configured(RethRpcModule::Admin, module)?;

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

        let hooks = if config.signal_config.mode.allows_runtime_admin() {
            let rpc_config = config.clone();
            hooks.add_rpc_module(move |ctx: &mut BaseRpcContext<'_>| {
                ExecutionUpgradeSignal::register_runtime_refresh_rpc(ctx, rpc_config)
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
                        // path and the manual admin RPC without re-installing on every block.
                        let mut schedule_id =
                            ExecutionUpgradeSignal::schedule_fork_id(chain_spec.as_ref());

                        loop {
                            tokio::select! {
                                _ = &mut signal => break,
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
                                    // instead of the one cached at startup. The cheap schedule-fork-id
                                    // compare guards the provider read so an unchanged schedule never
                                    // reads the head or logs a spurious head-read failure.
                                    if ExecutionUpgradeSignal::schedule_fork_id(chain_spec.as_ref())
                                        != schedule_id
                                    {
                                        match ExecutionUpgradeSignal::current_head(&provider) {
                                            Ok(head) => {
                                                if let Some(fork_id) =
                                                    ExecutionUpgradeSignal::refresh_advertised_fork_filter(
                                                        chain_spec.as_ref(),
                                                        head,
                                                        &network,
                                                        &mut schedule_id,
                                                    )
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

    /// Records the last [`ForkFilter`] installed via [`NetworkSyncUpdater::set_fork_filter`].
    ///
    /// Hand-rolled rather than `mockall::automock`: [`NetworkSyncUpdater`] is defined in reth, and
    /// `automock` can only be applied at a trait's own definition, so it cannot mock a foreign trait.
    #[derive(Debug, Default)]
    struct RecordingNetwork {
        installed: std::sync::Mutex<Option<reth_chainspec::ForkFilter>>,
    }

    impl RecordingNetwork {
        fn installed_opt(&self) -> Option<reth_chainspec::ForkFilter> {
            self.installed.lock().unwrap().clone()
        }

        fn installed(&self) -> reth_chainspec::ForkFilter {
            self.installed_opt().expect("set_fork_filter was never called")
        }
    }

    impl NetworkSyncUpdater for RecordingNetwork {
        fn update_sync_state(&self, _state: reth_network_p2p::sync::SyncState) {}

        fn update_status(&self, _head: Head) {}

        fn update_block_range(&self, _update: reth_eth_wire_types::BlockRangeUpdate) {}

        fn set_fork_filter(&self, fork_filter: reth_chainspec::ForkFilter) {
            *self.installed.lock().unwrap() = Some(fork_filter);
        }
    }

    /// Regression test for the runtime fork-filter fix.
    ///
    /// Audit finding: a runtime schedule update left reth's P2P fork filter stale, so a running node
    /// kept advertising the fork id it cached at startup and partitioned from freshly restarted
    /// peers once the fork activated. The fix reinstalls the fork filter whenever the runtime
    /// schedule changes. This exercises the exact per-poll routine the monitor loop runs and asserts
    /// it (1) is a no-op while the schedule is unchanged, (2) installs a filter matching a freshly
    /// restarted node the moment a fork is scheduled at runtime, and (3) is idempotent afterwards.
    /// If the fix regresses, the node would keep the stale filter and these assertions fail.
    #[test]
    fn refresh_advertised_fork_filter_tracks_runtime_schedule_changes() {
        use alloy_genesis::Genesis;
        use base_execution_chainspec::BaseChainSpecBuilder;
        use reth_chainspec::Chain;

        let chain_id = 9_100_100;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        let spec = BaseChainSpecBuilder::default()
            .chain(Chain::from_id(chain_id))
            .genesis(Genesis::default())
            .with_fork(EthereumHardfork::Osaka, ForkCondition::Never)
            .with_fork(BaseUpgrade::Azul, ForkCondition::Never)
            .build();

        let head = Head { timestamp: 43, ..Default::default() };
        let network = RecordingNetwork::default();
        let mut schedule_id = ExecutionUpgradeSignal::schedule_fork_id(&spec);

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
        assert_eq!(schedule_id, ExecutionUpgradeSignal::schedule_fork_id(&spec));

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
}
