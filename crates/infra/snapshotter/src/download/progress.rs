//! Progress tracking for snapshot downloads.
//!
//! Provides multi-bar progress display using [`indicatif`] with per-archive
//! download, extraction, and verification tracking.

use std::{sync::Arc, time::Duration};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Style template for download progress bars.
const DOWNLOAD_TEMPLATE: &str =
    "{msg:>24} [{bar:30.cyan/blue}] {binary_bytes:>8}/{binary_total_bytes:8} ({eta})";

/// Style template for spinner-based extraction/verification bars.
const SPINNER_TEMPLATE: &str = "{msg:>24} {spinner:.green} {wide_msg}";

/// Manages multiple progress bars for concurrent archive downloads.
#[derive(Debug, Clone)]
pub struct DownloadProgressTracker {
    multi: Arc<MultiProgress>,
}

impl DownloadProgressTracker {
    /// Creates a new progress tracker.
    pub fn new() -> Self {
        Self { multi: Arc::new(MultiProgress::new()) }
    }

    /// Creates a download progress bar for an archive of known size.
    pub fn add_download_bar(&self, name: &str, total_bytes: u64) -> ProgressBar {
        let pb = self.multi.add(ProgressBar::new(total_bytes));
        pb.set_style(
            ProgressStyle::with_template(DOWNLOAD_TEMPLATE)
                .expect("valid template")
                .progress_chars("=>-"),
        );
        pb.set_message(name.to_string());
        pb
    }

    /// Creates a spinner bar for extraction or verification phases.
    pub fn add_spinner(&self, name: &str) -> ProgressBar {
        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(ProgressStyle::with_template(SPINNER_TEMPLATE).expect("valid template"));
        pb.set_message(name.to_string());
        pb.enable_steady_tick(Duration::from_millis(120));
        pb
    }

    /// Creates a summary bar showing overall progress (N/M archives).
    pub fn add_summary_bar(&self, total_archives: u64) -> ProgressBar {
        let pb = self.multi.add(ProgressBar::new(total_archives));
        pb.set_style(
            ProgressStyle::with_template(
                "{msg:>24} [{bar:30.green/black}] {pos}/{len} archives ({elapsed})",
            )
            .expect("valid template")
            .progress_chars("=>-"),
        );
        pb.set_message("overall");
        pb
    }
}

impl Default for DownloadProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Formats a byte count into a human-readable string.
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }

    format!("{size:.2} {}", UNITS[unit_idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0.00 B");
        assert_eq!(format_size(512), "512.00 B");
    }

    #[test]
    fn format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536), "1.50 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1_048_576), "1.00 MB");
    }

    #[test]
    fn format_size_gigabytes() {
        assert_eq!(format_size(1_073_741_824), "1.00 GB");
    }

    #[test]
    fn format_size_terabytes() {
        assert_eq!(format_size(1_099_511_627_776), "1.00 TB");
    }
}
