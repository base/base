use std::collections::VecDeque;

use base_alloy_flashblocks::Flashblock;
use tokio::sync::mpsc;

use crate::{
    commands::common::{DaTracker, FlashblockEntry, LoadingState},
    config::ChainConfig,
    l1_client::FullSystemConfig,
    rpc::{BacklogFetchResult, BlockDaInfo, L1BlockInfo, L1ConnectionMode, TimestampedFlashblock},
    tui::ToastState,
};

const MAX_FLASH_BLOCKS: usize = 30;

/// Shared resources available to all TUI views.
#[derive(Debug)]
pub(crate) struct Resources {
    /// Active chain configuration.
    pub config: ChainConfig,
    /// Data availability monitoring state.
    pub da: DaState,
    /// Flashblock stream state.
    pub flash: FlashState,
    /// Toast notification state.
    pub toasts: ToastState,
    /// L1 system config fetched from the contract.
    pub system_config: Option<FullSystemConfig>,
    sys_config_rx: Option<mpsc::Receiver<FullSystemConfig>>,
}

/// State for DA (data availability) monitoring.
#[derive(Debug)]
pub(crate) struct DaState {
    /// Tracks L2 block DA contributions and backlog.
    pub tracker: DaTracker,
    /// Current backlog loading progress, if still loading.
    pub loading: Option<LoadingState>,
    /// Whether the initial backlog has finished loading.
    pub loaded: bool,
    /// Current L1 connection mode (WebSocket or polling).
    pub l1_connection_mode: Option<L1ConnectionMode>,
    buffered_flashblocks: Vec<Flashblock>,
    buffered_safe_heads: Vec<u64>,
    buffered_l1_blocks: Vec<L1BlockInfo>,
    fb_rx: Option<mpsc::Receiver<Flashblock>>,
    sync_rx: Option<mpsc::Receiver<u64>>,
    backlog_rx: Option<mpsc::Receiver<BacklogFetchResult>>,
    block_req_tx: Option<mpsc::Sender<u64>>,
    block_res_rx: Option<mpsc::Receiver<BlockDaInfo>>,
    l1_block_rx: Option<mpsc::Receiver<L1BlockInfo>>,
    l1_mode_rx: Option<mpsc::Receiver<L1ConnectionMode>>,
}

/// State for the flashblocks stream display.
#[derive(Debug)]
pub(crate) struct FlashState {
    /// Recent flashblock entries shown in the table.
    pub entries: VecDeque<FlashblockEntry>,
    /// Current block gas limit.
    pub current_gas_limit: u64,
    /// Current base fee per gas in wei.
    pub current_base_fee: Option<u128>,
    /// Total number of flashblock messages received.
    pub message_count: u64,
    /// Count of missed (gap) flashblocks detected.
    pub missed_flashblocks: u64,
    /// Whether the flashblock stream display is paused.
    pub paused: bool,
    last_flashblock: Option<(u64, u64)>,
    fb_rx: Option<mpsc::Receiver<TimestampedFlashblock>>,
}

impl Resources {
    /// Creates new resources with the given chain configuration.
    pub(crate) fn new(config: ChainConfig) -> Self {
        Self {
            config,
            da: DaState::new(),
            flash: FlashState::new(),
            toasts: ToastState::new(),
            system_config: None,
            sys_config_rx: None,
        }
    }

    /// Returns the configured chain name.
    pub(crate) fn chain_name(&self) -> &str {
        &self.config.name
    }

    /// Sets the channel for receiving L1 system config updates.
    pub(crate) fn set_sys_config_channel(&mut self, rx: mpsc::Receiver<FullSystemConfig>) {
        self.sys_config_rx = Some(rx);
    }

    /// Polls for a new system config from the background task.
    pub(crate) fn poll_sys_config(&mut self) {
        if let Some(ref mut rx) = self.sys_config_rx
            && let Ok(cfg) = rx.try_recv()
        {
            self.system_config = Some(cfg);
        }
    }
}

impl Default for DaState {
    fn default() -> Self {
        Self::new()
    }
}

