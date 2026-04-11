//! Terminal UI framework: frame layout, keybindings, and terminal lifecycle.

/// Application frame layout and help sidebar.
mod app_frame;
pub use app_frame::{AppFrame, AppLayout};

/// Keybinding display types.
mod keybinding;
pub use keybinding::Keybinding;

/// Terminal setup and teardown utilities.
mod terminal;
pub use terminal::{restore_terminal, setup_terminal};

/// Toast notification system.
mod toast;
pub use toast::{Toast, ToastLevel, ToastState};
