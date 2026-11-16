//! Test helpers shared by the conductor view's tests and the gateway
//! proposal flow's tests: gateway-mode resources and frame rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};
use tokio::sync::mpsc;

use crate::{
    ConductorPollUpdate,
    app::{GatewayClusterView, Resources, View, views::ConductorView},
};

pub(super) fn gateway_resources(fixture: Option<&str>) -> Resources {
    let mut config = crate::MonitoringConfig::mainnet();
    config.gate_url = Some(url::Url::parse("https://gate.example").unwrap());
    let mut resources = Resources::new(config);
    let (tx, rx) = mpsc::channel(4);
    resources.conductor.set_channel(rx);
    resources.conductor.gateway = Some(GatewayClusterView::default());
    if let Some(fixture) = fixture {
        let cluster = serde_json::from_str(fixture).unwrap();
        tx.try_send(ConductorPollUpdate::GateSnapshot(Box::new(cluster))).unwrap();
    }
    resources.conductor.poll();
    resources
}

pub(super) fn draw(view: &mut ConductorView, resources: &Resources) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            view.render(frame, area, resources);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

pub(super) fn rows(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

/// Foreground colour of the first cell of `text` on screen.
pub(super) fn fg_of(buffer: &Buffer, text: &str) -> Color {
    let rows = rows(buffer);
    let (y, row) = rows.iter().enumerate().find(|(_, row)| row.contains(text)).unwrap();
    let x = row[..row.find(text).unwrap()].chars().count();
    buffer[(x as u16, y as u16)].fg
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
