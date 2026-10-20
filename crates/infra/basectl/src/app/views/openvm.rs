//! Live `OpenVM` range prove of one L2 block: witness dump, then STARK and EVM
//! proving on an Axiom Edge cluster.

use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, LineGauge, Padding, Paragraph},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::mpsc,
    task::JoinHandle,
};
use url::Url;

use crate::{
    app::{Action, Resources, View},
    config::MonitoringConfig,
    output::{COLOR_ACTIVE_BORDER, COLOR_BASE_BLUE, COLOR_BURN, Format},
    rpc::{EdgeClient, EdgeProgram, EdgeProofRequest, EdgeProofType},
    tui::{Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Abort prove / back" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "p", description: "Restart prove" },
];

const LOG_CAP: usize = 200;

/// Axiom Edge manager base URL.
const EDGE_URL_ENV: &str = "OPENVM_EDGE_URL";
/// Must match the manager's `EDGE_PROGRAMS` loadout and its
/// `artifacts/programs/range/1/program.vmexe`.
const EDGE_PROGRAM_NAME: &str = "range";
const EDGE_PROGRAM_VERSION: u32 = 1;
const EDGE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Sent as the manager's per-proof watchdog; its default (300s in dev) is too
/// short for a real block, whose app segments alone take ~30 min on the dev cluster.
const EDGE_PROOF_TIMEOUT: Duration = Duration::from_secs(90 * 60);
/// Lets the manager's watchdog report first so the TUI shows its reason.
const EDGE_POLL_GRACE: Duration = Duration::from_secs(60);
/// `OpenVM`'s default segment budget (`15 << 30` bytes), plus the guest memory
/// image each segment uploads, overflows the 24 GB L4s the Edge workers run on.
const EDGE_SEGMENT_MEMORY_BYTES: u64 = 8 << 30;

/// Pipeline steps, in run order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Witness,
    Prove,
    Wrap,
}

impl Step {
    const ALL: [Self; 3] = [Self::Witness, Self::Prove, Self::Wrap];

    const fn label(self) -> &'static str {
        match self {
            Self::Witness => "Witness",
            Self::Prove => "Prove",
            Self::Wrap => "Wrap",
        }
    }

    /// Shown until the pipeline reports something more specific.
    const fn hint(self) -> &'static str {
        match self {
            Self::Witness => "picking the safe head",
            Self::Prove => "STARK on Axiom Edge",
            Self::Wrap => "root → halo2",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum StepStatus {
    #[default]
    Pending,
    Running(Instant),
    Done(Duration),
    Failed(Duration),
}

#[derive(Clone, Debug)]
struct StepRow {
    status: StepStatus,
    detail: String,
}

#[derive(Clone, Debug)]
struct LiveRpcs {
    chain_name: String,
    l1_rpc: Url,
    l1_beacon_rpc: Url,
    l2_rpc: Url,
    l2_node_rpc: Url,
    edge: EdgeClient,
}

impl LiveRpcs {
    fn from_env(config: &MonitoringConfig) -> Result<Self, String> {
        Self::from_config(config, std::env::var(EDGE_URL_ENV).ok().as_deref())
    }

    fn from_config(config: &MonitoringConfig, edge_url: Option<&str>) -> Result<Self, String> {
        let hint = "`basectl -c zeronet` has both.";
        let l1_beacon_rpc = config
            .l1_beacon_rpc
            .clone()
            .ok_or_else(|| format!("OpenVM prove needs l1_beacon_rpc. {hint}"))?;
        let l2_node_rpc = config
            .consensus_node_rpc
            .clone()
            .ok_or_else(|| format!("OpenVM prove needs consensus_node_rpc (op-node). {hint}"))?;
        let edge_url = edge_url.ok_or_else(|| {
            format!("OpenVM prove needs {EDGE_URL_ENV} (Axiom Edge manager URL).")
        })?;
        let edge_url =
            Url::parse(edge_url).map_err(|error| format!("{EDGE_URL_ENV} is invalid: {error}"))?;
        let edge = EdgeClient::new(edge_url).map_err(|error| format!("Edge client: {error:#}"))?;
        Ok(Self {
            chain_name: config.name.clone(),
            l1_rpc: config.l1_rpc.clone(),
            l1_beacon_rpc,
            l2_rpc: config.rpc.clone(),
            l2_node_rpc,
            edge,
        })
    }
}

#[derive(Debug)]
enum LiveEvent {
    Log(String),
    /// Starts a step and finishes every step before it.
    Enter(Step),
    Detail(Step, String),
    /// Fraction of app segments proven, once the segment count is known.
    Progress(f64),
    Finished(Result<(), String>),
}

impl LiveEvent {
    /// Parses the `OPENVM_WITNESS start=.. end=.. bytes=..` line `openvm-dump`
    /// prints once the range is chosen (`bytes=0`) and again once it is written.
    fn parse_witness(line: &str) -> Option<Self> {
        let rest = line.strip_prefix("OPENVM_WITNESS ")?;
        let field = |key: &str| -> Option<u64> {
            rest.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.parse().ok())
        };
        let (start, end, bytes) = (field("start=")?, field("end=")?, field("bytes=")?);
        let size = if bytes == 0 { "fetching".to_string() } else { Format::bytes(bytes) };
        Some(Self::Detail(Step::Witness, format!("blocks {start} → {end} · {size}")))
    }
}

