//! Derivation pipeline benchmarks.

use std::fmt::{self, Display, Formatter};
use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_eips::BlockNumHash;
use base_common_consensus::BaseBlock;
use base_common_genesis::{HardForkConfig, RollupConfig, SystemConfig};
use base_consensus_derive::{
    BatchQueue, ChannelBank, FrameQueue, FrameQueueProvider, L2ChainProvider, NextBatchProvider,
    NextFrameProvider, OriginAdvancer, OriginProvider, PipelineError, PipelineErrorKind,
    PipelineResult, StageReset,
};
use base_protocol::{
    Batch, BatchValidationProvider, BlockInfo, Channel, ChannelId, Frame, L2BlockInfo, SingleBatch,
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const FRAME_DATA_LEN: usize = 256;

#[derive(Debug)]
struct BenchFrameProvider {
    origin: BlockInfo,
}

#[derive(Debug, Clone, Copy)]
struct BenchProviderError;

impl Display for BenchProviderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("bench provider error")
    }
}

impl From<BenchProviderError> for PipelineErrorKind {
    fn from(_: BenchProviderError) -> Self {
        PipelineError::Provider("bench provider error".into()).temp()
    }
}

#[derive(Debug, Default)]
struct BenchBatchProvider {
    origin: Option<BlockInfo>,
}

#[derive(Debug, Clone, Default)]
struct BenchL2Provider;

#[async_trait::async_trait]
impl FrameQueueProvider for BenchFrameProvider {
    type Item = Vec<u8>;

    async fn next_data(&mut self) -> PipelineResult<Self::Item> {
        unreachable!("prune benchmarks pre-populate the frame queue")
    }
}

#[async_trait::async_trait]
impl OriginAdvancer for BenchFrameProvider {
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl NextFrameProvider for BenchFrameProvider {
    async fn next_frame(&mut self) -> PipelineResult<Frame> {
        unreachable!("channel-bank benchmarks call the stage directly")
    }
}

impl OriginProvider for BenchFrameProvider {
    fn origin(&self) -> Option<BlockInfo> {
        Some(self.origin)
    }
}

#[async_trait::async_trait]
impl StageReset for BenchFrameProvider {
    async fn reset(&mut self, _: BlockNumHash, _: SystemConfig) -> PipelineResult<()> {
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

impl OriginProvider for BenchBatchProvider {
    fn origin(&self) -> Option<BlockInfo> {
        self.origin
    }
}

#[async_trait::async_trait]
impl OriginAdvancer for BenchBatchProvider {
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl NextBatchProvider for BenchBatchProvider {
    fn flush(&mut self) {}

    fn span_buffer_size(&self) -> usize {
        0
    }

    async fn next_batch(&mut self, _: L2BlockInfo, _: &[BlockInfo]) -> PipelineResult<Batch> {
        Err(PipelineError::Eof.temp())
    }
}

#[async_trait::async_trait]
impl StageReset for BenchBatchProvider {
    async fn reset(&mut self, _: BlockNumHash, _: SystemConfig) -> PipelineResult<()> {
        Ok(())
    }

    async fn activate(&mut self) -> PipelineResult<()> {
        Ok(())
    }

    async fn flush_channel(&mut self) -> PipelineResult<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl BatchValidationProvider for BenchL2Provider {
    type Error = BenchProviderError;

    async fn l2_block_info_by_number(&mut self, _: u64) -> Result<L2BlockInfo, Self::Error> {
        Err(BenchProviderError)
    }

    async fn block_by_number(&mut self, _: u64) -> Result<BaseBlock, Self::Error> {
        Err(BenchProviderError)
    }
}

#[async_trait::async_trait]
impl L2ChainProvider for BenchL2Provider {
    type Error = BenchProviderError;

    async fn system_config_by_number(
        &mut self,
        _: u64,
        _: Arc<RollupConfig>,
    ) -> Result<SystemConfig, <Self as L2ChainProvider>::Error> {
        Err(BenchProviderError)
    }
}

fn holocene_config() -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        hardforks: HardForkConfig { holocene_time: Some(0), ..Default::default() },
        ..Default::default()
    })
}

fn canyon_config() -> Arc<RollupConfig> {
    Arc::new(RollupConfig {
        hardforks: HardForkConfig { canyon_time: Some(0), ..Default::default() },
        ..Default::default()
    })
}

fn new_frame_queue(frames: Vec<Frame>) -> FrameQueue<BenchFrameProvider> {
    let provider = BenchFrameProvider { origin: BlockInfo::default() };
    let mut queue = FrameQueue::new(provider, holocene_config());
    queue.queue.extend(frames);
    queue
}

fn new_channel_bank(channel_count: u16) -> ChannelBank<BenchFrameProvider> {
    let provider = BenchFrameProvider { origin: BlockInfo::default() };
    let mut bank = ChannelBank::new(Arc::new(RollupConfig::default()), provider);
    for seed in 0..channel_count {
        let id = channel_id(seed);
        let mut channel = Channel::new(id, BlockInfo::default());
        channel.add_frame(new_frame(id, 0, false), BlockInfo::default()).unwrap();
        bank.total_size += channel.size();
        bank.channel_queue.push_back(id);
        bank.channels.insert(id, channel);
    }
    bank
}

fn new_channel_bank_ready_last(channel_count: u16) -> ChannelBank<BenchFrameProvider> {
    let provider = BenchFrameProvider { origin: BlockInfo::default() };
    let mut bank = ChannelBank::new(canyon_config(), provider);
    for seed in 0..channel_count {
        let id = channel_id(seed);
        let mut channel = Channel::new(id, BlockInfo::default());
        channel
            .add_frame(new_frame(id, 0, seed == channel_count - 1), BlockInfo::default())
            .unwrap();
        bank.total_size += channel.size();
        bank.channel_queue.push_back(id);
        bank.channels.insert(id, channel);
    }
    bank
}

fn new_batch_queue(span_count: u64) -> BatchQueue<BenchBatchProvider, BenchL2Provider> {
    let prev = BenchBatchProvider { origin: Some(BlockInfo::default()) };
    let mut queue = BatchQueue::new(Arc::new(RollupConfig::default()), prev, BenchL2Provider);
    queue.next_spans =
        (0..span_count).map(|timestamp| SingleBatch { timestamp, ..Default::default() }).collect();
    queue
}

const fn channel_id(seed: u16) -> ChannelId {
    let [first, second] = seed.to_be_bytes();
    let mut id = [0; 16];
    let mut i = 0;
    while i < id.len() {
        id[i] = if i % 2 == 0 { first } else { second };
        i += 1;
    }
    id
}

fn new_frame(id: ChannelId, number: u16, is_last: bool) -> Frame {
    Frame::new(id, number, vec![number as u8; FRAME_DATA_LEN], is_last)
}

fn valid_channel(frame_count: u16) -> Vec<Frame> {
    let id = channel_id(1);
    (0..frame_count).map(|number| new_frame(id, number, number == frame_count - 1)).collect()
}

fn alternating_incomplete_channels(channel_count: u16, frames_per_channel: u16) -> Vec<Frame> {
    let mut frames = Vec::with_capacity(channel_count as usize * frames_per_channel as usize);
    for channel in 0..channel_count {
        for number in 0..frames_per_channel {
            frames.push(new_frame(channel_id(channel), number, false));
        }
    }
    frames
}

fn frame_queue_prune_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("derive_frame_queue_prune");

