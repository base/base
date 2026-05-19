//! Temporary execution discovery helper command.

use std::{collections::HashSet, net::SocketAddr, str::FromStr, time::Duration};

use base_common_chains::ChainConfig;
use clap::Args;
use eyre::WrapErr;
use reth_cli_runner::CliRunner;
use reth_cli_util::load_secret_key::rng_secret_key;
use reth_discv4::{DiscoveryUpdate, Discv4, Discv4Config, NodeRecord};
use tokio::{task::JoinHandle, time};
use tokio_stream::StreamExt;
use tracing::{debug, info, warn};

use crate::config::ResolvedChainConfig;

/// Arguments for `base connect`.
#[derive(Args, Clone, Debug)]
pub(crate) struct ConnectCommand {
    /// Execution peer enode to help seed through discv4.
    #[arg(value_name = "ENODE")]
    pub(crate) enode: NodeRecord,

    /// Additional execution bootnode enodes to try.
    #[arg(long = "bootnode", value_delimiter = ',', value_name = "ENODE")]
    pub(crate) bootnodes: Vec<NodeRecord>,

    /// Local UDP listen address for the temporary discovery helper.
    #[arg(long, default_value = "0.0.0.0:0")]
    pub(crate) listen_addr: SocketAddr,

    /// How long to keep the temporary discovery helper running, in seconds.
    #[arg(long, default_value_t = 60)]
    pub(crate) duration: u64,

    /// Delay between targeted discovery lookups, in seconds.
    #[arg(long, default_value_t = 5)]
    pub(crate) lookup_interval: u64,
}

impl ConnectCommand {
    /// Runs the temporary discovery helper.
    pub(crate) fn run(self, resolved_chain: ResolvedChainConfig) -> eyre::Result<()> {
        let mut bootnodes = Self::chain_execution_bootnodes(&resolved_chain)?;
        bootnodes.extend(self.bootnodes.iter().copied());
        let bootnodes = Self::unique_nodes(bootnodes);

        if bootnodes.is_empty() {
            eyre::bail!("chain `{}` has no execution bootnodes", resolved_chain.name);
        }

        CliRunner::try_default_runtime()?
            .run_command_until_exit(|_| async move { self.run_discovery(bootnodes).await })
    }

