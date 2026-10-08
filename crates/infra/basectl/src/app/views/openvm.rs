//! Live `OpenVM` range prove of one L2 block: witness dump, guest execute, app proof.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use alloy_primitives::hex;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph, Sparkline},
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
    tui::{Keybinding, Toast},
};

const KEYBINDINGS: &[Keybinding] = &[
    Keybinding { key: "Esc", description: "Abort prove / back" },
    Keybinding { key: "?", description: "Toggle help" },
    Keybinding { key: "p", description: "Restart prove" },
];

const LOG_CAP: usize = 200;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Derivation logs one WARN per historical batch; a live block emits millions.
const DUMP_RUST_LOG: &str = "info,batch_validator=off,batch_queue=off";
const BAR_BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Witness,
    Execute,
    Prove,
    Done,
}

impl Stage {
    const fn label(self) -> &'static str {
        match self {
            Self::Witness => "dumping witness",
            Self::Execute => "executing guest",
            Self::Prove => "proving",
            Self::Done => "done",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardState {
    Queued,
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
struct LiveRpcs {
    chain_name: String,
    l1_rpc: Url,
    l1_beacon_rpc: Url,
    l2_rpc: Url,
    l2_node_rpc: Url,
}

impl LiveRpcs {
    fn from_config(config: &MonitoringConfig) -> Result<Self, String> {
        let hint = "`basectl -c zeronet` has both.";
        let l1_beacon_rpc = config
            .l1_beacon_rpc
            .clone()
            .ok_or_else(|| format!("OpenVM prove needs l1_beacon_rpc. {hint}"))?;
        let l2_node_rpc = config
            .consensus_node_rpc
            .clone()
            .ok_or_else(|| format!("OpenVM prove needs consensus_node_rpc (op-node). {hint}"))?;
        Ok(Self {
            chain_name: config.name.clone(),
            l1_rpc: config.l1_rpc.clone(),
            l1_beacon_rpc,
            l2_rpc: config.rpc.clone(),
            l2_node_rpc,
        })
    }
}

#[derive(Debug)]
enum LiveEvent {
    Log(String),
    Stage(Stage),
    Witness { start: u64, end: u64, bytes: u64 },
    Digest(String),
    ExeCommit(String),
    Failed(String),
    Done { proof_bytes: Option<u64> },
}

impl LiveEvent {
    /// Recognizes the lines `openvm-dump` and `cargo-openvm` print to stdout.
    fn parse(line: &str) -> Option<Self> {
        if let Some(rest) = line.strip_prefix("OPENVM_WITNESS ") {
            let field = |key: &str| {
                rest.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.parse().ok())
            };
            return Some(Self::Witness {
                start: field("start=")?,
                end: field("end=")?,
                bytes: field("bytes=")?,
            });
        }
        if let Some(commit) = line.strip_prefix("exe commit:") {
            return Some(Self::ExeCommit(commit.trim().to_string()));
        }
        // `cargo openvm run` prints the revealed bytes as `Vec<u8>` Debug.
        let bytes = line.strip_prefix("Execution output:")?.trim();
        let bytes = bytes.strip_prefix('[')?.strip_suffix(']')?;
        let bytes: Vec<u8> =
            bytes.split(',').map(|byte| byte.trim().parse()).collect::<Result<_, _>>().ok()?;
        Some(Self::Digest(hex::encode_prefixed(bytes)))
    }
}

/// One in-flight or finished prove. Dropping it kills the pipeline.
#[derive(Debug)]
struct LiveProve {
    chain_name: String,
    rx: mpsc::Receiver<LiveEvent>,
    handle: Option<JoinHandle<()>>,
    stage: Stage,
    started_at: Instant,
    stage_started_at: Instant,
    stage_times: [Option<Duration>; 3],
    finished_at: Option<Instant>,
    error: Option<String>,
    log: VecDeque<String>,
    range: Option<(u64, u64)>,
    witness_bytes: Option<u64>,
    digest: Option<String>,
    exe_commit: Option<String>,
    proof_bytes: Option<u64>,
}

impl Drop for LiveProve {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl LiveProve {
    fn start(rpcs: LiveRpcs) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let chain_name = rpcs.chain_name.clone();
        Self::new(chain_name, rx, Some(tokio::spawn(run_pipeline(rpcs, tx))))
    }

    fn new(
        chain_name: String,
        rx: mpsc::Receiver<LiveEvent>,
        handle: Option<JoinHandle<()>>,
    ) -> Self {
        let now = Instant::now();
        Self {
            chain_name,
            rx,
            handle,
            stage: Stage::Witness,
            started_at: now,
            stage_started_at: now,
            stage_times: [None; 3],
            finished_at: None,
            error: None,
            log: VecDeque::new(),
            range: None,
            witness_bytes: None,
            digest: None,
            exe_commit: None,
            proof_bytes: None,
        }
    }

    const fn running(&self) -> bool {
        self.finished_at.is_none()
    }

    fn elapsed(&self) -> Duration {
        self.finished_at.unwrap_or_else(Instant::now).saturating_duration_since(self.started_at)
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message);
        self.finished_at = Some(Instant::now());
        if let Some(handle) = self.handle.take() {
            handle.abort();
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
            LiveEvent::Stage(stage) => self.enter(stage),
            LiveEvent::Witness { start, end, bytes } => {
                self.range = Some((start, end));
                self.witness_bytes = (bytes > 0).then_some(bytes);
            }
            LiveEvent::Digest(digest) => self.digest = Some(digest),
            LiveEvent::ExeCommit(commit) => self.exe_commit = Some(commit),
            LiveEvent::Failed(message) => {
                self.fail(message.clone());
                return Some(Toast::warning(message));
            }
            LiveEvent::Done { proof_bytes } => {
                self.enter(Stage::Done);
                self.proof_bytes = proof_bytes;
                self.finished_at = Some(Instant::now());
                self.handle = None;
                return Some(Toast::info("OpenVM app proof completed".to_string()));
            }
        }
        None
    }

    fn enter(&mut self, stage: Stage) {
        if stage == self.stage {
            return;
        }
        if let Some(slot) = self.stage_times.get_mut(self.stage as usize) {
            *slot = Some(self.stage_started_at.elapsed());
        }
        self.stage = stage;
        self.stage_started_at = Instant::now();
    }

    fn card_state(&self, stage: Stage) -> CardState {
        if self.stage > stage || self.stage == Stage::Done {
            CardState::Ok
        } else if self.stage < stage {
            CardState::Queued
        } else if self.error.is_some() {
            CardState::Failed
        } else {
            CardState::Running
        }
    }

    fn timing(&self, stage: Stage) -> String {
        match self.card_state(stage) {
            CardState::Ok => self
                .stage_times
                .get(stage as usize)
                .copied()
                .flatten()
                .map_or_else(String::new, |took| format!("took {}", format_duration(took))),
            CardState::Running => {
                format!("elapsed {}", format_duration(self.stage_started_at.elapsed()))
            }
            CardState::Failed => "failed".to_string(),
            CardState::Queued => "queued".to_string(),
        }
    }
}

/// Proves one live L2 block with the local `OpenVM` range guest.
///
/// Shells out to `openvm-dump` for the witness, then `cargo openvm run` and
/// `cargo openvm prove app`, and renders their progress.
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
        match LiveRpcs::from_config(&resources.config) {
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
                    live.fail("aborted".to_string());
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
        let Some(live) = self.live.as_ref() else {
            let hint = LiveRpcs::from_config(&resources.config)
                .err()
                .unwrap_or_else(|| "starting…".to_string());
            let block = Block::default()
                .title(" OpenVM range ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLOR_BASE_BLUE));
            frame.render_widget(
                Paragraph::new(format!("{hint}\n\np retry")).block(block).fg(Color::Gray),
                area,
            );
            return;
        };

        let range = live.range.map_or_else(String::new, |(start, end)| format!(" {start}→{end}"));
        let status = match (&live.error, live.stage) {
            (Some(_), _) => "failed",
            (None, Stage::Done) => "done",
            (None, _) => "live",
        };
        let block = Block::default()
            .title(format!(" OpenVM range · {}{range} · {status} ", live.chain_name))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(inner);
        render_pipeline(frame, chunks[0], live);
        render_details(frame, chunks[1], live);
        render_log(frame, chunks[2], live);
        render_footer(frame, chunks[3], live);
    }
}

fn openvm_dir() -> PathBuf {
    workspace_root().join("crates/proof/zk/programs/openvm")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

async fn run_pipeline(rpcs: LiveRpcs, tx: mpsc::Sender<LiveEvent>) {
    if let Err(error) = run_pipeline_steps(rpcs, &tx).await {
        let _ = tx.send(LiveEvent::Failed(error)).await;
    }
}

async fn run_pipeline_steps(rpcs: LiveRpcs, tx: &mpsc::Sender<LiveEvent>) -> Result<(), String> {
    let openvm = openvm_dir();
    let guest = openvm.join("range/ethereum");
    let elf = openvm.join("elf");
    let exe = elf.join("range.vmexe");
    let input = elf.join("input.json");
    let pk = elf.join("app.pk");
    let proof = elf.join("range.app.proof");
    if !exe.is_file() {
        return Err("range.vmexe missing. Run `just openvm build-elf` first.".to_string());
    }

    let mut dump = Command::new("cargo");
    dump.args(["run", "-p", "base-openvm-dump", "--", "--l1-rpc", rpcs.l1_rpc.as_str()])
        .args(["--l1-beacon-rpc", rpcs.l1_beacon_rpc.as_str()])
        .args(["--l2-rpc", rpcs.l2_rpc.as_str()])
        .args(["--l2-node-rpc", rpcs.l2_node_rpc.as_str()])
        .arg("--out-dir")
        .arg(&elf)
        .current_dir(workspace_root())
        .env("RUST_LOG", DUMP_RUST_LOG);
    run_logged(dump, tx, Stage::Witness).await?;

    let _ = tx.send(LiveEvent::Stage(Stage::Execute)).await;
    let mut run = cargo_openvm(&guest, &["run"]);
    run.arg("--exe").arg(&exe).arg("--input").arg(&input);
    run_logged(run, tx, Stage::Execute).await?;

    let _ = tx.send(LiveEvent::Stage(Stage::Prove)).await;
    // App keys depend only on openvm.toml and take well under a second, so
    // regenerate them rather than risk a key from an older VM config.
    let mut keygen = cargo_openvm(&guest, &["keygen", "--app-only"]);
    keygen.arg("--output-dir").arg(&elf);
    run_logged(keygen, tx, Stage::Prove).await?;
    let mut prove = cargo_openvm(&guest, &["prove", "app"]);
    prove
        .arg("--exe")
        .arg(&exe)
        .arg("--input")
        .arg(&input)
        .arg("--app-pk")
        .arg(&pk)
        .arg("--proof")
        .arg(&proof);
    run_logged(prove, tx, Stage::Prove).await?;

    let proof_bytes = std::fs::metadata(&proof).ok().map(|meta| meta.len());
    let _ = tx.send(LiveEvent::Done { proof_bytes }).await;
    Ok(())
}

fn cargo_openvm(guest: &Path, subcommand: &[&str]) -> Command {
    let mut command = Command::new("cargo");
    command.arg("openvm").args(subcommand).args(["--config", "openvm.toml"]).current_dir(guest);
    command
}

async fn run_logged(
    mut command: Command,
    tx: &mpsc::Sender<LiveEvent>,
    stage: Stage,
) -> Result<(), String> {
    command
        .env("CARGO_TERM_COLOR", "never")
        .env("NO_COLOR", "1")
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("failed to spawn: {error}"))?;
    let out_task = child.stdout.take().map(|out| tokio::spawn(pump_lines(out, tx.clone())));
    let err_task = child.stderr.take().map(|err| tokio::spawn(pump_lines(err, tx.clone())));

    let started = Instant::now();
    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.tick().await;
    let wait = child.wait();
    tokio::pin!(wait);
    let status = loop {
        tokio::select! {
            status = &mut wait => break status,
            _ = beat.tick() => {
                let line = format!("{}  {}", stage.label(), format_duration(started.elapsed()));
                let _ = tx.send(LiveEvent::Log(line)).await;
            }
        }
    };
    let status = status.map_err(|error| format!("command failed: {error}"))?;
    for task in [out_task, err_task].into_iter().flatten() {
        let _ = task.await;
    }
    if status.success() { Ok(()) } else { Err(format!("{} exited {status}", stage.label())) }
}

async fn pump_lines<R: AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<LiveEvent>) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = strip_ansi(line.trim());
        if line.is_empty() {
            continue;
        }
        if let Some(event) = LiveEvent::parse(&line) {
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

fn render_pipeline(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3); 3])
        .split(area);

    let witness = live.witness_bytes.map_or_else(
        || "dumping rkyv…".to_string(),
        |bytes| format!("{} rkyv", Format::bytes(bytes)),
    );
    let blocks = live.range.map_or_else(
        || "picking safe head…".to_string(),
        |(start, end)| format!("blocks {start} → {end}"),
    );
    let digest = live.digest.as_deref().map_or_else(|| "boot digest …".to_string(), short_hex);
    let proof = live.proof_bytes.map_or_else(
        || "cargo-openvm reports no %".to_string(),
        |bytes| format!("{} app proof", Format::bytes(bytes)),
    );

    render_stage_card(frame, cols[0], " Witness ", live, Stage::Witness, [witness, blocks]);
    render_stage_card(
        frame,
        cols[1],
        " Execute ",
        live,
        Stage::Execute,
        ["cargo openvm run".to_string(), digest],
    );
    render_stage_card(
        frame,
        cols[2],
        " App proof ",
        live,
        Stage::Prove,
        ["cargo openvm prove app".to_string(), proof],
    );
}

