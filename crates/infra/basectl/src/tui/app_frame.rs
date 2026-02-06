use ratatui::{layout::Rect, prelude::*};

use super::{HelpSidebar, Keybinding, StatusBar, StatusInfo};

/// Layout areas computed by `AppFrame`.
#[derive(Debug)]
pub struct AppLayout {
    /// Main content area.
    pub content: Rect,
    /// Sidebar area (if help is shown).
    pub sidebar: Option<Rect>,
    /// Status bar area at the bottom.
    pub status_bar: Rect,
}

/// App frame component that provides consistent layout for all views.
///
/// Handles:
/// - Main content area
/// - Optional help sidebar (right)
/// - Status bar (bottom)
#[derive(Debug)]
pub struct AppFrame;

impl AppFrame {
    /// Splits the given area into content, optional sidebar, and status bar areas.
    pub fn split_layout(area: Rect, show_help: bool) -> AppLayout {
        // Reserve space for status bar at the bottom
        let status_height = StatusBar::height();
        let main_height = area.height.saturating_sub(status_height);

        let main_area = Rect { x: area.x, y: area.y, width: area.width, height: main_height };

        let status_bar =
            Rect { x: area.x, y: area.y + main_height, width: area.width, height: status_height };

        // Split main area for help sidebar if needed
        let (content, sidebar) = if show_help {
            let (content_area, sidebar_area) = HelpSidebar::split_layout(main_area);
            (content_area, Some(sidebar_area))
        } else {
            (main_area, None)
        };

        AppLayout { content, sidebar, status_bar }
    }

    /// Renders the frame components (status bar and optional help sidebar).
    pub fn render(
        f: &mut Frame,
        layout: &AppLayout,
        config_name: &str,
        keybindings: &[Keybinding],
        status_info: Option<&StatusInfo>,
    ) {
        // Render status bar
        StatusBar::render(f, layout.status_bar, config_name, status_info);

        // Render help sidebar if visible
        if let Some(sidebar_area) = layout.sidebar {
            HelpSidebar::render(f, sidebar_area, keybindings);
        }
    }
}