    async fn run_discovery(self, bootnodes: Vec<NodeRecord>) -> eyre::Result<()> {
        let duration = Duration::from_secs(self.duration);
        let lookup_interval = Duration::from_secs(self.lookup_interval.max(1));
        let secret_key = rng_secret_key();
        let local_record = NodeRecord::from_secret_key(self.listen_addr, &secret_key);
        let config =
            Discv4Config::builder().enable_dht_random_walk(true).enable_lookup(true).build();
        let (discv4, mut service) =
            Discv4::bind(self.listen_addr, local_record, secret_key, config).await.wrap_err_with(
                || format!("failed to bind discovery socket at {}", self.listen_addr),
            )?;
        let mut updates = service.update_stream();
        let service_handle = service.spawn();
        let local_record = discv4.node_record();

        info!(
            target = %self.enode,
            local_enode = %local_record,
            bootnodes = bootnodes.len(),
            duration_secs = self.duration,
            lookup_interval_secs = self.lookup_interval.max(1),
            "Starting execution discovery connect helper"
        );

        for bootnode in &bootnodes {
            debug!(bootnode = %bootnode, "Adding execution bootnode");
            discv4.add_node(*bootnode);
        }
        discv4.add_node(self.enode);

        let mut added = 0usize;
        let mut removed = 0usize;
        let mut lookups = 0usize;
        let mut closest_records = 0usize;
        let mut lookup_tick = time::interval(lookup_interval);
        lookup_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let shutdown = time::sleep(duration);
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    break;
                }
                _ = lookup_tick.tick() => {
                    lookups += 1;
                    let lookup_result = tokio::select! {
                        _ = &mut shutdown => {
                            break;
                        }
                        result = discv4.lookup(self.enode.id) => result,
                    };
                    match lookup_result {
                        Ok(records) => {
                            closest_records = records.len();
                            for record in records {
                                debug!(
                                    peer_id = ?record.id,
                                    enode = %record,
                                    "Discovered execution peer near target"
                                );
                                discv4.add_node(record);
                            }
                            info!(
                                target = %self.enode,
                                records = closest_records,
                                "Completed target discovery lookup"
                            );
                        }
                        Err(error) => {
                            warn!(
                                target = %self.enode,
                                error = %error,
                                "Target discovery lookup failed"
                            );
                        }
                    }
                }
                update = updates.next() => {
                    let Some(update) = update else {
                        warn!("Discovery update stream closed");
                        break;
                    };
                    Self::record_update(update, &mut added, &mut removed);
                }
            }
        }

        discv4.terminate();
        Self::stop_service(service_handle).await?;

        info!(
            target = %self.enode,
            added,
            removed,
            lookups,
            closest_records,
            "Execution discovery connect helper finished"
        );

        Ok(())
    }

    fn chain_execution_bootnodes(
        resolved_chain: &ResolvedChainConfig,
    ) -> eyre::Result<Vec<NodeRecord>> {
        let chain_config =
            ChainConfig::by_chain_id(resolved_chain.l2_chain_id).ok_or_else(|| {
                eyre::eyre!(
                    "no built-in execution bootnodes for L2 chain ID {}",
                    resolved_chain.l2_chain_id
                )
            })?;

        chain_config
            .bootnodes
            .execution
            .iter()
            .map(|raw| {
                NodeRecord::from_str(raw)
                    .wrap_err_with(|| format!("failed to parse execution bootnode `{raw}`"))
            })
            .collect()
    }

    fn unique_nodes(nodes: Vec<NodeRecord>) -> Vec<NodeRecord> {
        let mut seen = HashSet::new();
        nodes.into_iter().filter(|node| seen.insert(*node)).collect()
    }

    fn record_update(update: DiscoveryUpdate, added: &mut usize, removed: &mut usize) {
        match update {
            DiscoveryUpdate::Added(record) => {
                *added += 1;
                info!(
                    peer_id = ?record.id,
                    enode = %record,
                    "Execution discovery peer added"
                );
            }
            DiscoveryUpdate::DiscoveredAtCapacity(record) => {
                debug!(
                    peer_id = ?record.id,
                    enode = %record,
                    "Execution discovery peer found at capacity"
                );
            }
            DiscoveryUpdate::EnrForkId(record, fork_id) => {
                debug!(
                    peer_id = ?record.id,
                    enode = %record,
                    fork_id = ?fork_id,
                    "Execution discovery peer advertised fork ID"
                );
            }
            DiscoveryUpdate::Removed(peer_id) => {
                *removed += 1;
                info!(peer_id = ?peer_id, "Execution discovery peer removed");
            }
            DiscoveryUpdate::Batch(updates) => {
                for update in updates {
                    Self::record_update(update, added, removed);
                }
            }
        }
    }

    async fn stop_service(handle: JoinHandle<()>) -> eyre::Result<()> {
        match handle.await {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(eyre::eyre!("discovery service task failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::{
        cli::BaseCli,
        commands::BaseCommand,
        config::{BuiltInChain, ChainArg},
    };

    const CUSTOMER_ENODE: &str = "enode://ad2b53dfd11aba810559694ddc3f727f14be09ff54f190c923e35f92b37dda2dfbcb25e5651221be54859ea939d4cf0a9fb4859f5c67eb128b168de1fbd11b0b@40.78.102.126:30303?discport=9200";

    #[test]
    fn parses_connect_command() {
        let cli = BaseCli::parse_from(["base", "--chain", "sepolia", "connect", CUSTOMER_ENODE]);

        assert!(matches!(cli.chain, ChainArg::BuiltIn(BuiltInChain::Sepolia)));
        let BaseCommand::Connect(connect) = cli.command else {
            panic!("expected connect command");
        };
        assert_eq!(connect.enode.to_string(), CUSTOMER_ENODE);
        assert_eq!(connect.listen_addr.to_string(), "0.0.0.0:0");
        assert_eq!(connect.duration, 60);
        assert_eq!(connect.lookup_interval, 5);
    }

    #[test]
    fn parses_connect_bootnode_override() {
        let cli = BaseCli::parse_from([
            "base",
            "connect",
            CUSTOMER_ENODE,
            "--bootnode",
            CUSTOMER_ENODE,
            "--duration",
            "10",
            "--lookup-interval",
            "2",
        ]);

        let BaseCommand::Connect(connect) = cli.command else {
            panic!("expected connect command");
        };
        assert_eq!(connect.bootnodes.len(), 1);
        assert_eq!(connect.duration, 10);
        assert_eq!(connect.lookup_interval, 2);
    }
}
