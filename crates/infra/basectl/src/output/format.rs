use std::time::Duration;

use chrono::DateTime;
use ratatui::prelude::Color;

const BLOCK_COLORS: [Color; 24] = [
    Color::Rgb(0, 82, 255),
    Color::Rgb(0, 140, 255),
    Color::Rgb(0, 180, 220),
    Color::Rgb(0, 190, 180),
    Color::Rgb(0, 180, 130),
    Color::Rgb(40, 180, 100),
    Color::Rgb(80, 180, 80),
    Color::Rgb(130, 180, 60),
    Color::Rgb(170, 170, 50),
    Color::Rgb(200, 160, 50),
    Color::Rgb(220, 140, 50),
    Color::Rgb(230, 110, 60),
    Color::Rgb(235, 90, 70),
    Color::Rgb(230, 70, 90),
    Color::Rgb(220, 60, 120),
    Color::Rgb(200, 60, 150),
    Color::Rgb(180, 70, 180),
    Color::Rgb(150, 80, 200),
    Color::Rgb(120, 90, 210),
    Color::Rgb(90, 100, 220),
    Color::Rgb(60, 110, 230),
    Color::Rgb(40, 130, 240),
    Color::Rgb(30, 160, 245),
    Color::Rgb(20, 180, 235),
];

/// Primary Base blue color.
pub const COLOR_BASE_BLUE: Color = Color::Rgb(0, 82, 255);
/// Active border highlight color.
pub const COLOR_ACTIVE_BORDER: Color = Color::Rgb(100, 180, 255);

/// Background color for the currently selected table row.
pub const COLOR_ROW_SELECTED: Color = Color::Rgb(60, 60, 80);
/// Background color for a highlighted (cross-referenced) table row.
pub const COLOR_ROW_HIGHLIGHTED: Color = Color::Rgb(40, 40, 60);

/// Color for DA growth rate indicators.
pub const COLOR_GROWTH: Color = Color::Rgb(255, 180, 100);
/// Color for DA burn rate indicators.
pub const COLOR_BURN: Color = Color::Rgb(100, 200, 100);
/// Color for gas target markers.
pub const COLOR_TARGET: Color = Color::Rgb(255, 200, 100);
/// Color for gas bar fill below target.
pub const COLOR_GAS_FILL: Color = Color::Rgb(100, 180, 255);

/// Formats a byte count into a human-readable string (e.g. "1.5M").
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1}G", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1}M", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.0}K", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes}B")
    }
}

/// Formats a gas value into a human-readable string (e.g. "30.0M").
pub fn format_gas(gas: u64) -> String {
    if gas >= 1_000_000 {
        format!("{:.1}M", gas as f64 / 1_000_000.0)
    } else if gas >= 1_000 {
        format!("{:.0}K", gas as f64 / 1_000.0)
    } else {
        gas.to_string()
    }
}

/// Truncates a block number to fit within `max_width` characters.
pub fn truncate_block_number(block_number: u64, max_width: usize) -> String {
    let s = block_number.to_string();
    if s.len() <= max_width { s } else { format!("…{}", &s[s.len() - (max_width - 1)..]) }
}

/// Formats a duration into a compact human-readable string (e.g. "2m30s").
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Formats a byte rate into a human-readable string (e.g. "1.2K/s").
pub fn format_rate(rate: Option<f64>) -> String {
    match rate {
        Some(r) if r >= 1_000_000.0 => format!("{:.1}M/s", r / 1_000_000.0),
        Some(r) if r >= 1_000.0 => format!("{:.1}K/s", r / 1_000.0),
        Some(r) => format!("{r:.0}B/s"),
        None => "-".to_string(),
    }
}

/// Formats a wei value as gwei with appropriate precision.
pub fn format_gwei(wei: u128) -> String {
    let gwei = wei as f64 / 1_000_000_000.0;
    if gwei >= 1.0 { format!("{gwei:.2} gwei") } else { format!("{gwei:.4} gwei") }
}

const BACKLOG_THRESHOLDS: &[(u64, Color)] = &[
    (5_000_000, Color::Rgb(100, 200, 100)),
    (10_000_000, Color::Rgb(150, 220, 100)),
    (20_000_000, Color::Rgb(200, 220, 80)),
    (30_000_000, Color::Rgb(240, 200, 60)),
    (45_000_000, Color::Rgb(255, 160, 60)),
    (60_000_000, Color::Rgb(255, 100, 80)),
];

/// Returns a color indicating backlog severity based on byte count.
pub fn backlog_size_color(bytes: u64) -> Color {
    BACKLOG_THRESHOLDS
        .iter()
        .find(|(threshold, _)| bytes < *threshold)
        .map_or(Color::Rgb(255, 80, 120), |(_, color)| *color)
}

/// Returns a unique color for the given block number.
pub const fn block_color(block_number: u64) -> Color {
    BLOCK_COLORS[(block_number as usize) % BLOCK_COLORS.len()]
}

/// Returns a brightened version of the block color for emphasis.
pub const fn block_color_bright(block_number: u64) -> Color {
    let Color::Rgb(r, g, b) = BLOCK_COLORS[(block_number as usize) % BLOCK_COLORS.len()] else {
        unreachable!()
    };
    Color::Rgb(
        r.saturating_add((255 - r) / 2),
        g.saturating_add((255 - g) / 2),
        b.saturating_add((255 - b) / 2),
    )
}

const TARGET_USAGE_MAX: f64 = 1.5;

/// Returns a color representing how close usage is to the target (blue to red).
pub fn target_usage_color(usage: f64) -> Color {
    let t = usage.clamp(0.0, TARGET_USAGE_MAX);
    if t <= 1.0 {
        lerp_rgb((0, 100, 255), (255, 255, 0), t)
    } else {
        lerp_rgb((255, 255, 0), (255, 0, 0), (t - 1.0) / (TARGET_USAGE_MAX - 1.0))
    }
}

pub(super) const fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f64) -> Color {
    Color::Rgb(
        (a.0 as f64 + (b.0 as f64 - a.0 as f64) * t) as u8,
        (a.1 as f64 + (b.1 as f64 - a.1 as f64) * t) as u8,
        (a.2 as f64 + (b.2 as f64 - a.2 as f64) * t) as u8,
    )
}

const FLASHBLOCK_TARGET_MS: i64 = 200;
const FLASHBLOCK_TOLERANCE_MS: i64 = 50;

/// Returns a color indicating how close a time delta is to the 200ms target.
pub fn time_diff_color(ms: i64) -> Color {
    let target = FLASHBLOCK_TARGET_MS;
    let tol = FLASHBLOCK_TOLERANCE_MS;
    if (target - tol..=target + tol).contains(&ms) {
        Color::Green
    } else if (target - 2 * tol..target - tol).contains(&ms) {
        Color::Blue
    } else if ms < target - 2 * tol {
        Color::Magenta
    } else if (target + tol..target + 2 * tol).contains(&ms) {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Formats a Unix timestamp (seconds since epoch) as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Falls back to the raw seconds string when the timestamp is out of range.
pub fn format_unix_timestamp(secs: u64) -> String {
    i64::try_from(secs)
        .ok()
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .map_or_else(|| secs.to_string(), |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}
