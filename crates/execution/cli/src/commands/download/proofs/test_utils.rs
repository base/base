//! Archive builders and HTTP servers shared by the proofs download tests.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use base_reth_cli::{ManifestGenerationParams, SnapshotGenerator};
use futures::stream;
use tokio::sync::Mutex;

use super::ProofsManifestEntry;

/// Builds a `.tar.zst` archive holding `content_pairs` as `(path, bytes)` entries.
pub(super) fn create_proofs_archive(content_pairs: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = zstd::Encoder::new(&mut buf, 0).unwrap();
    let mut builder = tar::Builder::new(encoder);

    for (path, data) in content_pairs {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }

    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
    buf
}

/// Packages `proofs` with the snapshotter's manifest generator and returns the generated
/// manifest JSON together with the proofs archive bytes.
pub(super) fn generate_proofs_snapshot(proofs: &[(&str, &[u8])]) -> (serde_json::Value, Vec<u8>) {
    let source = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("db")).unwrap();
    std::fs::write(source.path().join("db/mdbx.dat"), b"state-data").unwrap();
    for (path, data) in proofs {
        let path = source.path().join("proofs").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }

    let remote_static_files = std::collections::HashMap::new();
    SnapshotGenerator::generate_manifest(&ManifestGenerationParams {
        source_datadir: source.path(),
        output_dir: Some(output.path()),
        chain_id: 8453,
        base_url: None,
        block: Some(0),
        blocks_per_file: None,
        remote_static_files: &remote_static_files,
        previous_manifest: None,
        upload_proofs: true,
    })
    .unwrap();

    let manifest =
        serde_json::from_slice(&std::fs::read(output.path().join("manifest.json")).unwrap())
            .unwrap();
    let archive = std::fs::read(output.path().join("proofs.tar.zst")).unwrap();
    (manifest, archive)
}

/// Returns a manifest entry for `archive` served at `{base_url}/proofs.tar.zst`.
pub(super) fn proofs_entry(base_url: &str, archive: &[u8]) -> ProofsManifestEntry {
    ProofsManifestEntry {
        file_name: "proofs.tar.zst".to_string(),
        expected_size: archive.len() as u64,
        archive_url: format!("{base_url}/proofs.tar.zst"),
        output_files: Vec::new(),
    }
}

/// Serves `app` on an ephemeral local port and returns its base URL.
pub(super) async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    (base_url, handle)
}

/// Serves `manifest_json` at `/manifest.json` and a Range-aware archive at
/// `/proofs.tar.zst`, returning the manifest URL.
pub(super) async fn start_test_server(
    manifest_json: serde_json::Value,
    archive_bytes: Vec<u8>,
) -> (String, tokio::task::JoinHandle<()>) {
    let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();

    let app = Router::new()
        .route(
            "/manifest.json",
            get(move || {
                let data = manifest_bytes.clone();
                async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], data) }
            }),
        )
        .route("/proofs.tar.zst", get(handle_range))
        .with_state(archive_bytes);

    let (base_url, handle) = serve(app).await;
    (format!("{base_url}/manifest.json"), handle)
}

/// Serves `archive_bytes` at `/proofs.tar.zst`, honoring Range requests.
pub(super) async fn start_range_aware_server(
    archive_bytes: Vec<u8>,
) -> (String, tokio::task::JoinHandle<()>) {
    serve(Router::new().route("/proofs.tar.zst", get(handle_range)).with_state(archive_bytes)).await
}

/// Parses an inclusive `Range: bytes=start-end` header clamped to `len`.
pub(super) fn parse_byte_range(headers: &HeaderMap, len: usize) -> Option<(usize, usize)> {
    let spec = headers.get("Range")?.to_str().ok()?.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() { len.saturating_sub(1) } else { end.parse::<usize>().ok()? };
    let end = end.min(len.saturating_sub(1));
    (start < len && start <= end).then_some((start, end))
}

