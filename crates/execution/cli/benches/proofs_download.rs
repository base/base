//! Benchmarks for the proofs snapshot download on a synthetic archive.
//!
//! `proofs_extract` unpacks a fully downloaded archive, comparing
//! single-threaded decode and unpack with the decode/unpack thread pipeline.
//!
//! `proofs_download` fetches the archive from a local server throttled to
//! [`SERVER_BYTES_PER_SEC`], comparing a download followed by extraction with
//! extraction while the download runs.

use std::{
    fs::File,
    io::{self, BufReader},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base_execution_cli::commands::download::{
    ProofsArchiveAvailability, ProofsArchiveDownload, ProofsArchiveExtractor, ProofsDownloader,
    ProofsManifestEntry,
};
use criterion::{BatchSize, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use futures::stream;
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// Files in the synthetic proofs database.
const FILE_COUNT: usize = 32;

/// Size of each synthetic file.
const FILE_SIZE: usize = 8 << 20;

/// Download piece size, small enough that the archive spans many pieces.
const PIECE_SIZE: u64 = 4 << 20;

/// Parallel Range requests.
const CONCURRENCY: usize = 8;

/// Aggregate rate of the throttled server across all connections.
const SERVER_BYTES_PER_SEC: u64 = 256 << 20;

/// Body chunk size of the throttled server.
const SERVER_CHUNK: usize = 256 << 10;

/// Returns `len` bytes that zstd compresses about 2:1.
fn half_compressible(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if index % 64 < 32 { state as u8 } else { 0 }
        })
        .collect()
}

/// Builds the synthetic `proofs.tar.zst` archive.
fn synthetic_archive() -> Vec<u8> {
    let mut builder = tar::Builder::new(zstd::Encoder::new(Vec::new(), 3).unwrap());
    for index in 0..FILE_COUNT {
        let data = half_compressible(FILE_SIZE, index as u64);
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, format!("proofs/{index:06}.sst"), &data[..]).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Serves `/proofs.tar.zst` Range requests at [`SERVER_BYTES_PER_SEC`] shared
/// evenly across [`CONCURRENCY`] connections.
async fn serve_throttled(State(archive): State<Arc<Vec<u8>>>, headers: HeaderMap) -> Response {
    let Some((start, end)) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok()?.strip_prefix("bytes=")?.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)))
    else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };

    let connection_rate = SERVER_BYTES_PER_SEC as f64 / CONCURRENCY as f64;
    let chunk_delay = Duration::from_secs_f64(SERVER_CHUNK as f64 / connection_rate);
    let total = archive.len();
    let body = stream::unfold(start, move |offset| {
        let archive = Arc::clone(&archive);
        async move {
            if offset > end {
                return None;
            }
            tokio::time::sleep(chunk_delay).await;
            let next = (offset + SERVER_CHUNK).min(end + 1);
            Some((Ok::<_, std::io::Error>(Bytes::copy_from_slice(&archive[offset..next])), next))
        }
    });
    (
        StatusCode::PARTIAL_CONTENT,
        [(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"))],
        Body::from_stream(body),
    )
        .into_response()
}

/// Starts the throttled server on `runtime` and returns its address.
fn start_server(runtime: &Runtime, archive: Arc<Vec<u8>>) -> SocketAddr {
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/proofs.tar.zst", get(serve_throttled)).with_state(archive);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    })
}

/// Unpacks `archive_path` with decoding and unpacking on one thread, reading
/// and draining the archive the same way as the pipeline.
fn extract_single_thread(archive_path: &Path, target_dir: &Path) {
    let reader = BufReader::with_capacity(1 << 20, File::open(archive_path).unwrap());
    let mut decoder = zstd::Decoder::with_buffer(reader).unwrap();
    tar::Archive::new(&mut decoder).unpack(target_dir).unwrap();
    io::copy(&mut decoder, &mut io::sink()).unwrap();
}

/// Unpacks a fully downloaded `archive_path` with the decode/unpack pipeline.
fn extract_pipelined(archive_path: &Path, archive_len: u64, target_dir: &Path) {
    let availability = Arc::new(ProofsArchiveAvailability::new(archive_len));
    availability.publish(archive_len);
    ProofsArchiveExtractor::extract(archive_path, availability, target_dir).unwrap();
}

fn bench_extract(c: &mut Criterion) {
    let archive = synthetic_archive();
    let source = TempDir::new().unwrap();
    let archive_path = source.path().join("proofs.tar.zst");
    std::fs::write(&archive_path, &archive).unwrap();
    let archive_len = archive.len() as u64;

    let mut group = c.benchmark_group("proofs_extract");
    group.sample_size(10).throughput(Throughput::Bytes((FILE_COUNT * FILE_SIZE) as u64));
    group.bench_function("single_thread", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |target| {
                extract_single_thread(&archive_path, target.path());
                target
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function("pipelined", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |target| {
                extract_pipelined(&archive_path, archive_len, target.path());
                target
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn bench_download(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let archive = Arc::new(synthetic_archive());
    let addr = start_server(&runtime, Arc::clone(&archive));
    let entry = ProofsManifestEntry {
        file_name: "proofs.tar.zst".to_string(),
        expected_size: archive.len() as u64,
        archive_url: format!("http://{addr}/proofs.tar.zst"),
        output_files: Vec::new(),
    };

    let mut group = c.benchmark_group("proofs_download");
    group
        .sample_size(10)
        .sampling_mode(SamplingMode::Flat)
        .measurement_time(Duration::from_secs(10))
        .throughput(Throughput::Bytes(archive.len() as u64));
    group.bench_function("download_then_extract", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |target| {
                let cache_dir = target.path().join(".snapshot-cache");
                std::fs::create_dir_all(&cache_dir).unwrap();
                let download =
                    ProofsArchiveDownload::new(&entry, &cache_dir, CONCURRENCY, PIECE_SIZE);
                runtime.block_on(async {
                    let pieces = download.prepare().await.unwrap();
                    let availability = ProofsArchiveAvailability::new(entry.expected_size);
                    download.run(pieces, &availability).await.unwrap();
                });
                extract_pipelined(&download.part_path, entry.expected_size, target.path());
                std::fs::remove_dir_all(&cache_dir).unwrap();
                target
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function("extract_while_downloading", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |target| {
                runtime
                    .block_on(ProofsDownloader::download_and_extract(
                        &entry,
                        target.path(),
                        CONCURRENCY,
                        PIECE_SIZE,
                    ))
                    .unwrap();
                target
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_extract, bench_download);
criterion_main!(benches);
