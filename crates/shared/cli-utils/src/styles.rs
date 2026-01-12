//! Cli styles for [clap].

use clap::builder::{
    Styles,
    styling::{AnsiColor, Color, Style},
};

/// A wrapper type for CLI styles.
#[derive(Debug, Clone, Copy)]
pub struct CliStyles;

impl CliStyles {
    /// Initialize the CLI styles, returning a [`Styles`] instance.
    pub const fn init() -> Styles {
        clap::builder::Styles::styled()
            .usage(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
            .header(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
            .literal(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
            .invalid(Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Red))))
            .error(Style::new().bold().fg_color(Some(Color::Ansi(AnsiColor::Red))))
            .valid(Style::new().bold().underline().fg_color(Some(Color::Ansi(AnsiColor::Green))))
            .placeholder(Style::new().fg_color(Some(Color::Ansi(AnsiColor::White))))
    }
}
