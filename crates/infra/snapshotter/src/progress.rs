//! Shared progress-reporting helpers used across snapshot generation and upload.

use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle as StdJoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tracing::{info, warn};

/// Interval between periodic progress logs during long-running snapshot operations
/// (archive compression and artifact upload).
pub(crate) const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);

const UPLOAD_STALL_WARNING_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_STALLED_UPLOADS_IN_LOG: usize = 5;
const INTERACTIVE_UPLOAD_RENDER_INTERVAL: Duration = Duration::from_millis(250);
const INTERACTIVE_UPLOAD_ROWS: usize = 10;
const UPLOAD_PROGRESS_BAR_WIDTH: usize = 24;

const fn percent(done: u64, total: u64) -> u64 {
    if total == 0 { 100 } else { done.saturating_mul(100) / total }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = UNITS[0];
    for next_unit in UNITS.iter().skip(1) {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next_unit;
    }

    if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") }
}

fn progress_bar(done: u64, total: u64) -> String {
    let filled = if total == 0 {
        UPLOAD_PROGRESS_BAR_WIDTH
    } else {
        (((done.min(total) as u128) * (UPLOAD_PROGRESS_BAR_WIDTH as u128)) / (total as u128))
            as usize
    };
    format!(
        "{}{}",
        "#".repeat(filled),
        "-".repeat(UPLOAD_PROGRESS_BAR_WIDTH.saturating_sub(filled))
    )
}

fn truncate_for_display(value: &str, max_chars: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return "...".chars().take(max_chars).collect();
    }

    let front = (max_chars - 3) / 2;
    let back = max_chars - 3 - front;
    format!(
        "{}...{}",
        chars[..front].iter().collect::<String>(),
        chars[chars.len() - back..].iter().collect::<String>()
    )
}

/// Cumulative compression progress shared across every file in a single archive,
/// emitting a throttled log so large single-file archives report progress mid-stream.
#[derive(Debug)]
pub struct ArchiveProgress {
    archive_name: String,
    total_bytes: u64,
    started: Instant,
    last_log: Instant,
    bytes_done: u64,
}

impl ArchiveProgress {
    /// Creates a new tracker for an archive of `total_bytes` total uncompressed bytes.
    pub fn new(archive_name: String, total_bytes: u64) -> Self {
        let now = Instant::now();
        Self { archive_name, total_bytes, started: now, last_log: now, bytes_done: 0 }
    }

    /// Adds `n` newly-compressed bytes, emitting a progress log once per interval.
    pub fn record(&mut self, n: u64) {
        self.bytes_done += n;
        if self.last_log.elapsed() >= PROGRESS_LOG_INTERVAL {
            info!(
                archive = %self.archive_name,
                bytes_done = self.bytes_done,
                total_bytes = self.total_bytes,
                percent = percent(self.bytes_done, self.total_bytes),
                elapsed_secs = self.started.elapsed().as_secs(),
                "compressing archive"
            );
            self.last_log = Instant::now();
        }
    }
}

/// Shared compression progress for a whole chunked snapshot component such as
/// `transactions`, aggregating bytes across every archive compressed in parallel.
#[derive(Clone, Debug)]
pub struct ComponentProgressReporter {
    state: Arc<ComponentProgressState>,
}

impl ComponentProgressReporter {
    /// Registers an active archive within the component and returns a reporter
    /// that streams byte progress into that archive's row.
    pub(crate) fn start_archive(
        &self,
        archive_name: impl Into<String>,
        total_bytes: u64,
    ) -> ArchiveProgressReporter {
        let archive_name = archive_name.into();
        let now = Instant::now();
        if let Ok(mut active_archives) = self.state.active_archives.lock() {
            active_archives.insert(
                archive_name.clone(),
                ActiveArchiveState { total_bytes, bytes_done: 0, started: now },
            );
        }
        ArchiveProgressReporter { component: self.clone(), archive_name }
    }