    group.bench_function("valid_512", |b| {
        let frames = valid_channel(512);
        b.iter_batched(
            || new_frame_queue(frames.clone()),
            |mut queue| queue.prune(BlockInfo::default()),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("incomplete_channels_512", |b| {
        let frames = alternating_incomplete_channels(64, 8);
        b.iter_batched(
            || new_frame_queue(frames.clone()),
            |mut queue| queue.prune(BlockInfo::default()),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn channel_bank_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("derive_channel_bank");

    group.bench_function("prune_4096_channels_below_limit", |b| {
        b.iter_custom(|iters| {
            let mut bank = new_channel_bank(4096);
            let start = Instant::now();
            for _ in 0..iters {
                black_box(bank.prune()).unwrap();
            }
            start.elapsed()
        });
    });

    group.bench_function("ingest_frame_4096_channels", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut remaining = iters;
            while remaining > 0 {
                let batch_iters = remaining.min(u64::from(u16::MAX - 1));
                let mut bank = new_channel_bank(4096);
                let id = channel_id(4095);

                let start = Instant::now();
                for number in 1..=batch_iters as u16 {
                    black_box(bank.ingest_frame(black_box(new_frame(id, number, false)))).unwrap();
                }
                total += start.elapsed();
                remaining -= batch_iters;
            }
            total
        });
    });

    group.bench_function("read_ready_last_4096_channels", |b| {
        b.iter_batched(
            || new_channel_bank_ready_last(4096),
            |mut bank| black_box(bank.read()).unwrap().unwrap(),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn batch_queue_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("derive_batch_queue");

    group.bench_function("pop_cached_span_batches_4096", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            let mut remaining = iters;
            while remaining > 0 {
                let batch_iters = remaining.min(4096);
                let mut queue = new_batch_queue(batch_iters);
                let parent = L2BlockInfo::default();

                let start = Instant::now();
                for _ in 0..batch_iters {
                    black_box(queue.pop_next_batch(parent)).unwrap();
                }
                total += start.elapsed();
                remaining -= batch_iters;
            }
            total
        });
    });

    group.finish();
}

criterion_group!(benches, frame_queue_prune_benches, channel_bank_benches, batch_queue_benches);
criterion_main!(benches);
