//! Opt-in run of the writer, reader, catalog CAS, pruner and GC against a real S3-compatible
//! store (`MinIO`).
//!
//! Ignored by default. Start `MinIO` and create a bucket, then run:
//!
//! ```text
//! podman run -d --rm --name history-store-minio -p 9000:9000 \
//!   -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!   minio/minio server /data
//! HISTORY_STORE_MINIO_ENDPOINT=http://127.0.0.1:9000 HISTORY_STORE_MINIO_BUCKET=history \
//! AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1 \
//!   cargo test -p base-history-store --features test-utils --test minio -- --ignored
//! ```

use std::{sync::Arc, time::Duration};

use base_history_store::{
    ArchiveReader, ArchiveStore, ArchiveWriter, CacheConfig, Catalog, CatalogStore,
    ChainIdentity, ChunkVerifier, HistoryStoreError, OrphanGc, Pruner, ReaderConfig,
    SegmentObjects, SourcePool, StepOutcome, WriterConfig,
    test_utils::{ChainFixture, FIXTURE_CHAIN_ID, FixtureSource, test_index_options},
};
use object_store::{ObjectStoreExt, PutPayload};

/// Blocks per segment.
const N: u64 = 4;
/// Segments the writer publishes.
const SEGMENTS: u64 = 3;
/// Upper bound for the whole run.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// Opens the `MinIO` archive under a fresh prefix, or `None` when the env is not configured.
fn minio() -> Option<ArchiveStore> {
    let endpoint = std::env::var("HISTORY_STORE_MINIO_ENDPOINT").ok()?;
    let bucket = std::env::var("HISTORY_STORE_MINIO_BUCKET").ok()?;
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    Some(
        ArchiveStore::from_url(&format!("s3://{bucket}/run-{run}"), Some(&endpoint))
            .expect("minio store opens"),
    )
}

/// Writer, reader, CAS, retirement and GC all behave on `MinIO` as on the in-memory store.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs MinIO: set HISTORY_STORE_MINIO_ENDPOINT and HISTORY_STORE_MINIO_BUCKET"]
async fn archive_lifecycle_on_minio() {
    let archive = minio().expect("HISTORY_STORE_MINIO_ENDPOINT and _BUCKET must be set");
    tokio::time::timeout(RUN_TIMEOUT, lifecycle(archive)).await.expect("minio run timed out");
}

