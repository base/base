use std::collections::VecDeque;

use base_flashtypes::Flashblock;
use tokio::sync::mpsc;

use crate::{
    commands::common::{DaTracker, FlashblockEntry, LoadingState},
    config::ChainConfig,
    l1_client::FullSystemConfig,
    rpc::{BacklogFetchResult, BlobSubmission, BlockDaInfo, TimestampedFlashblock},
};

const MAX_FLASHBLOCKS: usize = 100;

#[derive(Debug)]
pub struct Resources {
    pub config: ChainConfig,
    pub da: DaState,
    pub flash: FlashState,
    pub system_config: Option<FullSystemConfig>,
    sys_config_rx: Option<mpsc::Receiver<FullSystemConfig>>,
}

#[derive(Debug)]
pub struct DaState {
    pub tracker: DaTracker,
    pub loading: Option<LoadingState>,
    pub loaded: bool,
    buffered_flashblocks: Vec<Flashblock>,
    fb_rx: Option<mpsc::Receiver<Flashblock>>,
    sync_rx: Option<mpsc::Receiver<u64>>,
    backlog_rx: Option<mpsc::Receiver<BacklogFetchResult>>,
    block_req_tx: Option<mpsc::Sender<u64>>,
    block_res_rx: Option<mpsc::Receiver<BlockDaInfo>>,
    blob_rx: Option<mpsc::Receiver<BlobSubmission>>,
}

#[derive(Debug)]
pub struct FlashState {
    pub entries: VecDeque<FlashblockEntry>,
    pub current_block: Option<u64>,
    pub current_gas_limit: u64,
    pub current_base_fee: Option<u128>,
    pub message_count: u64,
    pub paused: bool,
    fb_rx: Option<mpsc::Receiver<TimestampedFlashblock>>,
}

impl Resources {
    pub fn new(config: ChainConfig) -> Self {
        let has_op_node = config.op_node_rpc.is_some();
        Self {
            config,
            da: DaState::new(has_op_node),
            flash: FlashState::new(),
            system_config: None,
            sys_config_rx: None,
        }
    }

    pub fn chain_name(&self) -> &str {
        &self.config.name
    }

    pub fn set_sys_config_channel(&mut self, rx: mpsc::Receiver<FullSystemConfig>) {
        self.sys_config_rx = Some(rx);
    }

    pub fn poll_sys_config(&mut self) {
        if let Some(ref mut rx) = self.sys_config_rx
            && let Ok(cfg) = rx.try_recv()
        {
            self.system_config = Some(cfg);
        }
    }
}

impl DaState {
    pub fn new(has_op_node: bool) -> Self {
        Self {
            tracker: DaTracker::new(),
            loading: None,
            loaded: !has_op_node,
            buffered_flashblocks: Vec::new(),
            fb_rx: None,
            sync_rx: None,
            backlog_rx: None,
            block_req_tx: None,
            block_res_rx: None,
            blob_rx: None,
        }
    }

    pub fn set_channels(
        &mut self,
        fb_rx: mpsc::Receiver<Flashblock>,
        sync_rx: mpsc::Receiver<u64>,
        backlog_rx: mpsc::Receiver<BacklogFetchResult>,
        block_req_tx: mpsc::Sender<u64>,
        block_res_rx: mpsc::Receiver<BlockDaInfo>,
        blob_rx: mpsc::Receiver<BlobSubmission>,
    ) {
        self.fb_rx = Some(fb_rx);
        self.sync_rx = Some(sync_rx);
        self.backlog_rx = Some(backlog_rx);
        self.block_req_tx = Some(block_req_tx);
        self.block_res_rx = Some(block_res_rx);
        self.blob_rx = Some(blob_rx);
    }

    pub fn poll(&mut self) {
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
                BacklogFetchResult::Complete(initial) => {
                    self.tracker.set_initial_backlog(initial.safe_block, initial.da_bytes);
                    for fb in std::mem::take(&mut self.buffered_flashblocks) {
                        self.process_flashblock(&fb);
                    }
                    self.loaded = true;
                }
                BacklogFetchResult::Error(_) => {
                    for fb in std::mem::take(&mut self.buffered_flashblocks) {
                        self.process_flashblock(&fb);
                    }
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
            self.tracker.update_block_da(info.block_number, info.da_bytes);
        }

        let safe_blocks: Vec<_> = self
            .sync_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for safe_block in safe_blocks {
            self.tracker.update_safe_head(safe_block);
        }

        let blob_subs: Vec<_> = self
            .blob_rx
            .as_mut()
            .map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect())
            .unwrap_or_default();

        for blob_sub in blob_subs {
            self.tracker.record_l1_blob_submission(&blob_sub);
        }
    }

    fn process_flashblock(&mut self, fb: &Flashblock) {
        let block_number = fb.metadata.block_number;
        let da_bytes: u64 = fb.diff.transactions.iter().map(|tx| tx.len() as u64).sum();

        if fb.index == 0 {
            let prev = self
                .tracker
                .block_contributions
                .front()
                .map(|c| c.block_number)
                .filter(|&prev| prev < block_number);

            self.tracker.add_block(block_number, da_bytes);

            if let (Some(prev_block), Some(tx)) = (prev, &self.block_req_tx) {
                let _: Result<(), _> = tx.try_send(prev_block);
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
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_FLASHBLOCKS),
            current_block: None,
            current_gas_limit: 0,
            current_base_fee: None,
            message_count: 0,
            paused: false,
            fb_rx: None,
        }
    }

    pub fn set_channel(&mut self, fb_rx: mpsc::Receiver<TimestampedFlashblock>) {
        self.fb_rx = Some(fb_rx);
    }

    pub fn poll(&mut self) {
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

    pub fn add_flashblock(&mut self, tsf: TimestampedFlashblock) {
        let TimestampedFlashblock { flashblock: fb, received_at } = tsf;

        self.message_count += 1;

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
        if self.entries.len() > MAX_FLASHBLOCKS {
            self.entries.pop_back();
        }
    }
}
