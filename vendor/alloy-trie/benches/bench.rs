#![allow(missing_docs)]

use std::{hint::black_box, time::Duration};

use alloy_trie::nodes::encode_path_leaf;
use codspeed_criterion_compat::{
    BenchmarkGroup, Criterion, criterion_group, criterion_main, measurement::WallTime,
};
use nybbles::Nibbles;
use proptest::{prelude::*, strategy::ValueTree};

/// Benchmarks the nibble path encoding.
pub fn nibbles_path_encoding(c: &mut Criterion) {
    let lengths = [8u64, 16, 32, 64];

    let mut g = group(c, "encode_path_leaf");
    for len in lengths {
        g.throughput(codspeed_criterion_compat::Throughput::Bytes(len));
        let id = codspeed_criterion_compat::BenchmarkId::new("trie", len);
        g.bench_function(id, |b| {
            let nibbles = &get_nibbles(len as usize);
            b.iter(|| encode_path_leaf(black_box(nibbles), false))
        });
    }
}

fn group<'c>(c: &'c mut Criterion, name: &str) -> BenchmarkGroup<'c, WallTime> {
    let mut g = c.benchmark_group(name);
    g.warm_up_time(Duration::from_secs(1));
    g.noise_threshold(0.02);
    g
}

fn get_nibbles(len: usize) -> Nibbles {
    proptest::arbitrary::any_with::<Nibbles>(len.into())
        .new_tree(&mut Default::default())
        .unwrap()
        .current()
}

criterion_group!(benches, nibbles_path_encoding);
criterion_main!(benches);