    /// Adds `n` compressed source bytes to the component-wide total.
    pub fn record(&self, n: u64) {
        self.state.bytes_done.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n` source bytes to one active archive and the component total.
    pub(crate) fn record_archive_bytes(&self, archive_name: &str, n: u64) {
        self.record(n);
        if let Ok(mut active_archives) = self.state.active_archives.lock()
            && let Some(state) = active_archives.get_mut(archive_name)
        {
            state.bytes_done = state.bytes_done.saturating_add(n).min(state.total_bytes);
        }
    }

    /// Marks one archive within the component as fully packaged.
    pub(crate) fn archive_completed(&self, archive_name: &str) {
        if let Ok(mut active_archives) = self.state.active_archives.lock()
            && let Some(state) = active_archives.remove(archive_name)
        {
            let remaining = state.total_bytes.saturating_sub(state.bytes_done);
            if remaining > 0 {
                self.record(remaining);
            }
        }
        self.state.archives_done.fetch_add(1, Ordering::Relaxed);
    }

    /// Removes a failed archive from the active set without counting it complete.
    pub(crate) fn archive_failed(&self, archive_name: &str) {
        if let Ok(mut active_archives) = self.state.active_archives.lock() {
            active_archives.remove(archive_name);
        }
    }
}

/// Per-archive reporter backed by a shared component progress state.
#[derive(Clone, Debug)]
pub(crate) struct ArchiveProgressReporter {
    component: ComponentProgressReporter,
    archive_name: String,
}

impl ArchiveProgressReporter {
    /// Adds `n` source bytes to this archive and the parent component total.
    pub(crate) fn record(&self, n: u64) {
        self.component.record_archive_bytes(&self.archive_name, n);
    }

    /// Marks this archive as fully packaged and removes it from the active set.
    pub(crate) fn finish(&self) {
        self.component.archive_completed(&self.archive_name);
    }

    /// Removes this archive from the active set after a failure.
    pub(crate) fn fail(&self) {
        self.component.archive_failed(&self.archive_name);
    }
}

/// Owns a background logger that periodically reports one progress line for an
/// entire chunked component while its archives are being compressed.
pub struct ComponentProgressLogger {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<StdJoinHandle<()>>,
    reporter: ComponentProgressReporter,
}

impl std::fmt::Debug for ComponentProgressLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentProgressLogger")
            .field("reporter", &self.reporter)
            .finish_non_exhaustive()
    }
}

impl ComponentProgressLogger {
    /// Starts a periodic logger for one chunked component.
    pub fn new(component_name: String, total_bytes: u64, total_archives: usize) -> Self {
        let state = Arc::new(ComponentProgressState {
            component_name,
            total_bytes,
            total_archives,
            started: Instant::now(),
            bytes_done: AtomicU64::new(0),
            archives_done: AtomicU64::new(0),
            active_archives: Mutex::new(HashMap::new()),
            interactive: io::stdout().is_terminal(),
        });
        let reporter = ComponentProgressReporter { state: Arc::clone(&state) };
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let mut rendered_lines = 0usize;
            let tick = if state.interactive {
                INTERACTIVE_UPLOAD_RENDER_INTERVAL
            } else {
                PROGRESS_LOG_INTERVAL
            };
            while stop_rx.recv_timeout(tick).is_err() {
                let bytes_done = state.bytes_done.load(Ordering::Relaxed);
                let archives_done = state.archives_done.load(Ordering::Relaxed);
                let active_archives = state.active_archives.lock().ok().map(|active| {
                    let now = Instant::now();
                    let mut items: Vec<ArchiveSnapshot> = active
                        .iter()
                        .map(|(archive_name, archive)| ArchiveSnapshot {
                            archive_name: archive_name.clone(),
                            total_bytes: archive.total_bytes,
                            bytes_done: archive.bytes_done,
                            age_secs: now.duration_since(archive.started).as_secs(),
                        })
                        .collect();
                    items.sort_unstable_by(|a, b| a.archive_name.cmp(&b.archive_name));
                    items
                });

                if state.interactive {
                    rendered_lines = render_interactive_component(
                        &state.component_name,
                        &active_archives.unwrap_or_default(),
                        InteractiveSummary {
                            done: bytes_done,
                            total_bytes: state.total_bytes,
                            completed: archives_done,
                            total_items: state.total_archives,
                            elapsed_secs: state.started.elapsed().as_secs(),
                            previously_rendered_lines: rendered_lines,
                        },
                    );
                } else {
                    info!(
                        component = %state.component_name,
                        bytes_done,
                        total_bytes = state.total_bytes,
                        percent = percent(bytes_done, state.total_bytes),
                        archives_done,
                        total_archives = state.total_archives,
                        active_archives = active_archives.as_ref().map_or(0, Vec::len),
                        elapsed_secs = state.started.elapsed().as_secs(),
                        "compressing component"
                    );
                }
            }
            if state.interactive {
                clear_interactive_uploads(rendered_lines);
            }
        });
        Self { stop_tx: Some(stop_tx), join_handle: Some(join_handle), reporter }
    }

    /// Returns a cloneable reporter that worker threads can update concurrently.
    pub fn reporter(&self) -> ComponentProgressReporter {
        self.reporter.clone()
    }
}

impl Drop for ComponentProgressLogger {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Shared immutable metadata and atomics backing one component-wide compression logger.
#[derive(Debug)]
pub struct ComponentProgressState {
    /// Snapshot component name such as `headers` or `receipts`.
    pub component_name: String,
    /// Total uncompressed source bytes scheduled for this component.
    pub total_bytes: u64,
    /// Number of archives that will be produced for this component.
    pub total_archives: usize,
    /// Monotonic timestamp when component compression started.
    pub started: Instant,
    /// Aggregate uncompressed source bytes processed across all archives.
    pub bytes_done: AtomicU64,
    /// Number of archives that have fully completed compression.
    pub archives_done: AtomicU64,
    /// Currently active archive rows for the live component renderer.
    pub active_archives: Mutex<HashMap<String, ActiveArchiveState>>,
    /// Whether the component logger is rendering interactively to a TTY.
    pub interactive: bool,
}

/// Live progress state for one in-flight archive within a component.
#[derive(Debug)]
pub struct ActiveArchiveState {
    /// Total uncompressed source bytes for this archive.
    pub total_bytes: u64,
    /// Uncompressed source bytes processed so far for this archive.
    pub bytes_done: u64,
    /// Monotonic timestamp when this archive started compression.
    pub started: Instant,
}

#[derive(Clone, Debug)]
struct ArchiveSnapshot {
    archive_name: String,
    total_bytes: u64,
    bytes_done: u64,
    age_secs: u64,
}

struct InteractiveSummary {
    done: u64,
    total_bytes: u64,
    completed: u64,
    total_items: usize,
    elapsed_secs: u64,
    previously_rendered_lines: usize,
}

/// Cumulative upload progress shared across concurrent artifact uploads. A spawned
/// ticker reads the atomic byte counter and logs throughput once per interval.
#[derive(Debug)]
pub struct UploadProgress {
    uploaded: Arc<AtomicU64>,
    files_completed: Arc<AtomicU64>,
    total_bytes: u64,
    total_files: usize,
    active_uploads: Arc<Mutex<HashMap<String, UploadFileState>>>,
    interactive: bool,
}

impl UploadProgress {
    /// Builds a tracker whose total is the on-disk size of every file to be uploaded.
    /// Propagates any metadata error so the total stays consistent with the sizes the
    /// upload path itself records, preventing logged progress from overshooting 100%.
    pub async fn new(
        static_uploads: &[PathBuf],
        run_uploads: &[PathBuf],
        manifest_path: &Path,
    ) -> Result<Self> {
        let mut total_bytes = 0u64;
        let total_files = static_uploads.len() + run_uploads.len() + 1;
        let files = static_uploads
            .iter()
            .map(PathBuf::as_path)
            .chain(run_uploads.iter().map(PathBuf::as_path))
            .chain(std::iter::once(manifest_path));
        for file in files {
            let meta = tokio::fs::metadata(file)
                .await
                .with_context(|| format!("failed to stat {} for upload total", file.display()))?;
            total_bytes += meta.len();
        }
        Ok(Self {
            uploaded: Arc::new(AtomicU64::new(0)),
            files_completed: Arc::new(AtomicU64::new(0)),
            total_bytes,
            total_files,
            active_uploads: Arc::new(Mutex::new(HashMap::new())),
            interactive: io::stdout().is_terminal(),
        })
    }

    /// Adds `n` successfully-uploaded bytes to the cumulative counter.
    pub fn add(&self, n: u64) {
        self.uploaded.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n` uploaded bytes to the cumulative total and the named active file.
    pub(crate) fn add_for_file(&self, key: &str, n: u64) {
        self.add(n);
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.get_mut(key)
        {
            state.uploaded_bytes = state.uploaded_bytes.saturating_add(n).min(state.total_bytes);
            state.last_update = Instant::now();
            state.stage = UploadStage::Uploading;
        }
    }

    /// Registers an in-flight file upload so interactive rendering can show progress.
    pub(crate) fn start_file(&self, key: impl Into<String>, total_bytes: u64, stage: UploadStage) {
        let key = key.into();
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            let now = Instant::now();
            active_uploads.insert(
                key,
                UploadFileState {
                    total_bytes,
                    uploaded_bytes: 0,
                    stage,
                    started: now,
                    last_update: now,
                },
            );
        }
    }

