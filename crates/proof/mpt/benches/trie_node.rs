//! Benchmarks for the [`TrieNode`].

use std::hint::black_box;

use alloy_trie::Nibbles;
use base_proof_mpt::{NoopTrieProvider, TrieNode};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rand::{Rng, SeedableRng, rngs::StdRng, seq::IteratorRandom};

fn trie(c: &mut Criterion) {
    let mut g = c.benchmark_group("execution");
    g.sample_size(10);

    // Use pseudo-randomness for reproducibility. Both the key set and the deleted /
    // retrieved subsets are drawn from this seeded generator so a base-vs-head
    // comparison exercises the identical workload on each side.
    let mut rng = StdRng::seed_from_u64(42);

    g.bench_function("Insertion - 4096 nodes", |b| {
        let keys = (0..2usize.pow(12))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();

        b.iter(|| {
            let mut trie = TrieNode::Empty;
            for key in &keys {
                trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
            }
        });
    });

    g.bench_function("Insertion - 65,536 nodes", |b| {
        let keys = (0..2usize.pow(16))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();

        b.iter(|| {
            let mut trie = TrieNode::Empty;
            for key in &keys {
                trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
            }
        });
    });

    g.bench_function("Delete 16 nodes - 4096 nodes", |b| {
        let keys = (0..2usize.pow(12))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let keys_to_delete = keys.iter().copied().choose_multiple(&mut rng, 16);

        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        // Clone the populated trie in the setup closure so the deep-clone cost is
        // excluded from the measured deletion work.
        b.iter_batched(
            || trie.clone(),
            |mut trie| {
                for key in &keys_to_delete {
                    trie.delete(key, &NoopTrieProvider).unwrap();
                }
            },
            BatchSize::LargeInput,
        );
    });

    g.bench_function("Delete 16 nodes - 65,536 nodes", |b| {
        let keys = (0..2usize.pow(16))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let keys_to_delete = keys.iter().copied().choose_multiple(&mut rng, 16);

        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        b.iter_batched(
            || trie.clone(),
            |mut trie| {
                for key in &keys_to_delete {
                    trie.delete(key, &NoopTrieProvider).unwrap();
                }
            },
            BatchSize::LargeInput,
        );
    });

    g.bench_function("Open 1024 nodes - 4096 nodes", |b| {
        let keys = (0..2usize.pow(12))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let keys_to_retrieve = keys.iter().copied().choose_multiple(&mut rng, 1024);

        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        b.iter(|| {
            for key in &keys_to_retrieve {
                black_box(trie.open(key, &NoopTrieProvider).unwrap());
            }
        });
    });

    g.bench_function("Open 1024 nodes - 65,536 nodes", |b| {
        let keys = (0..2usize.pow(16))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let keys_to_retrieve = keys.iter().copied().choose_multiple(&mut rng, 1024);

        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        b.iter(|| {
            for key in &keys_to_retrieve {
                black_box(trie.open(key, &NoopTrieProvider).unwrap());
            }
        });
    });

    g.bench_function("Compute root, fully open trie - 4096 nodes", |b| {
        let keys = (0..2usize.pow(12))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        // `blind` takes `&self` and returns the root hash, so no clone is needed;
        // black-box the result so the computation is not optimized away.
        b.iter(|| {
            black_box(trie.blind());
        });
    });

    g.bench_function("Compute root, fully open trie - 65,536 nodes", |b| {
        let keys = (0..2usize.pow(16))
            .map(|_| Nibbles::unpack(rng.random::<[u8; 32]>()))
            .collect::<Vec<_>>();
        let mut trie = TrieNode::Empty;
        for key in &keys {
            trie.insert(key, key.to_vec().into(), &NoopTrieProvider).unwrap();
        }

        b.iter(|| {
            black_box(trie.blind());
        });
    });
}

criterion_group! {
    name = trie_benches;
    config = Criterion::default();
    targets = trie
}
criterion_main!(trie_benches);