impl DaState {
    /// Creates a new empty DA state.
    pub(crate) fn new() -> Self {
        Self {
            tracker: DaTracker::new(),
            loading: None,
            loaded: false,
            l1_connection_mode: None,
            buffered_flashblocks: Vec::new(),
            buffered_safe_heads: Vec::new(),
            buffered_l1_blocks: Vec::new(),
            fb_rx: None,
            sync_rx: None,
            backlog_rx: None,
            block_req_tx: None,
            block_res_rx: None,
            l1_block_rx: None,
            l1_mode_rx: None,
        }
    }

    /// Sets the channels used for receiving DA monitoring data.
    pub(crate) fn set_channels(
        &mut self,
        fb_rx: mpsc::Receiver<Flashblock>,
        sync_rx: mpsc::Receiver<u64>,
        backlog_rx: mpsc::Receiver<BacklogFetchResult>,
        block_req_tx: mpsc::Sender<u64>,
        block_res_rx: mpsc::Receiver<BlockDaInfo>,
        l1_block_rx: mpsc::Receiver<L1BlockInfo>,
    ) {
        self.fb_rx = Some(fb_rx);
        self.sync_rx = Some(sync_rx);
        self.backlog_rx = Some(backlog_rx);
        self.block_req_tx = Some(block_req_tx);
        self.block_res_rx = Some(block_res_rx);
        self.l1_block_rx = Some(l1_block_rx);
    }

    /// Sets the channel for receiving L1 connection mode updates.
    pub(crate) fn set_l1_mode_channel(&mut self, rx: mpsc::Receiver<L1ConnectionMode>) {
        self.l1_mode_rx = Some(rx);
    }

    /// Drains all pending messages from background channels and updates state.
    pub(crate) fn poll(&mut self) {
        let backlog_results: Vec<_> = self
            .backlog_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for result in backlog_results {
            match result {
                BacklogFetchResult::Progress(progress) => {
                    self.loading = Some(LoadingState {
                        current_block: progress.current_block,
                        total_blocks: progress.total_blocks,
                    });
                }
                BacklogFetchResult::Block(block) => {
                    self.tracker.add_backlog_block(
                        block.block_number,
                        block.da_bytes,
                        block.timestamp,
                    );
                }
                BacklogFetchResult::Complete(initial) => {
                    self.tracker.set_initial_backlog(initial.safe_block, initial.da_bytes);
                    self.flush_buffers();
                    self.loaded = true;
                }
                BacklogFetchResult::Error => {
                    self.flush_buffers();
                    self.loaded = true;
                }
            }
        }

        let flashblocks: Vec<_> = self
            .fb_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for fb in flashblocks {
            if self.loaded {
                self.process_flashblock(&fb);
            } else {
                self.buffered_flashblocks.push(fb);
            }
        }

        let block_infos: Vec<_> = self
            .block_res_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for info in block_infos {
            self.tracker.update_block_info(info.block_number, info.da_bytes, info.timestamp);
        }

        let safe_blocks: Vec<_> = self
            .sync_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for safe_block in safe_blocks {
            if self.loaded {
                self.tracker.update_safe_head(safe_block);
            } else {
                self.buffered_safe_heads.push(safe_block);
            }
        }

        let l1_blocks: Vec<_> = self
            .l1_block_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for l1_block in l1_blocks {
            if self.loaded {
                self.tracker.record_l1_block(l1_block);
            } else {
                self.buffered_l1_blocks.push(l1_block);
            }
        }

        if let Some(mode) = self.l1_mode_rx.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.l1_connection_mode = Some(mode);
        }
    }

    fn flush_buffers(&mut self) {
        for fb in std::mem::take(&mut self.buffered_flashblocks) {
            self.process_flashblock(&fb);
        }
        for safe_block in std::mem::take(&mut self.buffered_safe_heads) {
            self.tracker.update_safe_head(safe_block);
        }
        for l1_block in std::mem::take(&mut self.buffered_l1_blocks) {
            self.tracker.record_l1_block(l1_block);
        }
    }

    fn process_flashblock(&mut self, fb: &Flashblock) {
        let block_number = fb.metadata.block_number;
        let da_bytes: u64 = fb.diff.transactions.iter().map(|tx| tx.len() as u64).sum();
        let timestamp = fb.base.as_ref().map(|b| b.timestamp).unwrap_or(0);

        if fb.index == 0 {
            let prev_block = self
                .tracker
                .block_contributions
                .front()
                .map(|c| c.block_number)
                .filter(|&prev| prev < block_number);

            self.tracker.add_block(block_number, da_bytes, timestamp);

            if let (Some(prev), Some(tx)) = (prev_block, &self.block_req_tx) {
                for missing in (prev..block_number).rev() {
                    let _ = tx.try_send(missing);
                }
            }
        } else if let Some(contrib) =
            self.tracker.block_contributions.iter_mut().find(|c| c.block_number == block_number)
        {
            contrib.da_bytes = contrib.da_bytes.saturating_add(da_bytes);
            if block_number > self.tracker.safe_l2_block {
                self.tracker.da_backlog_bytes =
                    self.tracker.da_backlog_bytes.saturating_add(da_bytes);
            }
            self.tracker.growth_tracker.add_sample(da_bytes);
        }
    }
}