    /// Updates the current stage for one active file upload.
    pub(crate) fn set_stage(&self, key: &str, stage: UploadStage) {
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.get_mut(key)
        {
            state.stage = stage;
            state.last_update = Instant::now();
        }
    }

    /// Removes a failed file upload from the active set.
    pub(crate) fn fail_file(&self, key: &str) {
        if let Ok(mut active_uploads) = self.active_uploads.lock() {
            active_uploads.remove(key);
        }
    }

    /// Marks one whole artifact as fully uploaded and removes it from the active set.
    pub(crate) fn finish_file(&self, key: &str) {
        if let Ok(mut active_uploads) = self.active_uploads.lock()
            && let Some(state) = active_uploads.remove(key)
        {
            let remaining = state.total_bytes.saturating_sub(state.uploaded_bytes);
            if remaining > 0 {
                self.add(remaining);
            }
        }
        self.files_completed.fetch_add(1, Ordering::Relaxed);
    }

    /// Spawns a background logger or interactive renderer, depending on whether
    /// stdout is a terminal.
    pub(crate) fn spawn_logger(&self) -> UploadProgressLogger {
        let uploaded = Arc::clone(&self.uploaded);
        let files_completed = Arc::clone(&self.files_completed);
        let total_bytes = self.total_bytes;
        let total_files = self.total_files;
        let active_uploads = Arc::clone(&self.active_uploads);
        let interactive = self.interactive;
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut last_done = 0u64;
            let mut stalled_since: Option<Instant> = None;
            let mut last_stall_warning: Option<Instant> = None;
            let mut rendered_lines = 0usize;
            let tick = if interactive {
                INTERACTIVE_UPLOAD_RENDER_INTERVAL
            } else {
                PROGRESS_LOG_INTERVAL
            };

            while stop_rx.recv_timeout(tick).is_err() {
                let done = uploaded.load(Ordering::Relaxed);
                let files_done = files_completed.load(Ordering::Relaxed);
                let active_snapshot = active_uploads.lock().ok().map(|active| {
                    let now = Instant::now();
                    let mut items: Vec<UploadFileSnapshot> = active
                        .iter()
                        .map(|(key, state)| UploadFileSnapshot {
                            key: key.clone(),
                            total_bytes: state.total_bytes,
                            uploaded_bytes: state.uploaded_bytes,
                            stage: state.stage,
                            age_secs: now.duration_since(state.started).as_secs(),
                            idle_secs: now.duration_since(state.last_update).as_secs(),
                        })
                        .collect();
                    items.sort_unstable_by(|a, b| a.key.cmp(&b.key));
                    items
                });
                let active_count = active_snapshot.as_ref().map_or(0, Vec::len);

                if done == last_done && active_count > 0 {
                    let stalled_at = stalled_since.get_or_insert_with(Instant::now);
                    let should_warn = stalled_at.elapsed() >= UPLOAD_STALL_WARNING_AFTER
                        && last_stall_warning.is_none_or(|last_warning| {
                            last_warning.elapsed() >= UPLOAD_STALL_WARNING_AFTER
                        });
                    if should_warn {
                        let stalled_uploads: Vec<String> = active_snapshot
                            .as_ref()
                            .into_iter()
                            .flatten()
                            .take(MAX_STALLED_UPLOADS_IN_LOG)
                            .map(|state| {
                                format!(
                                    "{} [{}] {} / {} (idle {}s)",
                                    state.key,
                                    state.stage.label(),
                                    human_bytes(state.uploaded_bytes),
                                    human_bytes(state.total_bytes),
                                    state.idle_secs
                                )
                            })
                            .collect();
                        warn!(
                            bytes_uploaded = done,
                            total_bytes,
                            files_uploaded = files_done,
                            total_files,
                            stalled_secs = stalled_at.elapsed().as_secs(),
                            active_uploads = active_count,
                            stalled_uploads = ?stalled_uploads,
                            "upload progress is stalled"
                        );
                        last_stall_warning = Some(Instant::now());
                    }
                } else {
                    stalled_since = None;
                    last_stall_warning = None;
                }

                if interactive {
                    rendered_lines = render_interactive_uploads(
                        &active_snapshot.unwrap_or_default(),
                        done,
                        total_bytes,
                        files_done,
                        total_files,
                        started.elapsed().as_secs(),
                        rendered_lines,
                    );
                } else {
                    info!(
                        bytes_uploaded = done,
                        total_bytes,
                        percent = percent(done, total_bytes),
                        files_uploaded = files_done,
                        total_files,
                        active_uploads = active_count,
                        elapsed_secs = started.elapsed().as_secs(),
                        "uploading snapshot artifacts (progress)"
                    );
                }
                last_done = done;
            }
            if interactive {
                clear_interactive_uploads(rendered_lines);
            }
        });

        UploadProgressLogger { stop_tx: Some(stop_tx), join_handle: Some(join_handle) }
    }
}

