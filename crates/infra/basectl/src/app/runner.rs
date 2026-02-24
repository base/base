use anyhow::Result;
use base_alloy_flashblocks::Flashblock;
use tokio::sync::mpsc;

use super::{App, Resources, ViewId, views::create_view};
use crate::{
    config::ChainConfig,
    l1_client::{FullSystemConfig, fetch_full_system_config},
    rpc::{
        BacklogFetchResult, BlockDaInfo, L1BlockInfo, L1ConnectionMode, TimestampedFlashblock,
        fetch_initial_backlog_with_progress, run_block_fetcher, run_flashblock_ws,
        run_flashblock_ws_timestamped, run_l1_blob_watcher, run_safe_head_poller,
    },
    tui::Toast,
};

/// Launches the TUI application starting from the home view.
pub async fn run_app(config: ChainConfig) -> Result<()> {
    let mut resources = Resources::new(config.clone());

    start_background_services(&config, &mut resources);

    let app = App::new(resources, ViewId::Home);
    app.run(create_view).await
}

/// Launches the TUI application starting from the specified view.
pub async fn run_app_with_view(config: ChainConfig, initial_view: ViewId) -> Result<()> {
    let mut resources = Resources::new(config.clone());

    start_background_services(&config, &mut resources);

    let app = App::new(resources, initial_view);
    app.run(create_view).await
}

fn start_background_services(config: &ChainConfig, resources: &mut Resources) {
    let (fb_tx, fb_rx) = mpsc::channel::<TimestampedFlashblock>(100);
    let (da_fb_tx, da_fb_rx) = mpsc::channel::<Flashblock>(100);
    let (sync_tx, sync_rx) = mpsc::channel::<u64>(10);
    let (backlog_tx, backlog_rx) = mpsc::channel::<BacklogFetchResult>(1000);
    let (block_req_tx, block_req_rx) = mpsc::channel::<u64>(100);
    let (block_res_tx, block_res_rx) = mpsc::channel::<BlockDaInfo>(100);
    let (l1_block_tx, l1_block_rx) = mpsc::channel::<L1BlockInfo>(100);
    let (toast_tx, toast_rx) = mpsc::channel::<Toast>(50);

    resources.flash.set_channel(fb_rx);
    resources.da.set_channels(
        da_fb_rx,
        sync_rx,
        backlog_rx,
        block_req_tx,
        block_res_rx,
        l1_block_rx,
    );
    resources.toasts.set_channel(toast_rx);

    tokio::spawn(run_flashblock_ws_timestamped(
        config.flashblocks_ws.to_string(),
        fb_tx,
        toast_tx.clone(),
    ));

    tokio::spawn(run_flashblock_ws(config.flashblocks_ws.to_string(), da_fb_tx, toast_tx.clone()));

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

    tokio::spawn(run_safe_head_poller(config.rpc.to_string(), sync_tx, toast_tx));

    let (sys_config_tx, sys_config_rx) = mpsc::channel::<FullSystemConfig>(1);
    resources.set_sys_config_channel(sys_config_rx);

    let l1_rpc = config.l1_rpc.to_string();
    let system_config_addr = config.system_config;
    tokio::spawn(async move {
        if let Ok(cfg) = fetch_full_system_config(&l1_rpc, system_config_addr).await {
            let _ = sys_config_tx.send(cfg).await;
        }
    });
}