/// Returns the inclusive byte range `start..=end` of `data` as a 206 response.
pub(super) fn partial_content(data: &[u8], start: usize, end: usize) -> Response {
    (
        StatusCode::PARTIAL_CONTENT,
        [(axum::http::header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", data.len()))],
        data[start..=end].to_vec(),
    )
        .into_response()
}

/// Answers Range requests with 206 and anything else with the whole body.
pub(super) async fn handle_range(State(data): State<Vec<u8>>, headers: HeaderMap) -> Response {
    match parse_byte_range(&headers, data.len()) {
        Some((start, end)) => partial_content(&data, start, end),
        None => (StatusCode::OK, data).into_response(),
    }
}

/// State for [`start_recording_range_server`].
#[derive(Clone)]
pub(super) struct RecordingRangeState {
    data: Vec<u8>,
    requests: Arc<Mutex<Vec<(u64, u64)>>>,
}

/// Serves Range GETs and records each inclusive byte range that was asked for.
pub(super) async fn start_recording_range_server(
    archive_bytes: Vec<u8>,
) -> (String, Arc<Mutex<Vec<(u64, u64)>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/proofs.tar.zst", get(handle_recording_range))
        .with_state(RecordingRangeState { data: archive_bytes, requests: Arc::clone(&requests) });
    let (base_url, handle) = serve(app).await;
    (base_url, requests, handle)
}

async fn handle_recording_range(
    State(state): State<RecordingRangeState>,
    headers: HeaderMap,
) -> Response {
    let Some((start, end)) = parse_byte_range(&headers, state.data.len()) else {
        state.requests.lock().await.push((0, state.data.len().saturating_sub(1) as u64));
        return (StatusCode::OK, state.data).into_response();
    };
    state.requests.lock().await.push((start as u64, end as u64));
    partial_content(&state.data, start, end)
}

/// State for servers that misbehave on the first request only.
#[derive(Clone)]
pub(super) struct FirstRequestState {
    data: Vec<u8>,
    requests: Arc<AtomicUsize>,
}

/// Serves Range GETs, but the first response errors halfway through its body.
pub(super) async fn start_drop_then_range_server(
    archive_bytes: Vec<u8>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    start_first_request_server(archive_bytes, true).await
}

/// Serves Range GETs, but the first response ends cleanly after half its
/// bytes without a `Content-Length`.
pub(super) async fn start_short_then_range_server(
    archive_bytes: Vec<u8>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    start_first_request_server(archive_bytes, false).await
}

async fn start_first_request_server(
    archive_bytes: Vec<u8>,
    fail_stream: bool,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let state = FirstRequestState { data: archive_bytes, requests: Arc::clone(&requests) };
    let app = Router::new()
        .route(
            "/proofs.tar.zst",
            get(move |State(state): State<FirstRequestState>, headers: HeaderMap| async move {
                handle_first_request(state, headers, fail_stream)
            }),
        )
        .with_state(state);
    let (base_url, handle) = serve(app).await;
    (base_url, requests, handle)
}

fn handle_first_request(
    state: FirstRequestState,
    headers: HeaderMap,
    fail_stream: bool,
) -> Response {
    let request_n = state.requests.fetch_add(1, Ordering::SeqCst);
    let Some((start, end)) = parse_byte_range(&headers, state.data.len()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if request_n > 0 {
        return partial_content(&state.data, start, end);
    }

    let len = end - start + 1;
    let drop_at = start + len / 2;
    let first = Ok::<_, std::io::Error>(Bytes::from(state.data[start..drop_at].to_vec()));
    let body = if fail_stream {
        Body::from_stream(stream::iter([
            first,
            Err(std::io::Error::other("error decoding response body")),
        ]))
    } else {
        Body::from_stream(stream::iter([first]))
    };
    (
        StatusCode::PARTIAL_CONTENT,
        [(axum::http::header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", state.data.len()))],
        body,
    )
        .into_response()
}

/// State for [`start_concurrent_range_server`].
#[derive(Clone)]
pub(super) struct ConcurrentRangeState {
    data: Vec<u8>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

/// Serves slow Range GETs and records the peak number of concurrent requests.
pub(super) async fn start_concurrent_range_server(
    archive_bytes: Vec<u8>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/proofs.tar.zst", get(handle_concurrent_range)).with_state(
        ConcurrentRangeState {
            data: archive_bytes,
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::clone(&max_in_flight),
        },
    );
    let (base_url, handle) = serve(app).await;
    (base_url, max_in_flight, handle)
}

async fn handle_concurrent_range(
    State(state): State<ConcurrentRangeState>,
    headers: HeaderMap,
) -> Response {
    let current = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(current, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let response = handle_range(State(state.data), headers).await;
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    response
}
