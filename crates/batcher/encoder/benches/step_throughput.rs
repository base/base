//! Benchmarks for [`BatchEncoder`] step-path throughput.
//!
//! Measures how the encoder `step()` performs across different backlog depths
//! and batch types, exercising the per-block `block_da_backlog_bytes` cache.

use std::{hint::black_box, sync::Arc};

use alloy_consensus::{BlockBody, Header, SignableTransaction, TxLegacy};
use alloy_primitives::{B256, Bytes, Sealed, Signature};
use base_batcher_encoder::{BatchEncoder, BatchPipeline, EncoderConfig, StepResult};
use base_common_consensus::{BaseBlock, BaseTxEnvelope, TxDeposit};
use base_common_genesis::RollupConfig;
use base_protocol::{BatchType, L1BlockInfoBedrock, L1BlockInfoTx};
use criterion::{Criterion, criterion_group, criterion_main};

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

fn steps_until_idle(encoder: &mut BatchEncoder) {
    while let Ok(result) = encoder.step() {
        match result {
            StepResult::Idle => break,
            StepResult::BlockEncoded | StepResult::ChannelClosed => {}
        }
    }
}

fn bench_step_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("batcher_encoder/step_throughput");
    group.sample_size(20);

    // Single-batch mode: step through all blocks and drain
    group.bench_function("single_batch_4096_blocks", |b| {
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
                black_box(encoder.da_backlog_bytes());
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Span-batch mode: accumulate and flush span batches
    group.bench_function("span_batch_4096_blocks", |b| {
        b.iter_batched(
            || {
                let mut encoder = make_encoder(BatchType::Span);
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
                black_box(encoder.da_backlog_bytes());
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_step_throughput);
criterion_main!(benches);
