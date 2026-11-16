//! Approval gateway proposal flow for the conductor view: proposing a leader
//! transfer, tracking this session's proposals, and executing an approved one.
//!
//! [`GateProposals`] owns the proposal list, its overlays and the background
//! gateway requests; [`ConductorView`](super::ConductorView) delegates to it.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, PoisonError},
};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::{Mutex, Semaphore, mpsc};

use super::conductor::{CONFIRM_KEYS, render_confirm};
use crate::{
    ConfirmButton, ExecuteResponse, GateClient, GateError, GateOutcomeKind, GateSettings,
    ProposalStatusResponse, ProposeResponse, app::Resources, output::COLOR_BASE_BLUE, tui::Toast,
};

/// Result of a background approval gateway request.
#[derive(Debug)]
pub enum GateEvent {
    /// Fresh proposal status, numbered in request order so an older reply
    /// never replaces a newer one.
    Status(u64, Box<ProposalStatusResponse>),
    /// A proposal was created.
    Proposed(ProposeResponse),
    /// Execute outcome (the transfer result) as reported by the gateway.
    Executed(ExecuteResponse),
    /// A request failed; the message carries the gateway's reason.
    Failed(String),
    /// An execute request failed; the message carries the gateway's reason.
    ExecuteFailed(String),
}

/// Modal overlay owned by [`GateProposals`].
#[derive(Debug, Default)]
pub enum GateOverlay {
    /// No gateway overlay is open.
    #[default]
    None,
    /// Reason input for a gateway leader transfer proposal.
    ProposalForm {
        /// Target node name.
        target: String,
        /// Reason typed so far.
        reason: String,
    },
    /// Proposals tracked this session, with the execute action.
    Proposals {
        /// Currently-highlighted proposal index.
        cursor: usize,
    },
    /// Proposal ID input for tracking an existing proposal.
    OpenProposal {
        /// ID typed so far.
        input: String,
    },
    /// Yes/No confirmation before executing an approved proposal, which
    /// transfers leadership now.
    ConfirmExecute {
        /// Proposal ID.
        id: String,
        /// Target raft server ID, for the prompt.
        target: String,
        /// Currently-highlighted confirm button.
        button: ConfirmButton,
    },
}

/// Gateway proposals created or opened this session (memory only), their
/// overlays, and the background requests that keep them current.
#[derive(Debug, Default)]
pub struct GateProposals {
    overlay: GateOverlay,
    /// Gateway proposals created or opened this session (memory only).
    proposals: Vec<ProposalStatusResponse>,
    /// Sender cloned into background gateway requests.
    tx: Option<mpsc::Sender<GateEvent>>,
    /// Receiver drained on each tick.
    rx: Option<mpsc::Receiver<GateEvent>>,
    /// Proposal whose execute request is in flight. While set, `x` is
    /// ignored so a second execute isn't sent for the same transfer.
    executing: Option<String>,
    /// Gateway client shared by every request, built on first use (building
    /// it may run `gate_token_command`). Rebuilt when the resolved settings
    /// change or after a failed request, so an expired token is replaced.
    client: Arc<Mutex<Option<(GateSettings, GateClient)>>>,
    /// Proposal IDs with a status request in flight; a refresh of one of
    /// them is dropped, since the in-flight request will report.
    refreshing: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Shared cap on concurrent status requests, built on first use.
    status_slots: Option<Arc<Semaphore>>,
    /// Number of the last status request started.
    status_seq: u64,
    /// Number of the newest status applied, per proposal ID.
    status_applied: HashMap<String, u64>,
}

