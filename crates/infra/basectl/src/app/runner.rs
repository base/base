use std::{io::Write, time::Instant};

use anyhow::Result;
use base_common_flashblocks::Flashblock;
use base_common_genesis::SystemConfig;
use tokio::sync::{mpsc, watch};
use url::Url;

use super::{App, Resources, SourceLabel, ViewId, views::create_view};
use crate::{
    config::{ConductorSource, MonitoringConfig},
    rpc::{
        BacklogFetchResult, BlockDaInfo, ConductorPollUpdate, L1BlockInfo, L1ConnectionMode,
        ProofsSnapshot, TimestampedFlashblock, ValidatorNodeStatus, fetch_full_system_config,
        fetch_initial_backlog_with_progress, run_block_fetcher, run_conductor_poller,
        run_flashblock_ws, run_flashblock_ws_timestamped, run_l1_blob_watcher, run_proofs_poller,
        run_safe_head_poller, run_validator_poller,
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
/// Spawns tokio tasks for flashblock streams, L1 blob watching, DA backlog loading,
/// safe-head polling, system config fetching, conductor polling, validator polling,
/// and proof monitoring. All tasks communicate back through channels stored in
/// `resources`.
pub fn start_background_services(
    config: &MonitoringConfig,
    resources: &mut Resources,
    conductor_rpc: Option<Url>,
) {
    let (fb_tx, fb_rx) = mpsc::channel::<TimestampedFlashblock>(100);
    let (da_fb_tx, da_fb_rx) = mpsc::channel::<Flashblock>(100);
    let (sync_tx, sync_rx) = mpsc::channel::<u64>(10);
    let (backlog_tx, backlog_rx) = mpsc::channel::<BacklogFetchResult>(1000);
    let (block_req_tx, block_req_rx) = mpsc::channel::<u64>(100);
    let (block_res_tx, block_res_rx) = mpsc::channel::<BlockDaInfo>(100);
    let (l1_block_tx, l1_block_rx) = mpsc::channel::<L1BlockInfo>(100);
    let (toast_tx, toast_rx) = mpsc::channel::<Toast>(50);

    resources.flash.set_channel(fb_rx);

    // Create a watch channel seeded with the configured flashblocks URL.
    // If a conductor cluster is configured and all nodes carry flashblocks_ws
    // endpoints, `run_conductor_leader_url_tracker` will push the current
    // leader's URL into this channel so both subscriber tasks switch over
    // immediately on every leadership change.
    let (fb_url_tx, fb_url_rx) = watch::channel(config.flashblocks_ws.to_string());

    // Give FlashState a clone so it can detect URL changes and reset its
    // last-flashblock tracking state (avoids spurious missed-flashblock counts
    // when the first flashblock from the new leader arrives mid-block).
    resources.flash.set_url_rx(fb_url_rx.clone());

    resources.da.set_channels(
        da_fb_rx,
        sync_rx,
        backlog_rx,
        block_req_tx,
        block_res_rx,
        l1_block_rx,
    );
    resources.toasts.set_channel(toast_rx);

    tokio::spawn(run_flashblock_ws_timestamped(fb_url_rx.clone(), fb_tx, toast_tx.clone()));
    tokio::spawn(run_flashblock_ws(fb_url_rx, da_fb_tx, toast_tx.clone()));

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
        // Wire the URL sender into ConductorState so that the existing
        // conductor poll (200 ms) drives flashblocks URL changes instead of
        // a separate task that would duplicate the conductor_leader RPCs.
        // Discovered peers carry no flashblocks_ws endpoints, so this only
        // applies to statically configured clusters (devnet today).
        match &source {
            ConductorSource::Static(nodes) => {
                if nodes.iter().any(|n| n.flashblocks_ws.is_some()) {
                    resources.conductor.set_url_sender(nodes.clone(), fb_url_tx);
                } else {
                    resources.conductor.set_nodes_config(nodes.clone());
                }
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
}

/// Streams flashblocks as JSON lines to stdout.
pub async fn run_flashblocks_json(config: MonitoringConfig) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Flashblock>(100);
    let (toast_tx, mut toast_rx) = mpsc::channel::<Toast>(50);

    let (_, url_rx) = watch::channel(config.flashblocks_ws.to_string());
    tokio::spawn(run_flashblock_ws(url_rx, tx, toast_tx));

    tokio::spawn(async move {
        while let Some(toast) = toast_rx.recv().await {
            eprintln!("connection status: {}", toast.message);
        }
    });

    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());

    while let Some(fb) = rx.recv().await {
        serde_json::to_writer(&mut writer, &fb)?;
        writeln!(writer)?;
        writer.flush()?;
    }

    Ok(())
}
