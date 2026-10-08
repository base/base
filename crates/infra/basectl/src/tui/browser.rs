use std::process::Command;

use super::Toast;

/// Opens URLs in the user's default web browser.
#[derive(Debug, Clone, Copy)]
pub struct Browser;

impl Browser {
    /// Platform command that hands a URL to the default browser.
    const OPEN_COMMAND: &str = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };

    /// Launches `url` in the default browser without blocking the caller.
    ///
    /// Returns a toast describing the outcome for display in the TUI. The
    /// launcher process is reaped on a background thread so it never lingers
    /// as a zombie.
    pub fn open(url: &str) -> Toast {
        match Command::new(Self::OPEN_COMMAND).arg(url).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Toast::info(format!("Opening {url}"))
            }
            Err(e) => Toast::warning(format!("Failed to open browser: {e}")),
        }
    }
}