/// Clears a proposal's in-flight mark when its status request ends, whether
/// it succeeds, fails, or is dropped before it runs.
struct Refreshing {
    ids: Arc<std::sync::Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for Refreshing {
    fn drop(&mut self) {
        self.ids.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

impl GateProposals {
    /// Most status requests in flight at once, across all proposal IDs. A
    /// session tracks a handful of proposals, so this refreshes them all
    /// promptly while a long list on a slow gateway can't open a connection
    /// per row.
    const MAX_STATUS_REQUESTS: usize = 4;

    /// Footer keys while the proposal form or the open-by-ID input is shown.
    const INPUT_KEYS: &[(&str, &str)] = &[("Enter", "submit"), ("Esc", "cancel")];
    /// Footer keys while the proposals list is shown.
    const LIST_KEYS: &[(&str, &str)] =
        &[("x", "execute"), ("o", "open by ID"), ("r", "refresh"), ("Esc", "close")];

    /// The open gateway overlay.
    pub const fn overlay(&self) -> &GateOverlay {
        &self.overlay
    }

    /// Whether one of the gateway overlays is open.
    pub const fn is_open(&self) -> bool {
        !matches!(self.overlay, GateOverlay::None)
    }

    /// Closes any gateway overlay.
    pub fn close(&mut self) {
        self.overlay = GateOverlay::None;
    }

    /// Opens the reason form for a proposal to transfer leadership to `target`.
    pub fn open_form(&mut self, target: String) {
        self.overlay = GateOverlay::ProposalForm { target, reason: String::new() };
    }

    /// Opens the proposals list and refreshes every tracked proposal.
    pub fn open_list(&mut self, resources: &Resources) {
        self.overlay = GateOverlay::Proposals { cursor: 0 };
        let ids = self.proposals.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
        for id in ids {
            self.refresh(resources, id);
        }
    }

    /// Footer key hints for the open overlay, if any.
    pub const fn footer_keys(&self) -> Option<&'static [(&'static str, &'static str)]> {
        match self.overlay {
            GateOverlay::None => None,
            GateOverlay::ProposalForm { .. } | GateOverlay::OpenProposal { .. } => {
                Some(Self::INPUT_KEYS)
            }
            GateOverlay::Proposals { .. } => Some(Self::LIST_KEYS),
            GateOverlay::ConfirmExecute { .. } => Some(CONFIRM_KEYS),
        }
    }

    /// Applies finished gateway requests.
    pub fn tick(&mut self, resources: &mut Resources) {
        while let Some(event) = self.rx.as_mut().and_then(|rx| rx.try_recv().ok()) {
            self.apply_event(event, resources);
        }
    }

    /// Handles a key while a gateway overlay is open.
    pub fn handle_key(&mut self, key: KeyEvent, resources: &mut Resources) {
        match &mut self.overlay {
            GateOverlay::None => {}
            GateOverlay::ProposalForm { target, reason } => match key.code {
                KeyCode::Esc => self.close(),
                KeyCode::Backspace => {
                    reason.pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && reason.chars().count() < GateClient::MAX_REASON_CHARS =>
                {
                    reason.push(c);
                }
                KeyCode::Enter if GateClient::sanitize_reason(reason).is_empty() => {
                    resources.toasts.push(Toast::warning("reason required"));
                }
                KeyCode::Enter => {
                    let reason = reason.clone();
                    let target_id = resources
                        .conductor
                        .server_id_for(target)
                        .map_or_else(|| target.clone(), str::to_string);
                    self.overlay = GateOverlay::Proposals { cursor: 0 };
                    self.spawn(resources, GateEvent::Failed, move |client| async move {
                        client.propose(&target_id, &reason).await.map_or_else(
                            |e| GateEvent::Failed(e.tui_message()),
                            GateEvent::Proposed,
                        )
                    });
                }
                _ => {}
            },
            GateOverlay::OpenProposal { input } => match key.code {
                KeyCode::Esc => self.overlay = GateOverlay::Proposals { cursor: 0 },
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c)
                    if GateClient::is_id_char(c)
                        && input.len() < GateClient::MAX_PROPOSAL_ID_LEN =>
                {
                    input.push(c)
                }
                KeyCode::Enter => {
                    let id = input.clone();
                    self.overlay = GateOverlay::Proposals { cursor: 0 };
                    self.refresh(resources, id);
                }
                _ => {}
            },
            GateOverlay::Proposals { cursor } => {
                let count = self.proposals.len();
                let selected = self.proposals.get(*cursor).cloned();
                match key.code {
                    KeyCode::Esc => self.close(),
                    KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                        *cursor = (*cursor + count - 1) % count;
                    }
                    KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                        *cursor = (*cursor + 1) % count;
                    }
                    KeyCode::Char('o') => {
                        self.overlay = GateOverlay::OpenProposal { input: String::new() }
                    }
                    KeyCode::Char('r') => {
                        let ids = self.proposals.iter().map(|p| p.id.clone()).collect::<Vec<_>>();
                        for id in ids {
                            self.refresh(resources, id);
                        }
                    }
                    // One execute at a time: `x` does nothing while one runs.
                    KeyCode::Char('x') if self.executing.is_none() => {
                        if let Some(status) = selected {
                            self.overlay = GateOverlay::ConfirmExecute {
                                target: status
                                    .action
                                    .as_ref()
                                    .map_or_else(|| "?".to_string(), |a| a.target_id.clone()),
                                id: status.id,
                                button: ConfirmButton::No,
                            };
                        }
                    }
                    _ => {}
                }
            }
            GateOverlay::ConfirmExecute { id, button, .. } => match key.code {
                KeyCode::Esc | KeyCode::Char('n' | 'N') => self.close(),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    *button = match *button {
                        ConfirmButton::Yes => ConfirmButton::No,
                        ConfirmButton::No => ConfirmButton::Yes,
                    };
                }
                KeyCode::Char('y' | 'Y') => {
                    let id = id.clone();
                    self.execute(id, resources);
                }
                KeyCode::Enter => match button {
                    ConfirmButton::Yes => {
                        let id = id.clone();
                        self.execute(id, resources);
                    }
                    ConfirmButton::No => self.close(),
                },
                _ => {}
            },
        }
    }

    /// Draws the open gateway overlay, if any.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        match &self.overlay {
            GateOverlay::None => {}
            GateOverlay::ProposalForm { target, reason } => {
                let title = format!(" Propose leader transfer → {target} ");
                let inner = Self::render_popup(frame, area, 76, 7, title);
                let lines = vec![
                    Line::raw("A different operator approves it at the review URL; then execute."),
                    Line::raw(""),
                    Line::styled(format!("Reason: {reason}"), Style::default().fg(Color::Green)),
                ];
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
            GateOverlay::OpenProposal { input } => {
                let inner =
                    Self::render_popup(frame, area, 60, 3, " Open proposal by ID ".to_string());
                frame.render_widget(Paragraph::new(format!("ID: {input}")), inner);
            }
            GateOverlay::Proposals { cursor } => self.render_proposals(frame, area, *cursor),
            GateOverlay::ConfirmExecute { target, button, .. } => {
                let description =
                    format!("Executing will transfer leadership to {} now.", target.escape_debug());
                render_confirm(frame, area, &description, true, *button);
            }
        }
    }

    /// Executes approved proposal `id`, at most one at a time.
    fn execute(&mut self, id: String, resources: &Resources) {
        self.overlay = GateOverlay::Proposals { cursor: 0 };
        if self.executing.is_some() {
            return;
        }
        self.executing = Some(id.clone());
        let spawned = self.spawn(resources, GateEvent::ExecuteFailed, move |client| async move {
            client
                .execute(&id)
                .await
                .map_or_else(|e| GateEvent::ExecuteFailed(e.tui_message()), GateEvent::Executed)
        });
        if !spawned {
            self.executing = None;
        }
    }

    /// Runs one gateway request off the event loop on the shared client; the
    /// result arrives in `tick`, or `failed` if no client could be built.
    /// Returns `false` when no gateway is configured and nothing was started.
    fn spawn<F, Fut>(
        &mut self,
        resources: &Resources,
        failed: fn(String) -> GateEvent,
        job: F,
    ) -> bool
    where
        F: FnOnce(GateClient) -> Fut + Send + 'static,
        Fut: Future<Output = GateEvent> + Send + 'static,
    {
        let gate = match GateSettings::resolve(&resources.config, None, None) {
            Ok(Some(gate)) => Ok(gate),
            Ok(None) => return false,
            Err(error) => Err(error),
        };
        let tx = match &self.tx {
            Some(tx) => tx.clone(),
            None => {
                let (tx, rx) = mpsc::channel(16);
                self.rx = Some(rx);
                self.tx.insert(tx).clone()
            }
        };
        let shared = Arc::clone(&self.client);
        tokio::spawn(async move {
            let client = match gate {
                Ok(gate) => Self::shared_client(&shared, gate).await,
                Err(error) => Err(error),
            };
            let event = match client {
                Ok(client) => job(client).await,
                Err(error) => failed(error.tui_message()),
            };
            if matches!(event, GateEvent::Failed(_) | GateEvent::ExecuteFailed(_)) {
                *shared.lock().await = None;
            }
            let _ = tx.send(event).await;
        });
        true
    }

    /// The shared client for `gate`, built if missing or built from other
    /// settings. Concurrent callers wait on the lock, so one build serves
    /// them all; a failed build isn't kept.
    async fn shared_client(
        shared: &Mutex<Option<(GateSettings, GateClient)>>,
        gate: GateSettings,
    ) -> Result<GateClient, GateError> {
        let mut slot = shared.lock().await;
        if let Some((built_from, client)) = slot.as_ref()
            && *built_from == gate
        {
            return Ok(client.clone());
        }
        let client = gate.client_without_prompt().await?;
        *slot = Some((gate, client.clone()));
        Ok(client)
    }

    /// Fetches fresh status for proposal `id`, unless a status request for
    /// it is already in flight.
    fn refresh(&mut self, resources: &Resources, id: String) {
        if !self.refreshing.lock().unwrap_or_else(PoisonError::into_inner).insert(id.clone()) {
            return;
        }
        let mark = Refreshing { ids: Arc::clone(&self.refreshing), id: id.clone() };
        let slots = Arc::clone(
            self.status_slots
                .get_or_insert_with(|| Arc::new(Semaphore::new(Self::MAX_STATUS_REQUESTS))),
        );
        self.status_seq += 1;
        let seq = self.status_seq;
        self.spawn(resources, GateEvent::Failed, move |client| async move {
            let _mark = mark;
            let _slot = slots.acquire_owned().await;
            client.status(&id).await.map_or_else(
                |e| GateEvent::Failed(e.tui_message()),
                |status| GateEvent::Status(seq, Box::new(status)),
            )
        });
    }

    /// Applies a finished gateway request to the session list and toasts.
    fn apply_event(&mut self, event: GateEvent, resources: &mut Resources) {
        match event {
            GateEvent::Status(seq, status) => {
                let applied = self.status_applied.entry(status.id.clone()).or_default();
                if seq <= *applied {
                    return;
                }
                *applied = seq;
                match self.proposals.iter_mut().find(|known| known.id == status.id) {
                    Some(known) => *known = *status,
                    None => self.proposals.push(*status),
                }
            }
            GateEvent::Proposed(created) => {
                let id = created.id.escape_debug().to_string();
                let approve_at = created.review_url.as_deref().map_or_else(
                    || "in the approval portal".to_string(),
                    |url| format!("at:\n{}", url.escape_debug()),
                );
                resources.toasts.push(Toast::sticky_info(format!(
                    "Proposed {id}. Ask a different engineer to approve it {approve_at}\nthen: basectl conductor transfer execute {id}\n(any key closes)"
                )));
                self.refresh(resources, created.id);
            }
            GateEvent::Executed(result) => {
                self.executing = None;
                let message = result.message();
                resources.toasts.push(if result.outcome == GateOutcomeKind::Succeeded {
                    Toast::info(message)
                } else {
                    Toast::warning(message)
                });
                self.refresh(resources, result.id);
            }
            GateEvent::Failed(message) => resources.toasts.push(Toast::warning(message)),
            GateEvent::ExecuteFailed(message) => {
                self.executing = None;
                resources.toasts.push(Toast::warning(message));
            }
        }
    }

    /// Clears a centered popup, draws its titled border, and returns the inner area.
    fn render_popup(f: &mut Frame<'_>, area: Rect, width: u16, height: u16, title: String) -> Rect {
        let popup_w = width.min(area.width.saturating_sub(4));
        let popup_h = height.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(popup_w) / 2;
        let y = area.y + area.height.saturating_sub(popup_h) / 2;
        let popup = Rect { x, y, width: popup_w, height: popup_h };
        f.render_widget(Clear, popup);
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(COLOR_BASE_BLUE));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        inner
    }

    fn render_proposals(&self, f: &mut Frame<'_>, area: Rect, cursor: usize) {
        let proposals = &self.proposals;
        let executing = self.executing.as_deref();
        let title = " Leader transfer proposals (this session) ".to_string();
        let inner = Self::render_popup(f, area, 100, proposals.len() as u16 + 14, title);
        let now = Utc::now();
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines: Vec<Line<'_>> = Vec::new();
        if proposals.is_empty() {
            lines.push(Line::styled(
                "No proposals yet. Press t to propose or o to open by ID.",
                dim,
            ));
        }
        for (i, status) in proposals.iter().enumerate() {
            let state = if executing == Some(status.id.as_str()) {
                "executing…"
            } else {
                status.outcome.as_ref().map_or(status.status.as_str(), |o| o.label())
            };
            let style = if i == cursor {
                Style::default()
                    .fg(COLOR_BASE_BLUE)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(Color::White)
            };
            let target = status.action.as_ref().map_or("?", |a| a.target_id.as_str());
            let label = format!(
                "{}  {:<11} → {}  author {}",
                status.id.escape_debug(),
                state.escape_debug(),
                target.escape_debug(),
                status.author.escape_debug()
            );
            lines.push(Line::styled(label, style));
        }
        if let Some(status) = proposals.get(cursor) {
            lines.push(Line::raw(""));
            lines.extend(status.lines(now).into_iter().map(Line::raw));
            let label = "[x] execute (transfers leadership now)";
            let hint = if executing.is_some() {
                Some("an execute is already running".to_string())
            } else {
                status.execute_hint()
            };
            lines.push(hint.map_or_else(
                || Line::styled(label, Style::default().fg(Color::Yellow)),
                |reason| Line::styled(format!("{label} — unavailable: {reason}"), dim),
            ));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::{
        app::{
            View,
            views::{
                ConductorView,
                conductor_test_utils::{draw, gateway_resources, key, rows},
            },
        },
        gate_cluster_fixtures::CLUSTER_HEALTHY,
    };

    #[tokio::test]
    async fn second_execute_is_ignored_while_one_is_in_flight() {
        // A gateway whose execute never answers, counting requests.
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = axum::Router::new().route(
            "/admin/conductor/proposals/{id}/execute",
            axum::routing::post(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<String>()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let mut config = crate::MonitoringConfig::mainnet();
        config.gate_url = Some(url::Url::parse(&format!("http://{address}/")).unwrap());
        config.gate_token_command = Some("echo tok".to_string());
        let mut resources = Resources::new(config);
        let mut view = ConductorView::new();
        view.gate.proposals.push(
            serde_json::from_value(serde_json::json!({
                "id": "rev_1", "status": "approved", "author": "alice",
                "action": {"target_id": "sequencer-1"},
                "approval": {"approvers": ["bob"], "approved_at": "2026-01-01T00:00:00Z"},
            }))
            .unwrap(),
        );
        view.gate.overlay = GateOverlay::Proposals { cursor: 0 };

        view.handle_key(key(KeyCode::Char('x')), &mut resources);
        assert!(matches!(view.gate.overlay, GateOverlay::ConfirmExecute { .. }));
        view.handle_key(key(KeyCode::Char('y')), &mut resources);
        assert!(matches!(view.gate.overlay, GateOverlay::Proposals { .. }));
        assert_eq!(view.gate.executing.as_deref(), Some("rev_1"));
        let buffer = draw(&mut view, &resources);
        let screen = rows(&buffer).join("\n");
        assert!(screen.contains("rev_1  executing…"), "{screen}");

        // Wait for the first execute to reach the gateway (token command plus
        // HTTP request can be slow under load).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while hits.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "first execute didn't reach the mock gateway within 5 s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // While it runs, `x` (then Yes) opens nothing and sends nothing.
        view.handle_key(key(KeyCode::Char('x')), &mut resources);
        assert!(matches!(view.gate.overlay, GateOverlay::Proposals { .. }));
        view.handle_key(key(KeyCode::Char('y')), &mut resources);
        assert!(matches!(view.gate.overlay, GateOverlay::Proposals { .. }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // A finished execute clears the flag, and `x` works again.
        view.gate.apply_event(GateEvent::ExecuteFailed("boom".to_string()), &mut resources);
        assert!(view.gate.executing.is_none());
        view.handle_key(key(KeyCode::Char('x')), &mut resources);
        assert!(matches!(view.gate.overlay, GateOverlay::ConfirmExecute { .. }));
    }

    #[tokio::test]
    async fn refreshing_proposals_runs_the_token_command_once() {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = axum::Router::new().route(
            "/admin/conductor/proposals/{id}",
            axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { serde_json::json!({"id": id, "status": "pending"}).to_string() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        let mut config = crate::MonitoringConfig::mainnet();
        config.gate_url = Some(url::Url::parse(&format!("http://{address}/")).unwrap());
        config.gate_token_command = Some(format!("echo run >> '{}'; echo tok", runs.display()));
        let mut resources = Resources::new(config);
        let mut view = ConductorView::new();
        for id in ["rev_1", "rev_2", "rev_3"] {
            view.gate
                .proposals
                .push(serde_json::from_value(serde_json::json!({"id": id})).unwrap());
        }

        // `p` refreshes every tracked proposal, concurrently.
        view.handle_key(key(KeyCode::Char('p')), &mut resources);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while hits.load(Ordering::SeqCst) < 3 {
            assert!(tokio::time::Instant::now() < deadline, "refreshes didn't reach the gateway");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
    }

    /// A gateway whose status replies take `delay`, counting requests.
    async fn delayed_gateway(delay: Duration) -> (Resources, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let router = axum::Router::new().route(
            "/admin/conductor/proposals/{id}",
            axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    tokio::time::sleep(delay).await;
                    serde_json::json!({"id": id, "status": "pending"}).to_string()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut config = crate::MonitoringConfig::mainnet();
        config.gate_url = Some(url::Url::parse(&format!("http://{address}/")).unwrap());
        config.gate_token_command = Some("echo tok".to_string());
        (Resources::new(config), hits)
    }

    async fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "{what} within 5 s");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn repeated_refreshes_of_one_proposal_send_one_request() {
        let (mut resources, hits) = delayed_gateway(Duration::from_millis(500)).await;
        let mut gate = GateProposals {
            overlay: GateOverlay::Proposals { cursor: 0 },
            proposals: vec![serde_json::from_value(serde_json::json!({"id": "rev_1"})).unwrap()],
            ..Default::default()
        };

        for _ in 0..10 {
            gate.handle_key(key(KeyCode::Char('r')), &mut resources);
        }
        wait_for("first refresh reaches the gateway", || hits.load(Ordering::SeqCst) > 0).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Once it answers, the next refresh goes out.
        let refreshing = Arc::clone(&gate.refreshing);
        wait_for("in-flight mark clears", || refreshing.lock().unwrap().is_empty()).await;
        gate.handle_key(key(KeyCode::Char('r')), &mut resources);
        wait_for("second refresh reaches the gateway", || hits.load(Ordering::SeqCst) == 2).await;
    }

    #[tokio::test]
    async fn status_requests_are_capped_across_proposals() {
        let (mut resources, hits) = delayed_gateway(Duration::from_secs(60)).await;
        let proposals = (0..12)
            .map(|i| serde_json::from_value(serde_json::json!({"id": format!("rev_{i}")})).unwrap())
            .collect();
        let mut gate = GateProposals {
            overlay: GateOverlay::Proposals { cursor: 0 },
            proposals,
            ..Default::default()
        };

        gate.handle_key(key(KeyCode::Char('r')), &mut resources);
        let max = GateProposals::MAX_STATUS_REQUESTS;
        wait_for("capped refreshes reach the gateway", || hits.load(Ordering::SeqCst) == max).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hits.load(Ordering::SeqCst), max);
        assert_eq!(gate.refreshing.lock().unwrap().len(), 12);
    }

    #[test]
    fn unstarted_refresh_clears_its_in_flight_mark() {
        // No gateway configured: nothing starts, and the ID isn't left marked.
        let resources = Resources::new(crate::MonitoringConfig::mainnet());
        let mut gate = GateProposals::default();
        gate.refresh(&resources, "rev_1".to_string());
        assert!(gate.refreshing.lock().unwrap().is_empty());
    }

    #[test]
    fn older_status_reply_does_not_replace_a_newer_one() {
        let mut resources = Resources::new(crate::MonitoringConfig::mainnet());
        let mut gate = GateProposals::default();
        let status = |status: &str| {
            Box::new(
                serde_json::from_value(serde_json::json!({"id": "rev_1", "status": status}))
                    .unwrap(),
            )
        };

        gate.apply_event(GateEvent::Status(2, status("approved")), &mut resources);
        gate.apply_event(GateEvent::Status(1, status("pending")), &mut resources);
        assert_eq!(gate.proposals[0].status, "approved");
        gate.apply_event(GateEvent::Status(3, status("executed")), &mut resources);
        assert_eq!(gate.proposals[0].status, "executed");
    }

    #[test]
    fn open_by_id_accepts_only_proposal_id_characters_up_to_the_limit() {
        let mut resources = Resources::new(crate::MonitoringConfig::mainnet());
        let mut gate = GateProposals {
            overlay: GateOverlay::OpenProposal { input: String::new() },
            ..Default::default()
        };
        let input = |gate: &GateProposals| match &gate.overlay {
            GateOverlay::OpenProposal { input } => input.clone(),
            other => panic!("{other:?}"),
        };

        for c in ['/', '?', ' ', '.', '%'] {
            gate.handle_key(key(KeyCode::Char(c)), &mut resources);
            assert_eq!(input(&gate), "", "{c:?}");
        }
        for c in "aZ09_-".chars() {
            gate.handle_key(key(KeyCode::Char(c)), &mut resources);
        }
        assert_eq!(input(&gate), "aZ09_-");
        assert!(GateClient::check_id(&input(&gate)).is_ok());

        for _ in 0..GateClient::MAX_PROPOSAL_ID_LEN {
            gate.handle_key(key(KeyCode::Char('x')), &mut resources);
        }
        assert_eq!(input(&gate).len(), GateClient::MAX_PROPOSAL_ID_LEN);
        assert!(GateClient::check_id(&input(&gate)).is_ok());
    }

    #[tokio::test]
    async fn gateway_mode_t_proposes_to_the_gateway_server_id() {
        let mut resources = gateway_resources(Some(CLUSTER_HEALTHY));
        let mut view = ConductorView::new();
        view.selected = 1;

        view.handle_key(key(KeyCode::Char('t')), &mut resources);

        assert!(matches!(
            &view.gate.overlay,
            GateOverlay::ProposalForm { target, .. } if target == "sequencer-1"
        ));
        assert_eq!(resources.conductor.server_id_for("sequencer-1"), Some("sequencer-1"));
        assert_eq!(resources.conductor.server_id_for("unknown"), None);
    }

    #[tokio::test]
    async fn propose_toast_shows_the_full_review_url_on_one_row() {
        let mut resources = Resources::new(crate::MonitoringConfig::mainnet());
        let mut view = ConductorView::new();
        let url = "https://review.example/reviews/rev_42";
        view.gate.apply_event(
            GateEvent::Proposed(crate::ProposeResponse {
                id: "rev_42".to_string(),
                review_url: Some(url.to_string()),
            }),
            &mut resources,
        );
        for _ in 0..3 {
            resources.toasts.poll();
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                view.render(frame, area, &resources);
                resources.toasts.render(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..34)
            .map(|y| (0..120).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();

        assert!(rows.iter().any(|row| row.contains(url)), "{}", rows.join("\n"));
        assert!(rows.iter().any(|row| row.contains("basectl conductor transfer execute rev_42")));
        // Row 32 is the outer frame's bottom border (row 33 is the footer);
        // the toast must not draw over it.
        let frame_bottom = rows[32].trim_end();
        assert!(frame_bottom.starts_with('└') && frame_bottom.ends_with('┘'), "{frame_bottom}");
        let inside = &frame_bottom['└'.len_utf8()..frame_bottom.len() - '┘'.len_utf8()];
        assert!(!inside.contains(['└', '┘']), "{frame_bottom}");
        let toast_bottom = rows.iter().position(|row| row.contains("┘ │")).unwrap();
        assert!(toast_bottom < 32, "{}", rows.join("\n"));
        assert!(resources.toasts.dismiss_sticky());
        assert!(resources.toasts.current().is_none());
    }
}