/// Body of [`archive_lifecycle_on_minio`].
async fn lifecycle(archive: ArchiveStore) {
    let fixture = ChainFixture::new(SEGMENTS * N, 2);
    let chain = ChainIdentity {
        chain_id: FIXTURE_CHAIN_ID,
        genesis_hash: fixture.blocks[0].hash,
        blocks_per_segment: N,
    };
    let writer = ArchiveWriter::open(
        Arc::clone(&archive.store),
        archive.layout.clone(),
        chain,
        SourcePool::new(vec![Arc::new(FixtureSource { fixture: fixture.clone() })]),
        ChunkVerifier::new(fixture.chain_spec.clone()),
        WriterConfig::default(),
        test_index_options(),
    )
    .await
    .expect("writer opens on minio");

    for k in 0..SEGMENTS {
        let outcome = writer.step().await.expect("writer step");
        assert!(
            matches!(outcome, StepOutcome::Published { segment, h, .. } if segment == k && h == (k + 1) * N - 1),
            "segment {k} must publish with H = {}, got {outcome:?}",
            (k + 1) * N - 1
        );
    }
    let outcome = writer.step().await.expect("writer step after tip");
    assert!(
        matches!(outcome, StepOutcome::NotReady { needed, finalized: Some(f) } if needed == SEGMENTS * N + N - 1 && f == fixture.tip()),
        "no further segment is finalized, got {outcome:?}"
    );

    // Reader parity.
    let reader = ArchiveReader::open(
        Arc::clone(&archive.store),
        archive.layout.clone(),
        ReaderConfig {
            chain_id: FIXTURE_CHAIN_ID,
            genesis_hash: fixture.blocks[0].hash,
            chain_spec: fixture.chain_spec.clone(),
            cache: CacheConfig::default(),
            index: test_index_options(),
        },
    )
    .await
    .expect("reader opens on minio");
    let snapshot = reader.snapshot().await.expect("snapshot");
    assert_eq!(snapshot.h, Some(SEGMENTS * N - 1), "reader sees H");
    assert_eq!(snapshot.e, 0, "nothing retired yet");
    for expected in &fixture.blocks {
        let n = expected.header.number;
        let block = reader.block(&snapshot, n).await.expect("block");
        assert_eq!(block.hash, expected.hash, "hash of block {n}");
        assert_eq!(block.transactions, expected.transactions, "transactions of block {n}");
        let receipts = reader.receipts(&snapshot, n).await.expect("receipts");
        assert_eq!(receipts, expected.receipts, "receipts of block {n}");
        assert_eq!(
            reader.block_number(&snapshot, expected.hash).await.expect("b‖ lookup"),
            Some(n),
            "hash index of block {n}"
        );
    }

    // Catalog CAS: two updates from the same version, exactly one wins.
    let catalogs = CatalogStore::new(Arc::clone(&archive.store), archive.layout.clone());
    let current = catalogs.load_required().await.expect("catalog");
    let next = Catalog { generation: current.catalog.generation + 1, ..current.catalog.clone() };
    let (first, second) =
        tokio::join!(catalogs.update(&current, next.clone()), catalogs.update(&current, next));
    let results = [first, second];
    let wins = results.iter().filter(|result| result.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|result| matches!(result, Err(HistoryStoreError::CasConflict)))
        .count();
    assert_eq!((wins, conflicts), (1, 1), "exactly one CAS wins and one conflicts: {results:?}");

    // Retire segment 0, sweep it with no grace, and check readers see E move.
    let objects = SegmentObjects::new(Arc::clone(&archive.store), archive.layout.clone());
    let pruner = Pruner::new(catalogs.clone(), objects.clone(), writer.epoch, 4);
    pruner.retire(0, 1).await.expect("retire segment 0");
    let swept = pruner.sweep(&writer.index, Duration::ZERO, 2).await.expect("sweep");
    assert_eq!(swept, vec![0], "segment 0 is swept after a zero grace period");
    let listed = objects.list_all().await.expect("list");
    assert!(listed.iter().all(|(k, _)| *k != 0), "no segment 0 objects remain: {listed:?}");
    let snapshot = reader.snapshot().await.expect("snapshot after retirement");
    assert_eq!(snapshot.e, N, "E moves to the first block of segment 1");
    let err = reader.block(&snapshot, 0).await.expect_err("retired block");
    assert!(
        matches!(err, HistoryStoreError::HistoryPruned { block: 0, earliest } if earliest == N),
        "retired blocks are reported as pruned, got {err}"
    );
    reader.block(&snapshot, N).await.expect("segment 1 still readable");

    // An orphan of an older epoch above H is collected; published segments are kept.
    let orphan = archive.layout.segment_object(SEGMENTS + 5, writer.epoch - 1, "headers");
    archive.store.put(&orphan, PutPayload::from_static(b"orphan")).await.expect("put orphan");
    let report = OrphanGc::new(catalogs.clone(), objects.clone(), writer.epoch)
        .run(&writer.index, Duration::ZERO, chrono::Utc::now() + chrono::Duration::seconds(5))
        .await
        .expect("gc");
    assert_eq!(report.deleted, vec![orphan], "only the orphan is deleted");
    let remaining: Vec<u64> = objects.list_all().await.expect("list").into_iter().map(|(k, _)| k).collect();
    assert!(
        remaining.iter().all(|k| (1..SEGMENTS).contains(k)),
        "only published, unretired segments remain: {remaining:?}"
    );

    // A newer claim fences the writer's catalog updates on the real backend.
    let before = catalogs.load_required().await.expect("catalog before claim");
    let claimed = catalogs.claim_at(writer.epoch + 1).await.expect("claim");
    assert_eq!(claimed.catalog.writer_epoch, writer.epoch + 1, "claim records the newer epoch");
    let err = writer
        .publish(before, SEGMENTS, SEGMENTS * N + N - 1, Default::default())
        .await
        .expect_err("superseded writer must not publish");
    assert!(matches!(err, HistoryStoreError::Fenced { .. }), "superseded writer is fenced: {err}");
    assert_eq!(
        catalogs.load_required().await.expect("catalog").catalog.h,
        Some(SEGMENTS * N - 1),
        "H unchanged after the fenced publish"
    );

    writer.index.close().await.expect("close index");
    reader.index.close().await.expect("close reader index");
}