/// One in-flight or finished prove. Dropping it kills the pipeline and cancels
/// its Edge proof.
#[derive(Debug)]
struct LiveProve {
    chain_name: String,
    edge: EdgeClient,
    proof_uuid: String,
    rx: mpsc::Receiver<LiveEvent>,
    handle: Option<JoinHandle<()>>,
    started_at: Instant,
    finished_at: Option<Instant>,
    steps: [StepRow; 3],
    progress: Option<f64>,
    log: VecDeque<String>,
}

impl Drop for LiveProve {
    fn drop(&mut self) {
        if self.running() {
            self.stop();
        }
    }
}

impl LiveProve {
    fn start(rpcs: LiveRpcs) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let chain: String =
            rpcs.chain_name.chars().filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-').collect();
        let proof_uuid = format!("basectl-{chain}-{millis}");
        let mut live =
            Self::new(rpcs.chain_name.clone(), rpcs.edge.clone(), proof_uuid.clone(), rx);
        live.handle = Some(tokio::spawn(run_pipeline(rpcs, proof_uuid, tx)));
        live
    }

    fn new(
        chain_name: String,
        edge: EdgeClient,
        proof_uuid: String,
        rx: mpsc::Receiver<LiveEvent>,
    ) -> Self {
        let now = Instant::now();
        let mut steps = Step::ALL
            .map(|step| StepRow { status: StepStatus::Pending, detail: step.hint().into() });
        steps[Step::Witness as usize].status = StepStatus::Running(now);
        Self {
            chain_name,
            edge,
            proof_uuid,
            rx,
            handle: None,
            started_at: now,
            finished_at: None,
            steps,
            progress: None,
            log: VecDeque::new(),
        }
    }

    const fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    fn elapsed(&self) -> Duration {
        self.finished_at.unwrap_or_else(Instant::now).saturating_duration_since(self.started_at)
    }

    /// Kills the pipeline and cancels its Edge proof. The manager runs one proof
    /// at a time, so an abandoned proof would make the next start fail with 409.
    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if self.steps[Step::Prove as usize].status == StepStatus::Pending {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (edge, proof_uuid) = (self.edge.clone(), self.proof_uuid.clone());
            runtime.spawn(async move {
                let _ = edge.cancel_proof(&proof_uuid).await;
            });
        }
    }

    fn poll(&mut self) -> Option<Toast> {
        let mut toast = None;
        while let Ok(event) = self.rx.try_recv() {
            toast = self.apply(event).or(toast);
        }
        toast
    }

    fn apply(&mut self, event: LiveEvent) -> Option<Toast> {
        match event {
            LiveEvent::Log(line) => {
                if self.log.len() == LOG_CAP {
                    self.log.pop_front();
                }
                self.log.push_back(line);
            }
            LiveEvent::Enter(step) => self.enter(step),
            LiveEvent::Detail(step, detail) => self.steps[step as usize].detail = detail,
            LiveEvent::Progress(ratio) => self.progress = Some(ratio),
            LiveEvent::Finished(result) => return Some(self.finish(result)),
        }
        None
    }

    fn enter(&mut self, step: Step) {
        let now = Instant::now();
        for row in &mut self.steps[..step as usize] {
            if let StepStatus::Running(since) = row.status {
                row.status = StepStatus::Done(now - since);
            }
        }
        self.steps[step as usize].status = StepStatus::Running(now);
    }

    fn finish(&mut self, result: Result<(), String>) -> Toast {
        if result.is_err() {
            self.stop();
        }
        let now = Instant::now();
        self.finished_at = Some(now);
        self.handle = None;
        for row in &mut self.steps {
            let StepStatus::Running(since) = row.status else { continue };
            row.status = match &result {
                Ok(()) => StepStatus::Done(now - since),
                Err(error) => {
                    row.detail.clone_from(error);
                    StepStatus::Failed(now - since)
                }
            };
        }
        match result {
            Ok(()) => Toast::info(format!(
                "Edge EVM proof completed in {}",
                Format::duration(self.elapsed())
            )),
            Err(error) => Toast::warning(error),
        }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let keys =
            if self.running() { " p restart · Esc abort " } else { " p prove again · Esc back " };
        let block = Block::default()
            .title(format!(" OpenVM · {} ", self.chain_name))
            .title(Line::from(format!(" {} ", Format::duration(self.elapsed()))).right_aligned())
            .title_bottom(keys)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [steps, log] =
            Layout::vertical([Constraint::Length(Step::ALL.len() as u16 + 1), Constraint::Min(0)])
                .areas(inner);
        let rows = Layout::vertical([Constraint::Length(1); Step::ALL.len()]).split(steps);
        for (step, row) in Step::ALL.into_iter().zip(rows.iter()) {
            self.render_step(frame, *row, step);
        }
        self.render_log(frame, log);
    }

    fn render_step(&self, frame: &mut Frame<'_>, area: Rect, step: Step) {
        let row = &self.steps[step as usize];
        let (glyph, color, took) = match row.status {
            StepStatus::Pending => ('○', Color::DarkGray, None),
            StepStatus::Running(since) => ('●', COLOR_ACTIVE_BORDER, Some(since.elapsed())),
            StepStatus::Done(took) => ('✓', COLOR_BURN, Some(took)),
            StepStatus::Failed(took) => ('✗', Color::Red, Some(took)),
        };
        let [head, mut body, clock] =
            Layout::horizontal([Constraint::Length(12), Constraint::Min(0), Constraint::Length(9)])
                .areas(area);
        frame.render_widget(
            Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::raw(step.label()),
            ]),
            head,
        );

        let detail_color =
            if matches!(row.status, StepStatus::Failed(_)) { Color::Red } else { Color::Gray };
        let detail = Line::from(row.detail.as_str()).fg(detail_color);
        let progress = self.progress.filter(|_| matches!(row.status, StepStatus::Running(_)));
        if let Some(ratio) = progress.filter(|_| step == Step::Prove) {
            let [bar, text] =
                Layout::horizontal([Constraint::Min(8), Constraint::Length(detail.width() as u16)])
                    .spacing(1)
                    .areas(body);
            let gauge = LineGauge::default()
                .ratio(ratio.clamp(0.0, 1.0))
                .label("")
                .filled_symbol("━")
                .filled_style(Style::default().fg(COLOR_ACTIVE_BORDER))
                .unfilled_style(Style::default().fg(Color::DarkGray));
            frame.render_widget(gauge, bar);
            body = text;
        }
        frame.render_widget(detail, body);

        if let Some(took) = took {
            frame.render_widget(
                Line::from(format!("{} ", Format::duration(took)))
                    .right_aligned()
                    .fg(Color::DarkGray),
                clock,
            );
        }
    }

    fn render_log(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default().padding(Padding::horizontal(1));
        let height = block.inner(area).height as usize;
        let lines: Vec<Line<'_>> = self
            .log
            .iter()
            .skip(self.log.len().saturating_sub(height))
            .map(|line| Line::from(line.as_str()).fg(Color::DarkGray))
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// Proves one live L2 block with the `OpenVM` range guest.
///
/// Shells out to `openvm-dump` for the witness, then proves it on the Axiom
/// Edge manager at `OPENVM_EDGE_URL`.
#[derive(Debug, Default)]
pub struct OpenVmView {
    live: Option<LiveProve>,
    started: bool,
}

impl OpenVmView {
    /// Creates the view. The first tick starts a prove on the active network.
    pub fn new() -> Self {
        Self::default()
    }

    fn restart(&mut self, resources: &mut Resources) {
        self.started = true;
        match LiveRpcs::from_env(&resources.config) {
            Ok(rpcs) => {
                resources.toasts.push(Toast::info(format!(
                    "Proving one {} block with OpenVM",
                    rpcs.chain_name
                )));
                self.live = Some(LiveProve::start(rpcs));
            }
            Err(message) => resources.toasts.push(Toast::warning(message)),
        }
    }
}

impl View for OpenVmView {
    fn keybindings(&self) -> &'static [Keybinding] {
        KEYBINDINGS
    }

    fn consumes_esc(&self) -> bool {
        self.live.as_ref().is_some_and(LiveProve::running)
    }

    fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) -> Action {
        match key.code {
            KeyCode::Esc => {
                if let Some(live) = self.live.as_mut().filter(|live| live.running()) {
                    resources.toasts.push(live.finish(Err("aborted".to_string())));
                }
            }
            KeyCode::Char('p') => self.restart(resources),
            _ => {}
        }
        Action::None
    }

    fn tick(&mut self, resources: &mut Resources) -> Action {
        if !self.started {
            self.restart(resources);
        }
        if let Some(toast) = self.live.as_mut().and_then(LiveProve::poll) {
            resources.toasts.push(toast);
        }
        Action::None
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, resources: &Resources) {
        if let Some(live) = self.live.as_ref() {
            live.render(frame, area);
            return;
        }
        let hint =
            LiveRpcs::from_env(&resources.config).err().unwrap_or_else(|| "starting…".to_string());
        let block = Block::default()
            .title(" OpenVM ")
            .title_bottom(" p retry ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE))
            .padding(Padding::horizontal(1));
        frame.render_widget(Paragraph::new(hint).block(block).fg(Color::Gray), area);
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

async fn run_pipeline(rpcs: LiveRpcs, proof_uuid: String, tx: mpsc::Sender<LiveEvent>) {
    let result = prove(&rpcs, &proof_uuid, &tx).await;
    let _ = tx.send(LiveEvent::Finished(result)).await;
}

async fn prove(
    rpcs: &LiveRpcs,
    proof_uuid: &str,
    tx: &mpsc::Sender<LiveEvent>,
) -> Result<(), String> {
    let elf = workspace_root().join("crates/proof/zk/programs/openvm/elf");
    // RPCs go through env so cargo's `Running …` line keeps them out of the log.
    let mut dump = Command::new("cargo");
    dump.args(["run", "-p", "base-openvm-dump", "--"])
        .env("OPENVM_L1_RPC", rpcs.l1_rpc.as_str())
        .env("OPENVM_L1_BEACON_RPC", rpcs.l1_beacon_rpc.as_str())
        .env("OPENVM_L2_RPC", rpcs.l2_rpc.as_str())
        .env("OPENVM_L2_NODE_RPC", rpcs.l2_node_rpc.as_str())
        .arg("--out-dir")
        .arg(&elf)
        .current_dir(workspace_root())
        .env_remove("RUST_LOG");
    run_logged(dump, tx).await.map_err(|error| format!("witness dump {error}"))?;

    let stdin_path = elf.join("input.bin");
    let stdin = tokio::fs::read(&stdin_path)
        .await
        .map_err(|error| format!("failed to read {}: {error}", stdin_path.display()))?;
    let _ = tx.send(LiveEvent::Enter(Step::Prove)).await;

    let edge_error = |error: anyhow::Error| format!("Edge: {error:#}");
    log(tx, format!("edge: uploading {} input as {proof_uuid}", Format::bytes(stdin.len() as u64)))
        .await;
    rpcs.edge.upload_input(proof_uuid, stdin).await.map_err(edge_error)?;
    let request = EdgeProofRequest {
        proof_uuid: proof_uuid.to_string(),
        program: EdgeProgram { name: EDGE_PROGRAM_NAME.to_string(), version: EDGE_PROGRAM_VERSION },
        proof_type: EdgeProofType::Evm,
        timeout_secs: EDGE_PROOF_TIMEOUT.as_secs(),
        segment_memory: Some(EDGE_SEGMENT_MEMORY_BYTES),
    };
    let started = rpcs.edge.start_proof(&request).await.map_err(edge_error)?;
    log(tx, format!("edge: proving on {} workers", started.num_workers)).await;

    let deadline = Instant::now() + EDGE_PROOF_TIMEOUT + EDGE_POLL_GRACE;
    let mut last_summary = String::new();
    let mut wrapping = false;
    loop {
        if Instant::now() > deadline {
            return Err(format!("Edge proof exceeded {}", Format::duration(EDGE_PROOF_TIMEOUT)));
        }
        tokio::time::sleep(EDGE_POLL_INTERVAL).await;
        let state = rpcs.edge.proof_state(proof_uuid).await.map_err(edge_error)?;
        let segments = state.num_segments.filter(|&segments| segments > 0);
        let detail = format!(
            "app {}/{} · leaf {} · internal {}",
            state.app_proofs_count,
            segments.map_or_else(|| "?".to_string(), |segments| segments.to_string()),
            state.leaf_proofs_count,
            state.internal_proofs_count,
        );
        let summary = format!("edge: {} · {detail}", state.status.label());
        if summary != last_summary {
            log(tx, summary.clone()).await;
            last_summary = summary;
            if let Some(segments) = segments {
                let ratio = state.app_proofs_count as f64 / segments as f64;
                let _ = tx.send(LiveEvent::Progress(ratio)).await;
            }
            let _ = tx.send(LiveEvent::Detail(Step::Prove, detail)).await;
        }
        if state.stark_done() && !wrapping {
            wrapping = true;
            let _ = tx.send(LiveEvent::Enter(Step::Wrap)).await;
        }
        if state.is_completed() {
            if let (Some(root), Some(halo2)) =
                (state.total_root_prove_ms, state.total_halo2_prove_ms)
            {
                let detail = format!(
                    "root {} · halo2 {}",
                    Format::duration(Duration::from_millis(root)),
                    Format::duration(Duration::from_millis(halo2))
                );
                let _ = tx.send(LiveEvent::Detail(Step::Wrap, detail)).await;
            }
            return Ok(());
        }
        if state.is_terminal() {
            return Err(format!("Edge proof {}", state.status));
        }
    }
}

async fn log(tx: &mpsc::Sender<LiveEvent>, line: String) {
    let _ = tx.send(LiveEvent::Log(line)).await;
}

async fn run_logged(mut command: Command, tx: &mpsc::Sender<LiveEvent>) -> Result<(), String> {
    command
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1")
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("failed to spawn: {error}"))?;
    let out_task = child.stdout.take().map(|out| tokio::spawn(pump_lines(out, tx.clone())));
    let err_task = child.stderr.take().map(|err| tokio::spawn(pump_lines(err, tx.clone())));

    let status = child.wait().await.map_err(|error| format!("failed: {error}"))?;
    for task in [out_task, err_task].into_iter().flatten() {
        let _ = task.await;
    }
    if status.success() { Ok(()) } else { Err(format!("exited {status}")) }
}

async fn pump_lines<R: AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<LiveEvent>) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = strip_ansi(line.trim());
        if line.is_empty() {
            continue;
        }
        if let Some(event) = LiveEvent::parse_witness(&line) {
            let _ = tx.send(event).await;
        }
        let _ = tx.send(LiveEvent::Log(line)).await;
    }
}

