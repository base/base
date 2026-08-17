//! Benchmarks for decoding a [`Flashblock`] off the wire.
//!
//! `try_decode_message` runs for every flashblock a consumer receives, decompressing
//! (brotli) and JSON-parsing the payload. It is deterministic and CPU-bound, so it is
//! a good per-PR advisory signal for the streaming receive path.

use std::{hint::black_box, io::Write};

use base_common_flashblocks::Flashblock;
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};

/// A representative flashblock payload in the v0.4.1 wire format.
const PAYLOAD_JSON: &str = r#"{
    "payload_id": "0x0000000000000000",
    "index": 0,
    "base": {
        "parent_beacon_block_root": "0x0101010101010101010101010101010101010101010101010101010101010101",
        "parent_hash": "0x0202020202020202020202020202020202020202020202020202020202020202",
        "fee_recipient": "0x0000000000000000000000000000000000000000",
        "prev_randao": "0x0303030303030303030303030303030303030303030303030303030303030303",
        "block_number": "0x9",
        "gas_limit": "0xf4240",
        "timestamp": "0x6553f100",
        "extra_data": "0xaabb",
        "base_fee_per_gas": "0xa"
    },
    "diff": {
        "state_root": "0x0404040404040404040404040404040404040404040404040404040404040404",
        "receipts_root": "0x0505050505050505050505050505050505050505050505050505050505050505",
        "logs_bloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "gas_used": "0x7a120",
        "block_hash": "0x0606060606060606060606060606060606060606060606060606060606060606",
        "transactions": ["0x0102"],
        "withdrawals": [],
        "withdrawals_root": "0x0707070707070707070707070707070707070707070707070707070707070707",
        "blob_gas_used": "0x2c"
    },
    "metadata": {
        "block_number": 1234
    }
}"#;

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
    let plain = Bytes::from(PAYLOAD_JSON.as_bytes().to_vec());
    let brotli = Bytes::from(brotli_compress(PAYLOAD_JSON.as_bytes()));

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