#[derive(Debug)]
pub(crate) struct UploadProgressLogger {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<StdJoinHandle<()>>,
}

impl UploadProgressLogger {
    /// Stops the background upload logger and clears any interactive rendering.
    pub(crate) fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for UploadProgressLogger {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum UploadStage {
    CreatingMultipart,
    Uploading,
    CompletingMultipart,
}

impl UploadStage {
    const fn label(self) -> &'static str {
        match self {
            Self::CreatingMultipart => "create",
            Self::Uploading => "upload",
            Self::CompletingMultipart => "complete",
        }
    }
}

#[derive(Debug)]
struct UploadFileState {
    total_bytes: u64,
    uploaded_bytes: u64,
    stage: UploadStage,
    started: Instant,
    last_update: Instant,
}

#[derive(Clone, Debug)]
struct UploadFileSnapshot {
    key: String,
    total_bytes: u64,
    uploaded_bytes: u64,
    stage: UploadStage,
    age_secs: u64,
    idle_secs: u64,
}

fn render_interactive_uploads(
    active: &[UploadFileSnapshot],
    done: u64,
    total_bytes: u64,
    files_done: u64,
    total_files: usize,
    elapsed_secs: u64,
    previously_rendered_lines: usize,
) -> usize {
    const TOTAL_LINES: usize = INTERACTIVE_UPLOAD_ROWS + 1;

    let mut stdout = io::stdout().lock();
    let lines_to_move_up = previously_rendered_lines.saturating_sub(1);
    if lines_to_move_up > 0 {
        let _ = write!(stdout, "\x1b[{lines_to_move_up}F");
    }

    let hidden_count = active.len().saturating_sub(INTERACTIVE_UPLOAD_ROWS);
    for row in 0..INTERACTIVE_UPLOAD_ROWS {
        let _ = write!(stdout, "\x1b[2K");
        if let Some(file) = active.get(row) {
            let name = file.key.rsplit('/').next().unwrap_or(&file.key);
            let line = format!(
                "{:<36} [{}] [{}] {:>10} / {:<10} {:>4}% {:>5}s",
                truncate_for_display(name, 36),
                file.stage.label(),
                progress_bar(file.uploaded_bytes, file.total_bytes),
                human_bytes(file.uploaded_bytes),
                human_bytes(file.total_bytes),
                percent(file.uploaded_bytes, file.total_bytes),
                file.age_secs
            );
            let _ = write!(stdout, "{line}");
        }
        let _ = writeln!(stdout);
    }

    let status = format!(
        "files {files_done}/{total_files} | active {}{} | total {} / {} ({}%) | elapsed {}s",
        active.len(),
        if hidden_count > 0 { format!(" (+{hidden_count} hidden)") } else { String::new() },
        human_bytes(done),
        human_bytes(total_bytes),
        percent(done, total_bytes),
        elapsed_secs
    );
    let _ = write!(stdout, "\x1b[2K{status}");
    let _ = stdout.flush();

    TOTAL_LINES
}

fn clear_interactive_uploads(rendered_lines: usize) {
    if rendered_lines == 0 {
        return;
    }

    let mut stdout = io::stdout().lock();
    let lines_to_move_up = rendered_lines.saturating_sub(1);
    if lines_to_move_up > 0 {
        let _ = write!(stdout, "\x1b[{lines_to_move_up}F");
    }
    for line in 0..rendered_lines {
        let _ = write!(stdout, "\x1b[2K");
        if line + 1 < rendered_lines {
            let _ = writeln!(stdout);
        }
    }
    let _ = write!(stdout, "\r");
    let _ = stdout.flush();
}

fn render_interactive_component(
    component_name: &str,
    active: &[ArchiveSnapshot],
    summary: InteractiveSummary,
) -> usize {
    const TOTAL_LINES: usize = INTERACTIVE_UPLOAD_ROWS + 1;

    let mut stdout = io::stdout().lock();
    let lines_to_move_up = summary.previously_rendered_lines.saturating_sub(1);
    if lines_to_move_up > 0 {
        let _ = write!(stdout, "\x1b[{lines_to_move_up}F");
    }

    let hidden_count = active.len().saturating_sub(INTERACTIVE_UPLOAD_ROWS);
    for row in 0..INTERACTIVE_UPLOAD_ROWS {
        let _ = write!(stdout, "\x1b[2K");
        if let Some(archive) = active.get(row) {
            let line = format!(
                "{:<36} [{}] {:>10} / {:<10} {:>4}% {:>5}s",
                truncate_for_display(&archive.archive_name, 36),
                progress_bar(archive.bytes_done, archive.total_bytes),
                human_bytes(archive.bytes_done),
                human_bytes(archive.total_bytes),
                percent(archive.bytes_done, archive.total_bytes),
                archive.age_secs
            );
            let _ = write!(stdout, "{line}");
        }
        let _ = writeln!(stdout);
    }

    let status = format!(
        "component {component_name} | archives {}/{} | active {}{} | total {} / {} ({}%) | elapsed {}s",
        summary.completed,
        summary.total_items,
        active.len(),
        if hidden_count > 0 { format!(" (+{hidden_count} hidden)") } else { String::new() },
        human_bytes(summary.done),
        human_bytes(summary.total_bytes),
        percent(summary.done, summary.total_bytes),
        summary.elapsed_secs
    );
    let _ = write!(stdout, "\x1b[2K{status}");
    let _ = stdout.flush();

    TOTAL_LINES
}
