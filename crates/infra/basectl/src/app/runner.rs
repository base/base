use std::time::Instant;

use anyhow::Result;
use base_common_genesis::SystemConfig;
use tokio::sync::mpsc;
use url::Url;

use super::{App, Resources, SourceLabel, ViewId, views::create_view};
use crate::{
    config::{ConductorSource, MonitoringConfig},
    rpc::{
        BacklogFetchResult, BlockDaInfo, ConductorPollUpdate, L1BlockInfo, L1ConnectionMode,
        PodsSnapshot, ProofsSnapshot, ValidatorNodeStatus, fetch_full_system_config,
        fetch_initial_backlog_with_progress, run_block_fetcher, run_conductor_poller,
        run_l1_blob_watcher, run_pods_poller, run_proofs_poller, run_safe_head_poller,
        run_validator_poller,
    },
    tui::Toast,
};

/// Launches the TUI application starting from the specified view and network.
///
/// `conductor_rpc` is the optional `--conductor-rpc` CLI override; when set it
/// forces the conductor source into `Discover` mode regardless of config.
pub async fn run_app(
    initial_view: ViewId,
    network: &str,
    conductor_rpc: Option<Url>,
) -> Result<()> {
    let mut config = MonitoringConfig::load(network).await?;
    if config.conductors.is_none()
        && let Some(bootstrap) = conductor_rpc.as_ref()
    {
        let detect_rpc = config.detect_rpc_for(Some(bootstrap));
        if let Some(detected) = MonitoringConfig::detect_name_from_rpc(&detect_rpc).await {
            config.name = detected;
        }
    }
    let mut resources = Resources::new(config.clone());
    start_background_services(&config, &mut resources, conductor_rpc.clone());
    let app = App::new(resources, initial_view, conductor_rpc);
    app.run(create_view).await
}

/// Resolves the active conductor source from CLI flag and config.
///
/// Precedence: hand-configured `conductors` list > CLI `--conductor-rpc` flag >
/// `discovery.bootstrap_rpc` from config. Static config wins so local devnet
/// (which ships with a hardcoded 3-node list) isn't accidentally clobbered by
/// the default `--conductor-rpc` value. Returns `None` when no source is
/// configured (conductor view will simply show no nodes).
fn resolve_conductor_source(
    cli_flag: Option<Url>,
    config: &MonitoringConfig,
) -> Option<ConductorSource> {
    if let Some(nodes) = config.conductors.clone() {
        return Some(ConductorSource::Static(nodes));
    }
    if let Some(bootstrap) = cli_flag {
        let ports = config.discovery.as_ref().map(|d| d.ports.clone()).unwrap_or_default();
        return Some(ConductorSource::Discover { bootstrap, ports });
    }
    if let Some(d) = config.discovery.as_ref()
        && let Some(bootstrap) = d.bootstrap_rpc.clone()
    {
        return Some(ConductorSource::Discover { bootstrap, ports: d.ports.clone() });
    }
    None
}

/// Starts all background data-fetching services, wiring their channels into `resources`.
///
/// Spawns tokio tasks for L1 blob watching, DA backlog loading, safe-head polling,
/// system config fetching, conductor polling, validator polling, and proof monitoring.
/// All tasks communicate back through channels stored in `resources`.
pub fn start_background_services(
    config: &MonitoringConfig,
    resources: &mut Resources,
    conductor_rpc: Option<Url>,
) {
    let (sync_tx, sync_rx) = mpsc::channel::<u64>(10);
    let (backlog_tx, backlog_rx) = mpsc::channel::<BacklogFetchResult>(1000);
    let (block_req_tx, block_req_rx) = mpsc::channel::<u64>(100);
    let (block_res_tx, block_res_rx) = mpsc::channel::<BlockDaInfo>(100);
    let (l1_block_tx, l1_block_rx) = mpsc::channel::<L1BlockInfo>(100);
    let (toast_tx, toast_rx) = mpsc::channel::<Toast>(50);

    resources.da.set_channels(sync_rx, backlog_rx, block_req_tx, block_res_rx, l1_block_rx);
    resources.toasts.set_channel(toast_rx);

    tokio::spawn(run_block_fetcher(
        config.rpc.to_string(),
        block_req_rx,
        block_res_tx,
        toast_tx.clone(),
    ));

    if let Some(batcher_addr) = config.batcher_address {
        let (l1_mode_tx, l1_mode_rx) = mpsc::channel::<L1ConnectionMode>(1);
        resources.da.set_l1_mode_channel(l1_mode_rx);
        tokio::spawn(run_l1_blob_watcher(
            config.l1_rpc.to_string(),
            batcher_addr,
            l1_block_tx,
            l1_mode_tx,
            toast_tx.clone(),
        ));
    }

    tokio::spawn(fetch_initial_backlog_with_progress(config.rpc.to_string(), backlog_tx));

    let proofs_toast_tx = toast_tx.clone();
    tokio::spawn(run_safe_head_poller(config.rpc.to_string(), sync_tx, toast_tx));

    let (sys_config_tx, sys_config_rx) = mpsc::channel::<SystemConfig>(1);
    resources.set_sys_config_channel(sys_config_rx);

    let l1_rpc = config.l1_rpc.to_string();
    let system_config_addr = config.system_config;
    tokio::spawn(async move {
        if let Ok(cfg) = fetch_full_system_config(&l1_rpc, system_config_addr).await {
            let _ = sys_config_tx.send(cfg).await;
        }
    });

    if let Some(source) = resolve_conductor_source(conductor_rpc, config) {
        let (conductor_tx, conductor_rx) = mpsc::channel::<ConductorPollUpdate>(8);
        resources.conductor.set_channel(conductor_rx);
        resources.conductor.set_source_label(match &source {
            ConductorSource::Static(_) => SourceLabel::Static,
            ConductorSource::Discover { bootstrap, .. } => SourceLabel::Discovered {
                bootstrap: bootstrap.clone(),
                last_refresh: Instant::now(),
            },
        });
        match &source {
            ConductorSource::Static(nodes) => {
                resources.conductor.set_nodes_config(nodes.clone());
            }
            ConductorSource::Discover { .. } => {
                if let Some(bootstrap) = source.bootstrap_node() {
                    resources.conductor.set_nodes_config(vec![bootstrap]);
                }
            }
        }
        tokio::spawn(run_conductor_poller(source, conductor_tx));
    }

    if let Some(validator_nodes) = config.validators.clone() {
        let (validator_tx, validator_rx) = mpsc::channel::<Vec<ValidatorNodeStatus>>(4);
        resources.validators.set_channel(validator_rx);
        tokio::spawn(run_validator_poller(validator_nodes, validator_tx));
    }

    if let Some(proofs_config) = config.proofs.clone() {
        let (proofs_tx, proofs_rx) = mpsc::channel::<ProofsSnapshot>(4);
        resources.proofs.set_channel(proofs_rx);
        tokio::spawn(run_proofs_poller(
            proofs_config,
            config.l1_rpc.clone(),
            config.rpc.clone(),
            proofs_tx,
            proofs_toast_tx,
        ));
    }

    if let Some(pods_config) = config.pods.clone() {
        let (pods_tx, pods_rx) = mpsc::channel::<PodsSnapshot>(4);
        resources.pods.set_channel(pods_rx);
        tokio::spawn(run_pods_poller(pods_config, pods_tx));
    }
}
