//! Component benchmarks for [`BatchEncoder`] step-path internals.
//!
//! Splits the step budget into composition (`block_to_single_batch`) and
//! channel compression (`add_batch` to a [`ShadowCompressor`] channel) so that
//! future optimization work has a before/after reference for each stage.

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_batcher_encoder::{BatchEncoder, BatchPipeline, EncoderConfig, StepResult};
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::RollupConfig;
use base_comp::{BatchComposer, ChannelOut, Config, ShadowCompressor};
use base_protocol::{Batch, BatchType, ChannelId, L1BlockInfoBedrock, L1BlockInfoTx};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

const BLOCK_COUNT: usize = 4_096;
const USER_TXS_PER_BLOCK: usize = 8;
const USER_TX_INPUT_BYTES: usize = 512;

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

fn make_encoder(batch_type: BatchType) -> BatchEncoder {
    let config = EncoderConfig {
        target_frame_size: 120_000,
        target_num_frames: 1,
        max_frame_size: 128_000,
        max_channel_duration: u64::MAX,
        sub_safety_margin: 0,
        da_type: base_batcher_encoder::DaType::Calldata,
        batch_type,
        approx_compr_ratio: 0.4,
        max_l1_tx_size_bytes: None,
    };
    BatchEncoder::new(Arc::new(RollupConfig::default()), config)
}

/// Measure composition: `block_to_single_batch` for each block.
fn bench_composition_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("batcher_encoder/step_components/composition");

    group.bench_function("single_4096_blocks", |b| {
        b.iter(|| {
            let mut parent_hash = B256::ZERO;
            for number in 0..BLOCK_COUNT as u64 {
                let block = make_block(parent_hash, number);
                let _hash = black_box(block.header.hash_slow());
                parent_hash = _hash;
                let (_batch, _info) =
                    black_box(BatchComposer::block_to_single_batch(&block).unwrap());
            }
        });
    });
}

/// Measure channel compression: `add_batch` to a [`ShadowCompressor`] channel.
fn bench_add_batch_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("batcher_encoder/step_components/add_batch");

    // Build batches first, then measure `add_batch` in isolation.
    let batches: Vec<Batch> = {
        let mut parent_hash = B256::ZERO;
        let mut result = Vec::with_capacity(BLOCK_COUNT);
        for number in 0..BLOCK_COUNT as u64 {
            let block = make_block(parent_hash, number);
            let _hash = black_box(block.header.hash_slow());
            parent_hash = _hash;
            let (single, _l1_info) =
                black_box(BatchComposer::block_to_single_batch(&block).unwrap());
            result.push(Batch::Single(single));
        }
        result
    };

    group.bench_function("single_4096_batches_into_channel", |b| {
        b.iter_batched(
            || {
                let comp_config = Config {
                    target_output_size: 10_000_000,
                    approx_compr_ratio: 0.4,
                    kind: base_comp::CompressorType::Shadow,
                    compression_algo: base_comp::CompressionAlgo::Zlib,
                };
                ChannelOut::new(
                    ChannelId::default(),
                    Arc::new(RollupConfig::default()),
                    ShadowCompressor::from(comp_config),
                )
            },
            |mut ch| {
                for batch in &batches {
                    let _ = black_box(ch.add_batch(batch.clone()));
                }
                black_box(ch.input_bytes())
            },
            BatchSize::SmallInput,
        );
    });
}

/// Measure the combined encoder step path (sanity check vs existing `step_throughput`).
fn bench_full_steps(c: &mut Criterion) {
    let mut group = c.benchmark_group("batcher_encoder/step_components/full");

    fn steps_until_idle(encoder: &mut BatchEncoder) {
        while let Ok(result) = encoder.step() {
            match result {
                StepResult::Idle => break,
                StepResult::BlockEncoded | StepResult::ChannelClosed => {}
            }
        }
    }

    group.bench_function("single_4096_blocks", |b| {
        b.iter_batched(
            || {
                let mut encoder = make_encoder(BatchType::Single);
                let mut parent_hash = B256::ZERO;
                for number in 0..BLOCK_COUNT as u64 {
                    let block = make_block(parent_hash, number);
                    parent_hash = black_box(block.header.hash_slow());
                    encoder.add_block(block).unwrap();
                }
                encoder
            },
            |mut encoder| {
                steps_until_idle(&mut encoder);
                black_box(encoder.da_backlog_bytes())
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_composition_only, bench_add_batch_only, bench_full_steps);
criterion_main!(benches);