fn render_stage_card(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    live: &LiveProve,
    stage: Stage,
    lines: [String; 2],
) {
    let state = live.card_state(stage);
    // ponytail: cargo-openvm prints no progress, so a running card pulses off
    // the wall clock instead of filling. Swap for real telemetry if an OpenVM
    // host backend ever streams it.
    let pulse = 0.2 + 0.35 * (live.stage_started_at.elapsed().as_secs_f64() * 1.8).sin().abs();
    let (ratio, color, border) = match state {
        CardState::Ok => (1.0, COLOR_BURN, COLOR_BURN),
        CardState::Running => (pulse, COLOR_ACTIVE_BORDER, COLOR_ACTIVE_BORDER),
        CardState::Failed => (0.0, Color::Red, Color::Red),
        CardState::Queued => (0.0, Color::DarkGray, Color::DarkGray),
    };
    let block = Block::default()
        .title(title)
        .title_bottom(format!(" {} ", live.timing(stage)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut text = vec![fill_bar(ratio, inner.width.saturating_sub(1) as usize, color)];
    text.extend(lines.map(|line| Line::from(line).fg(Color::Gray)));
    frame.render_widget(Paragraph::new(text), inner);
}

fn render_details(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    render_values(frame, cols[0], live);
    render_wave(frame, cols[1], live);
}

fn render_values(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let block = Block::default()
        .title(" Public values ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let value = |value: Option<String>, pending: &str, color: Color| {
        value.map_or_else(
            || Span::styled(pending.to_string(), Style::default().fg(Color::DarkGray)),
            |value| Span::styled(value, Style::default().fg(color)),
        )
    };
    let text = vec![
        kv_line(
            "boot digest",
            value(live.digest.clone(), "keccak256(abi.encode(boot)) …", Color::Cyan),
        ),
        kv_line("exe commit", value(live.exe_commit.clone(), "…", Color::Gray)),
        kv_line(
            "app proof",
            value(
                live.proof_bytes
                    .map(|bytes| format!("{}  elf/range.app.proof", Format::bytes(bytes))),
                "…",
                COLOR_BURN,
            ),
        ),
        kv_line(
            "witness",
            value(
                live.witness_bytes
                    .map(|bytes| format!("{}  0x01-framed rkyv", Format::bytes(bytes))),
                "…",
                Color::Gray,
            ),
        ),
        kv_line("guest", value(Some("elf/range.vmexe".to_string()), "", Color::Gray)),
    ];
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_wave(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let (color, fill) = match live.card_state(live.stage) {
        CardState::Running => (COLOR_ACTIVE_BORDER, 1.0),
        CardState::Failed => (Color::DarkGray, 0.4),
        CardState::Ok | CardState::Queued => (COLOR_BURN, 1.0),
    };
    let block = Block::default()
        .title(format!(" {}  {} ", live.stage.label(), format_duration(live.elapsed())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // ponytail: a decorative waveform scrolled off the wall clock so the panel
    // visibly moves during the silent prove; it freezes when the prove ends.
    let width = inner.width as usize;
    let phase = live.elapsed().as_secs_f64() * 2.4;
    let len = (fill * width as f64) as usize;
    let data: Vec<u64> = (0..len)
        .map(|i| {
            let x = i as f64 * 0.37 + phase;
            (24.0 + 14.0 * x.sin() + 7.0 * (x * 0.29).cos() + 3.0 * (i as f64 * 0.91 + phase).sin())
                as u64
        })
        .collect();
    frame.render_widget(
        Sparkline::default().data(&data).max(50).style(Style::default().fg(color)),
        inner,
    );
}

fn render_log(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let block = Block::default()
        .title(" Prove log ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BASE_BLUE));
    let height = block.inner(area).height as usize;
    let lines: Vec<Line<'_>> = live
        .log
        .iter()
        .skip(live.log.len().saturating_sub(height))
        .map(|line| Line::from(line.as_str()).fg(Color::Gray))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, live: &LiveProve) {
    let hint = match (&live.error, live.running()) {
        (Some(error), _) => format!("  p retry   Esc back    {error}"),
        (None, true) => {
            format!("  p restart   Esc abort    elapsed {}", format_duration(live.elapsed()))
        }
        (None, false) => {
            format!("  p prove again   Esc back    took {}", format_duration(live.elapsed()))
        }
    };
    frame.render_widget(Paragraph::new(hint).fg(Color::DarkGray), area);
}

fn kv_line<'a>(key: &'a str, value: Span<'a>) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:<12}"), Style::default().fg(Color::DarkGray)),
        value,
    ])
}

fn fill_bar(ratio: f64, width: usize, color: Color) -> Line<'static> {
    let width = width.max(1);
    let filled = (ratio.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let spans: Vec<Span<'static>> = (0..width)
        .map(|i| match filled.saturating_sub(i * 8).min(8) {
            0 => Span::styled("░", Style::default().fg(Color::DarkGray)),
            8 => Span::styled("█", Style::default().fg(color)),
            sub => Span::styled(BAR_BLOCKS[sub - 1].to_string(), Style::default().fg(color)),
        })
        .collect();
    Line::from(spans)
}

fn short_hex(hex: &str) -> String {
    match (hex.get(..6), hex.get(hex.len().saturating_sub(4)..)) {
        (Some(head), Some(tail)) if hex.len() > 12 => format!("{head}…{tail}"),
        _ => hex.to_string(),
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_prove() -> LiveProve {
        let (_tx, rx) = mpsc::channel(1);
        LiveProve::new("zeronet".to_string(), rx, None)
    }

    #[test]
    fn rpcs_require_beacon_and_op_node() {
        let mut config = MonitoringConfig::mainnet();
        config.l1_beacon_rpc = None;
        assert!(LiveRpcs::from_config(&config).is_err());

        config.l1_beacon_rpc = Some(Url::parse("http://127.0.0.1:5052").unwrap());
        config.consensus_node_rpc = Some(Url::parse("http://127.0.0.1:7545").unwrap());
        assert_eq!(LiveRpcs::from_config(&config).unwrap().chain_name, "mainnet");
    }

    #[test]
    fn parses_dump_and_cargo_openvm_lines() {
        match LiveEvent::parse("OPENVM_WITNESS start=10 end=11 bytes=43000") {
            Some(LiveEvent::Witness { start: 10, end: 11, bytes: 43000 }) => {}
            other => panic!("unexpected {other:?}"),
        }
        match LiveEvent::parse("exe commit: 0x0089") {
            Some(LiveEvent::ExeCommit(commit)) => assert_eq!(commit, "0x0089"),
            other => panic!("unexpected {other:?}"),
        }
        match LiveEvent::parse("Execution output: [53, 41, 91, 255]") {
            Some(LiveEvent::Digest(digest)) => assert_eq!(digest, "0x35295bff"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(LiveEvent::parse("Execution output: [not, bytes]").is_none());
        assert!(LiveEvent::parse("INFO compiling range").is_none());
    }

    #[test]
    fn stages_advance_and_failure_marks_active_card() {
        let mut live = idle_prove();
        assert_eq!(live.card_state(Stage::Witness), CardState::Running);
        assert_eq!(live.card_state(Stage::Prove), CardState::Queued);

        live.apply(LiveEvent::Stage(Stage::Execute));
        live.apply(LiveEvent::Stage(Stage::Prove));
        assert_eq!(live.card_state(Stage::Witness), CardState::Ok);
        assert!(live.timing(Stage::Execute).starts_with("took"));
        assert!(live.running());

        assert!(live.apply(LiveEvent::Failed("prove exited 1".to_string())).is_some());
        assert_eq!(live.card_state(Stage::Prove), CardState::Failed);
        assert_eq!(live.card_state(Stage::Execute), CardState::Ok);
        assert!(!live.running());
    }

    #[test]
    fn done_completes_every_card() {
        let mut live = idle_prove();
        live.apply(LiveEvent::Done { proof_bytes: Some(14) });
        for stage in [Stage::Witness, Stage::Execute, Stage::Prove] {
            assert_eq!(live.card_state(stage), CardState::Ok);
        }
        assert_eq!(live.proof_bytes, Some(14));
        assert!(!live.running());
    }

    #[test]
    fn renders_finished_prove_in_small_terminal() {
        let mut live = idle_prove();
        live.apply(LiveEvent::Witness { start: 5, end: 6, bytes: 43_000 });
        live.apply(LiveEvent::Digest("0x35295b7a37d3".to_string()));
        live.apply(LiveEvent::Done { proof_bytes: Some(14_000_000) });
        let mut view = OpenVmView { live: Some(live), started: true };
        let resources = Resources::new(MonitoringConfig::mainnet());

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
        terminal.draw(|frame| view.render(frame, frame.area(), &resources)).unwrap();
        let screen: String =
            terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect();
        assert!(screen.contains("5→6"));
        assert!(screen.contains("done"));
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let raw =
            "\u{1b}[2m2026-09-25T22:56:57Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m Dropping old batch";
        assert_eq!(strip_ansi(raw), "2026-09-25T22:56:57Z  WARN Dropping old batch");
    }
}
