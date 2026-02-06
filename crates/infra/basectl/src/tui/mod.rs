pub mod app_frame;
pub mod help_sidebar;
pub mod homescreen;
pub mod keybinding;
pub mod status_bar;
pub mod terminal;

pub use app_frame::{AppFrame, AppLayout};
pub use help_sidebar::HelpSidebar;
pub use homescreen::{HomeSelection, run_homescreen};
pub use keybinding::Keybinding;
pub use status_bar::{StatusBar, StatusInfo};
pub use terminal::{restore_terminal, setup_terminal};

/// Result of a view navigation action
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavResult {
    /// Return to homescreen
    Home,
    /// Quit the application
    Quit,
}
