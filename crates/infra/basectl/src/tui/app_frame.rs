use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use url::Url;

use super::Keybinding;
use crate::{config::MonitoringConfig, output::COLOR_BASE_BLUE};

const HELP_SIDEBAR_WIDTH: u16 = 30;

/// Layout regions produced by splitting the terminal area.
#[derive(Debug)]
pub struct AppLayout {
    /// Main content area for the active view.
    pub content: Rect,
    /// Optional help sidebar area.
    pub sidebar: Option<Rect>,
}

/// Whether the network badge shows the consensus-layer (CL) endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsensusBadge {
    /// Show `CL <host:port>`, or `CL -` when none is configured.
    Shown,
    /// Omit the CL segment.
    Hidden,
}

impl ConsensusBadge {
    /// Hidden only in approval-gateway mode with no CL configured, where
    /// `CL -` says nothing useful; otherwise the badge is unchanged.
    pub const fn resolve(config: &MonitoringConfig, gateway_mode: bool) -> Self {
        if gateway_mode && config.consensus_node_rpc.is_none() { Self::Hidden } else { Self::Shown }
    }
}

/// Handles the top-level application frame layout and help sidebar rendering.
#[derive(Debug)]
pub struct AppFrame;

impl AppFrame {
    /// Splits the terminal area into content and optional help sidebar.
    pub fn split_layout(area: Rect, show_help: bool) -> AppLayout {
        if show_help && area.width > HELP_SIDEBAR_WIDTH + 20 {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(HELP_SIDEBAR_WIDTH)])
                .split(area);

            AppLayout { content: chunks[0], sidebar: Some(chunks[1]) }
        } else {
            AppLayout { content: area, sidebar: None }
        }
    }

    /// Renders the network badge (always) and the help sidebar (when visible).
    pub fn render(
        f: &mut Frame<'_>,
        layout: &AppLayout,
        config: &MonitoringConfig,
        using_public_rpc: bool,
        pending_network: bool,
        consensus: ConsensusBadge,
        keybindings: &[Keybinding],
    ) {
        render_network_badge(
            f,
            layout.content,
            config,
            using_public_rpc,
            pending_network,
            consensus,
        );
        if let Some(sidebar) = layout.sidebar {
            render_help_sidebar(f, sidebar, &config.name, keybindings);
        }
    }

    /// Returns a compact host-and-port label for an RPC URL.
    pub fn endpoint_label(url: &Url) -> String {
        let origin = url.origin().ascii_serialization();
        origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .unwrap_or(&origin)
            .to_string()
    }
}

/// Renders active RPC endpoints in a badge pinned to the top-right corner.
fn render_network_badge(
    f: &mut Frame<'_>,
    area: Rect,
    config: &MonitoringConfig,
    using_public_rpc: bool,
    pending_network: bool,
    consensus: ConsensusBadge,
) {
    let mode = if using_public_rpc { "pub" } else { "cfg" };
    let toggle = if config.public_rpc.is_some() {
        if using_public_rpc { " | e:cfg" } else { " | e:pub" }
    } else {
        ""
    };
    let short_badge = format!(" [{} | EL {mode}{toggle}] ", config.name);
    let badge = if pending_network {
        " [...] ".to_string()
    } else {
        let el = AppFrame::endpoint_label(&config.rpc);
        let cl = match consensus {
            ConsensusBadge::Shown => {
                let endpoint = config
                    .consensus_node_rpc
                    .as_ref()
                    .map(AppFrame::endpoint_label)
                    .unwrap_or_else(|| "-".to_string());
                format!(" | CL {endpoint}")
            }
            ConsensusBadge::Hidden => String::new(),
        };
        format!(" [{} | EL {mode} {el}{cl}{toggle}] ", config.name)
    };
    let badge_width = Line::from(badge.as_str()).width() as u16;
    if area.height == 0 {
        return;
    }
    let badge = if !pending_network && area.width < badge_width.saturating_add(20) {
        short_badge
    } else {
        badge
    };
    let badge_width = Line::from(badge.as_str()).width() as u16;
    if area.width < badge_width {
        return;
    }
    let badge_area =
        Rect { x: area.x + area.width - badge_width, y: area.y, width: badge_width, height: 1 };
    f.render_widget(
        Paragraph::new(badge)
            .style(Style::default().fg(COLOR_BASE_BLUE).add_modifier(Modifier::BOLD)),
        badge_area,
    );
}

fn render_help_sidebar(
    f: &mut Frame<'_>,
    area: Rect,
    config_name: &str,
    keybindings: &[Keybinding],
) {
    let block = Block::default()
        .title(format!(" Help [{config_name}] "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = keybindings
        .iter()
        .map(|kb| {
            Line::from(vec![
                Span::styled(format!("{:>12}", kb.key), Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(kb.description, Style::default().fg(Color::White)),
            ])
        })
        .collect();

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("           n", Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled("Switch network", Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("           e", Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled("Toggle configured/public EL", Style::default().fg(Color::White)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("           ?", Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled("Close help", Style::default().fg(Color::White)),
    ]));

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, text::Line};
    use url::Url;

    use super::{AppFrame, ConsensusBadge};
    use crate::config::MonitoringConfig;

    fn header(config: &MonitoringConfig, consensus: ConsensusBadge) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                let layout = AppFrame::split_layout(frame.area(), false);
                AppFrame::render(frame, &layout, config, false, false, consensus, &[]);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..120).map(|x| buffer[(x, 0)].symbol()).collect::<String>().trim().to_string()
    }

    #[test]
    fn gateway_mode_header_omits_the_unconfigured_cl() {
        let mut config = MonitoringConfig::mainnet();
        config.consensus_node_rpc = None;
        config.public_rpc = None;
        let el = AppFrame::endpoint_label(&config.rpc);

        let consensus = ConsensusBadge::resolve(&config, true);
        assert_eq!(consensus, ConsensusBadge::Hidden);
        assert_eq!(header(&config, consensus), format!("[{} | EL cfg {el}]", config.name));

        // Direct conductor config: unchanged, including `CL -`.
        let consensus = ConsensusBadge::resolve(&config, false);
        assert_eq!(consensus, ConsensusBadge::Shown);
        assert_eq!(header(&config, consensus), format!("[{} | EL cfg {el} | CL -]", config.name));

        // A configured CL stays visible in gateway mode too.
        config.consensus_node_rpc = Some(Url::parse("http://127.0.0.1:7545").unwrap());
        let consensus = ConsensusBadge::resolve(&config, true);
        assert_eq!(
            header(&config, consensus),
            format!("[{} | EL cfg {el} | CL 127.0.0.1:7545]", config.name)
        );
    }

    #[test]
    fn unicode_badge_width_uses_display_columns() {
        let badge = " [网络 | EL cfg] ";

        assert_eq!(Line::from(badge).width(), 17);
        assert_ne!(Line::from(badge).width(), badge.len());
    }
}
