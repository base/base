//! Component benchmarks for [`BatchEncoder::close_current_channel`] internals.
//!
//! The prior `step_components` bench showed that channel close absorbs ~97.5 % of
//! the total step budget.  This bench isolates the close path so that future
//! compression-finalisation or frame-output optimisations have a quantified
//! before/after reference.

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::RollupConfig;
use base_comp::{BatchComposer, ChannelOut, Config, ShadowCompressor};
use base_protocol::{Batch, ChannelId, L1BlockInfoBedrock, L1BlockInfoTx};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const BLOCK_COUNT: usize = 1_024;
const USER_TXS_PER_BLOCK: usize = 8;
const USER_TX_INPUT_BYTES: usize = 512;

const TARGET_FRAME_SIZE: usize = 120_000;
const MAX_FRAME_SIZE: usize = 128_000;

fn make_deposit_tx() -> BaseTxEnvelope {
    let calldata = L1BlockInfoTx::Bedrock(L1BlockInfoBedrock::default()).encode_calldata();
    BaseTxEnvelope::Deposit(Sealed::new(TxDeposit { input: calldata, ..Default::default() }))
}

fn make_user_tx(seed: u64) -> BaseTxEnvelope {
    let tx = TxLegacy {
        nonce: seed,
        input: Bytes::from(vec![seed as u8; USER_TX_INPUT_BYTES]),
        ..Default::default()
    };
    BaseTxEnvelope::Legacy(tx.into_signed(Signature::test_signature()))
}

fn make_block(parent_hash: B256, number: u64) -> BaseBlock {
    let mut transactions = Vec::with_capacity(1 + USER_TXS_PER_BLOCK);
    transactions.push(make_deposit_tx());
    transactions
        .extend((0..USER_TXS_PER_BLOCK).map(|offset| make_user_tx(number * 100 + offset as u64)));

    BaseBlock {
        header: Header { parent_hash, number, ..Default::default() },
        body: BlockBody { transactions, ..Default::default() },
    }
}

/// Close-channel component: measure the `output_frame` drain loop on a
/// pre-filled [`ShadowCompressor`] channel.
///
/// The step loop (composition + `add_batch`) accounts for only ~1.4 % of the
/// total encoder step budget. This bench isolates the channel-close drain
/// phase — `flush()`, repeated `output_frame` calls, and `Arc::new` frame
/// creation — which absorbs the remaining ~97.5 %.
fn bench_close_channel_drain(c: &mut Criterion) {
    let mut group =
        c.benchmark_group("batcher_encoder/close_channel/components/output_frame_drain");

    // Build a pre-filled channel once per iteration and measure only the drain loop.
    group.bench_function("single_1024_blocks_drain_only", |b| {
        b.iter_batched(
            || {
                // Simulate what close_current_channel does after step() fills the channel.
                let comp_config = Config {
                    target_output_size: TARGET_FRAME_SIZE as u64,
                    approx_compr_ratio: 0.4,
                    kind: base_comp::CompressorType::Shadow,
                    compression_algo: base_comp::CompressionAlgo::Brotli10,
                };
                let comp = ShadowCompressor::from(comp_config);
                let mut ch =
                    ChannelOut::new(ChannelId::default(), Arc::new(RollupConfig::default()), comp);

                // Fill with batches from 1024 blocks.
                let mut parent_hash = B256::ZERO;
                for number in 0..BLOCK_COUNT as u64 {
                    let block = make_block(parent_hash, number);
                    parent_hash = black_box(block.header.hash_slow());
                    let (single, _info) = BatchComposer::block_to_single_batch(&block).unwrap();
                    let _ = ch.add_batch(Batch::Single(single));
                }

                // Flush and close (as close_current_channel does).
                let _ = ch.flush();
                ch.close();

                ch
            },
            |mut ch| {
                // Drain all frames — this is the core of close_current_channel's
                // frame-production loop.
                let mut frames: Vec<Arc<base_protocol::Frame>> = Vec::new();
                while ch.ready_bytes() > 0 {
                    match ch.output_frame(MAX_FRAME_SIZE) {
                        Ok(frame) => frames.push(Arc::new(frame)),
                        Err(_) => break,
                    }
                }
                black_box(frames)
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("span_1024_blocks_drain_only", |b| {
        // For span batches the channel contains fewer, larger compressed batches,
        // so the drain shape is different (fewer frames, larger per-frame data).
        b.iter_batched(
            || {
                let comp_config = Config {
                    target_output_size: TARGET_FRAME_SIZE as u64,
                    approx_compr_ratio: 0.4,
                    kind: base_comp::CompressorType::Shadow,
                    compression_algo: base_comp::CompressionAlgo::Brotli10,
                };
                let comp = ShadowCompressor::from(comp_config);
                let mut ch =
                    ChannelOut::new(ChannelId::default(), Arc::new(RollupConfig::default()), comp);

                // Build a SpanBatch from 1024 blocks and write it to the channel.
                let mut parent_hash = B256::ZERO;
                let mut span = base_protocol::SpanBatch { chain_id: 0, ..Default::default() };
                for number in 0..BLOCK_COUNT as u64 {
                    let block = make_block(parent_hash, number);
                    parent_hash = black_box(block.header.hash_slow());
                    let (single, info) = BatchComposer::block_to_single_batch(&block).unwrap();
                    let seq = info.sequence_number();
                    let _ = span.append_singular_batch(single, seq);
                }
                let _ = ch.add_batch(Batch::Span(span));

                let _ = ch.flush();
                ch.close();

                ch
            },
            |mut ch| {
                let mut frames: Vec<Arc<base_protocol::Frame>> = Vec::new();
                while ch.ready_bytes() > 0 {
                    match ch.output_frame(MAX_FRAME_SIZE) {
                        Ok(frame) => frames.push(Arc::new(frame)),
                        Err(_) => break,
                    }
                }
                black_box(frames)
            },
            BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, bench_close_channel_drain);
criterion_main!(benches);
