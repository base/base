//! Benchmarks for decoding a [`Flashblock`] off the wire.
//!
//! `try_decode_message` runs for every flashblock a consumer receives, decompressing
//! (brotli) and JSON-parsing the payload. It is deterministic and CPU-bound, so it is
//! a good per-PR advisory signal for the streaming receive path.
//!
//! The fixture is a *representative* flashblock rather than a minimal one. A busy Base
//! flashblock slice carries on the order of a hundred transactions plus a receipt per
//! transaction, and decode cost is dominated by hex-decoding those transaction blobs
//! and walking that metadata — not by parsing the fixed header. A single tiny
//! transaction would mostly measure header overhead and make the benchmark
//! insensitive to the work the receive path actually does, so the fixture is sized to
//! that shape via [`Fixture::TRANSACTION_COUNT`] and [`Fixture::TRANSACTION_SIZE_BYTES`].

use std::{hint::black_box, io::Write};

use alloy_primitives::{Address, B256, Bloom, Bytes as PrimitiveBytes, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblocksPayloadV1,
};
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::{Map, Value, json};

/// Builds a representative flashblock fixture for the decode benchmark.
struct Fixture;

impl Fixture {
    /// Transactions in the fixture — the order of magnitude of a busy Base flashblock slice.
    const TRANSACTION_COUNT: usize = 150;
    /// Body size of each synthetic transaction, in bytes — a typical ERC-20 transfer /
    /// small swap calldata footprint. Only length and count drive decode cost; the
    /// contents are irrelevant.
    const TRANSACTION_SIZE_BYTES: usize = 256;

    /// The representative first flashblock, serialized to its JSON wire bytes.
    fn payload_json() -> Vec<u8> {
        serde_json::to_vec(&Self::payload()).expect("serialize fixture payload")
    }

    /// A representative first flashblock: base header, a full transaction set, and
    /// v0.5.0-style metadata carrying one receipt per transaction.
    fn payload() -> FlashblocksPayloadV1 {
        let transactions =
            (0..Self::TRANSACTION_COUNT).map(Self::transaction).collect::<Vec<_>>();

        FlashblocksPayloadV1 {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::from([1u8; 32]),
                parent_hash: B256::from([2u8; 32]),
                fee_recipient: Address::ZERO,
                prev_randao: B256::from([3u8; 32]),
                block_number: 9,
                gas_limit: 1_000_000,
                timestamp: 1_700_000_000,
                extra_data: PrimitiveBytes::from(vec![0xAA, 0xBB]),
                base_fee_per_gas: U256::from(10u64),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::from([4u8; 32]),
                receipts_root: B256::from([5u8; 32]),
                logs_bloom: Bloom::default(),
                gas_used: 500_000,
                block_hash: B256::from([6u8; 32]),
                transactions,
                withdrawals: Vec::new(),
                withdrawals_root: B256::from([7u8; 32]),
                blob_gas_used: Some(44),
            },
            metadata: Self::metadata(),
        }
    }

    /// v0.5.0-style metadata with one receipt per transaction, keyed by a per-index
    /// transaction hash, mirroring the shape a real consumer must parse.
    fn metadata() -> Value {
        let receipts = (0..Self::TRANSACTION_COUNT)
            .map(|index| {
                let hash = B256::from([index as u8; 32]);
                let receipt = json!({
                    "type": "0x2",
                    "status": true,
                    "cumulativeGasUsed": "0x5208",
                    "logs": [],
                });
                (hash.to_string(), receipt)
            })
            .collect::<Map<String, Value>>();

        json!({
            "block_number": 1234,
            "receipts": receipts,
            "new_account_balances": {},
        })
    }

    /// A deterministic synthetic transaction body of [`Self::TRANSACTION_SIZE_BYTES`]
    /// bytes. The contents are seeded from `index` so the fixture is stable run to run.
    fn transaction(index: usize) -> PrimitiveBytes {
        let mut body = Vec::with_capacity(Self::TRANSACTION_SIZE_BYTES);
        let mut state = index as u8;
        for _ in 0..Self::TRANSACTION_SIZE_BYTES {
            state = state.wrapping_mul(31).wrapping_add(17);
            body.push(state);
        }
        PrimitiveBytes::from(body)
    }
}

/// Brotli-compress with the same parameters the flashblocks publisher uses.
fn brotli_compress(data: &[u8]) -> Vec<u8> {
    let mut compressed = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
        writer.write_all(data).expect("write compressed payload");
    }
    compressed
}

fn bench_flashblock_decode(c: &mut Criterion) {
    let json = Fixture::payload_json();
    let plain = Bytes::from(json.clone());
    let brotli = Bytes::from(brotli_compress(&json));

    // Fail loudly at setup if the fixture stops decoding, rather than silently
    // benchmarking an error path.
    Flashblock::try_decode_message(plain.clone()).expect("plain payload decodes");
    Flashblock::try_decode_message(brotli.clone()).expect("brotli payload decodes");

    let mut group = c.benchmark_group("flashblock_decode");
    group.bench_function("plain_json", |b| {
        b.iter(|| black_box(Flashblock::try_decode_message(black_box(plain.clone())).unwrap()));
    });
    group.bench_function("brotli", |b| {
        b.iter(|| black_box(Flashblock::try_decode_message(black_box(brotli.clone())).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, bench_flashblock_decode);
criterion_main!(benches);
