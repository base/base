//! Deterministic instruction-count benchmarks for decoding a [`Flashblock`].
//!
//! The `iai-callgrind` counterpart to `flashblock_decode.rs`. `try_decode_message`
//! (brotli decompress + JSON parse) is deterministic and CPU-bound over a fixed,
//! representative fixture, so its retired instruction count is a faithful, zero-variance
//! proxy for wall-clock time. Building and compressing the fixture runs in the unmeasured
//! setup phase so only the decode is counted. See `flashblock_decode.rs` for why the
//! fixture is sized to a busy Base flashblock slice.

// iai-callgrind's `library_benchmark` / `library_benchmark_group` macros expand to
// undocumented modules, functions, and constants that `-D warnings` rejects. Benches
// are not part of the crate's public API, so this file carries an approved exception to
// the workspace's no-`allow(missing_docs)` rule rather than documenting generated code.
#![allow(missing_docs)]

use std::{hint::black_box, io::Write};

use alloy_primitives::{Address, B256, Bloom, Bytes as PrimitiveBytes, U256};
use alloy_rpc_types_engine::PayloadId;
use base_common_flashblocks::{
    ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, FlashblocksPayloadV1,
};
use bytes::Bytes;
use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use serde_json::{Map, Value, json};

/// Builds a representative flashblock fixture for the decode benchmark.
struct Fixture;

impl Fixture {
    /// Transactions in the fixture — the order of magnitude of a busy Base flashblock slice.
    const TRANSACTION_COUNT: usize = 150;
    /// Body size of each synthetic transaction, in bytes.
    const TRANSACTION_SIZE_BYTES: usize = 256;

    /// The representative first flashblock, serialized to its JSON wire bytes.
    fn payload_json() -> Vec<u8> {
        serde_json::to_vec(&Self::payload()).expect("serialize fixture payload")
    }

    /// The plain-JSON wire payload, ready to feed the decoder.
    fn plain() -> Bytes {
        Bytes::from(Self::payload_json())
    }

    /// The brotli-compressed wire payload, ready to feed the decoder.
    fn brotli() -> Bytes {
        let mut compressed = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(&Self::payload_json()).expect("write compressed payload");
        }
        Bytes::from(compressed)
    }

    /// A representative first flashblock: base header, a full transaction set, and
    /// v0.5.0-style metadata carrying one receipt per transaction.
    fn payload() -> FlashblocksPayloadV1 {
        let transactions = (0..Self::TRANSACTION_COUNT).map(Self::transaction).collect::<Vec<_>>();

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
    /// bytes, seeded from `index` so the fixture is stable run to run.
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

#[library_benchmark]
#[bench::plain_json(Fixture::plain())]
#[bench::brotli(Fixture::brotli())]
fn decode(payload: Bytes) {
    black_box(Flashblock::try_decode_message(black_box(payload)).unwrap());
}

library_benchmark_group!(
    name = flashblock_decode;
    benchmarks = decode
);

main!(library_benchmark_groups = flashblock_decode);