impl Default for FlashState {
    fn default() -> Self {
        Self::new()
    }
}

impl FlashState {
    /// Creates a new empty flashblock state.
    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_FLASH_BLOCKS * 10),
            current_gas_limit: 0,
            current_base_fee: None,
            message_count: 0,
            missed_flashblocks: 0,
            paused: false,
            last_flashblock: None,
            fb_rx: None,
        }
    }

    /// Sets the channel for receiving timestamped flashblocks.
    pub(crate) fn set_channel(&mut self, fb_rx: mpsc::Receiver<TimestampedFlashblock>) {
        self.fb_rx = Some(fb_rx);
    }

    /// Drains pending flashblocks from the channel unless paused.
    pub(crate) fn poll(&mut self) {
        if self.paused {
            return;
        }

        let flashblocks: Vec<_> = self
            .fb_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for tsf in flashblocks {
            self.add_flashblock(tsf);
        }
    }

    fn evict_old_blocks(&mut self) {
        let mut distinct = 0usize;
        let mut last_block = None;
        let mut keep = self.entries.len();
        for (i, entry) in self.entries.iter().enumerate() {
            if last_block != Some(entry.block_number) {
                distinct += 1;
                last_block = Some(entry.block_number);
                if distinct > MAX_FLASH_BLOCKS {
                    keep = i;
                    break;
                }
            }
        }
        self.entries.truncate(keep);
    }

    /// Processes a received flashblock and updates tracking state.
    pub(crate) fn add_flashblock(&mut self, tsf: TimestampedFlashblock) {
        let TimestampedFlashblock { flashblock: fb, received_at } = tsf;

        self.message_count += 1;

        let block_number = fb.metadata.block_number;
        let index = fb.index;
        if let Some((last_block, last_index)) = self.last_flashblock {
            if block_number == last_block && index > last_index + 1 {
                self.missed_flashblocks += index - last_index - 1;
            } else if block_number > last_block && index > 0 {
                self.missed_flashblocks += index;
            }
        }
        self.last_flashblock = Some((block_number, index));

        let base_fee =
            fb.base.as_ref().map(|base| base.base_fee_per_gas.try_into().unwrap_or(u128::MAX));

        let prev_base_fee = self.current_base_fee;

        if let Some(ref base) = fb.base {
            self.current_gas_limit = base.gas_limit;
            self.current_base_fee = base_fee;
        }

        let time_diff_ms =
            self.entries.front().map(|prev| (received_at - prev.timestamp).num_milliseconds());

        let entry = FlashblockEntry {
            block_number: fb.metadata.block_number,
            index: fb.index,
            tx_count: fb.diff.transactions.len(),
            gas_used: fb.diff.gas_used,
            gas_limit: self.current_gas_limit,
            base_fee,
            prev_base_fee,
            timestamp: received_at,
            time_diff_ms,
        };

        self.entries.push_front(entry);
        self.evict_old_blocks();
    }
}