/// Drops CSI escape sequences so child output cannot restyle the terminal.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            chars.by_ref().take_while(|next| !next.is_ascii_alphabetic()).for_each(drop);
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_prove() -> LiveProve {
        let (_tx, rx) = mpsc::channel(1);
        let edge = EdgeClient::new(Url::parse("http://127.0.0.1:1").unwrap()).unwrap();
        LiveProve::new("zeronet".to_string(), edge, "basectl-p".to_string(), rx)
    }

    fn screen(live: &LiveProve) -> String {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| live.render(frame, frame.area())).unwrap();
        terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn rpcs_require_beacon_op_node_and_edge_url() {
        let mut config = MonitoringConfig::mainnet();
        config.l1_beacon_rpc = None;
        assert!(LiveRpcs::from_config(&config, Some("http://127.0.0.1:3000")).is_err());

        config.l1_beacon_rpc = Some(Url::parse("http://127.0.0.1:5052").unwrap());
        config.consensus_node_rpc = Some(Url::parse("http://127.0.0.1:7545").unwrap());
        let missing = LiveRpcs::from_config(&config, None).unwrap_err();
        assert!(missing.contains(EDGE_URL_ENV));
        let rpcs = LiveRpcs::from_config(&config, Some("http://127.0.0.1:3000")).unwrap();
        assert_eq!(rpcs.chain_name, "mainnet");
    }

    #[test]
    fn parses_openvm_dump_witness_line() {
        match LiveEvent::parse_witness("OPENVM_WITNESS start=10 end=11 bytes=0") {
            Some(LiveEvent::Detail(Step::Witness, detail)) => {
                assert_eq!(detail, "blocks 10 → 11 · fetching");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(LiveEvent::parse_witness("OPENVM_WITNESS start=10").is_none());
        assert!(LiveEvent::parse_witness("INFO compiling range").is_none());
    }

    #[test]
    fn failure_marks_the_running_step_with_its_reason() {
        let mut live = idle_prove();
        live.apply(LiveEvent::Enter(Step::Prove));
        assert!(matches!(live.steps[Step::Witness as usize].status, StepStatus::Done(_)));
        assert!(matches!(live.steps[Step::Prove as usize].status, StepStatus::Running(_)));

        assert!(live.apply(LiveEvent::Finished(Err("Edge proof failed: OOM".into()))).is_some());
        let prove = &live.steps[Step::Prove as usize];
        assert!(matches!(prove.status, StepStatus::Failed(_)));
        assert_eq!(prove.detail, "Edge proof failed: OOM");
        assert_eq!(live.steps[Step::Wrap as usize].status, StepStatus::Pending);
        assert!(!live.running());
    }

    #[test]
    fn success_completes_every_started_step() {
        let mut live = idle_prove();
        live.apply(LiveEvent::Enter(Step::Prove));
        live.apply(LiveEvent::Enter(Step::Wrap));
        live.apply(LiveEvent::Finished(Ok(())));
        assert!(live.steps.iter().all(|row| matches!(row.status, StepStatus::Done(_))));
        assert!(!live.running());
    }

    #[test]
    fn renders_progress_then_failure_reason() {
        let mut live = idle_prove();
        live.apply(LiveEvent::parse_witness("OPENVM_WITNESS start=5 end=6 bytes=43000").unwrap());
        live.apply(LiveEvent::Enter(Step::Prove));
        live.apply(LiveEvent::Progress(0.5));
        live.apply(LiveEvent::Detail(Step::Prove, "app 7/14 · leaf 0 · internal 0".into()));
        let running = screen(&live);
        assert!(running.contains("OpenVM · zeronet"));
        assert!(running.contains("✓ Witness"));
        assert!(running.contains("blocks 5 → 6"));
        assert!(running.contains("● Prove"));
        assert!(running.contains("app 7/14"));

        live.apply(LiveEvent::Finished(Err("Edge proof failed: GPU OOM".into())));
        let failed = screen(&live);
        assert!(failed.contains("✗ Prove"));
        assert!(failed.contains("Edge proof failed: GPU OOM"));
        assert!(failed.contains("○ Wrap"));
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let raw =
            "\u{1b}[2m2026-09-25T22:56:57Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m Dropping old batch";
        assert_eq!(strip_ansi(raw), "2026-09-25T22:56:57Z  WARN Dropping old batch");
    }
}
