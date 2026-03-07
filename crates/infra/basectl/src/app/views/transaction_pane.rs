use std::ops::Range;

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};
use tokio::sync::mpsc;

use crate::{
    commands::common::{COLOR_ACTIVE_BORDER, COLOR_ROW_SELECTED},
    rpc::TxSummary,
    tui::Toast,
};

/// Reusable transaction list pane that can be embedded in any block-listing view.
///
/// Fetches and displays the transactions for a single L2 block, with keyboard
/// navigation, clipboard copy support, and focused/unfocused rendering states.
#[derive(Debug)]
pub(crate) struct TransactionPane {
    /// The block number whose transactions are displayed.
    pub block_number: u64,
    /// Display title (e.g. "Block 123" or "Flashblock `123::2`").
    title_prefix: String,
    transactions: Vec<TxSummary>,
    table_state: TableState,
    loading: bool,
    rx: Option<mpsc::Receiver<Vec<TxSummary>>>,
    /// Optional range to slice the fetched transactions (for flashblock-specific views).
    tx_range: Option<Range<usize>>,
    /// Block explorer base URL for opening transactions in a browser (e.g.
    /// `https://basescan.org`).
    explorer_base_url: Option<String>,
}

impl TransactionPane {
    /// Creates a new pane that immediately begins fetching transactions for `block_number`.
    ///
    /// If `tx_range` is provided, only the specified slice of the block's transactions
    /// will be displayed (used for flashblock-specific views).
    pub(crate) fn new(
        block_number: u64,
        title_prefix: String,
        l2_rpc: &str,
        tx_range: Option<Range<usize>>,
        explorer_base_url: Option<&str>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(1);
        let rpc = l2_rpc.to_string();
        tokio::spawn(crate::rpc::fetch_block_transactions(rpc, block_number, tx));

        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            block_number,
            title_prefix,
            transactions: Vec::new(),
            table_state,
            loading: true,
            rx: Some(rx),
            tx_range,

            explorer_base_url: explorer_base_url.map(String::from),
        }
    }

    /// Creates a pane with pre-decoded transaction data.
    pub(crate) fn with_data(
        block_number: u64,
        title_prefix: String,
        transactions: Vec<TxSummary>,
        explorer_base_url: Option<&str>,
    ) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            block_number,
            title_prefix,
            transactions,
            table_state,
            loading: false,
            rx: None,
            tx_range: None,

            explorer_base_url: explorer_base_url.map(String::from),
        }
    }

    /// Creates a pane for a full block using an authoritative RPC fetch.
    ///
    /// DA block inspection intentionally avoids relying on streamed flashblock
    /// caches, which may be incomplete after reconnects or message gaps.
    pub(crate) fn for_block(
        block_number: u64,
        l2_rpc: &str,
        explorer_base_url: Option<&str>,
    ) -> Self {
        // DA block inspection should default to authoritative RPC data.
        // Streamed flashblock caches can be incomplete during reconnects or gaps.
        Self::new(block_number, format!("Block {block_number}"), l2_rpc, None, explorer_base_url)
    }

    /// Polls background fetch channels for results.
    pub(crate) fn poll(&mut self) {
        if let Some(ref mut rx) = self.rx
            && let Ok(txns) = rx.try_recv()
        {
            self.transactions = match &self.tx_range {
                Some(range) => txns.into_iter().skip(range.start).take(range.len()).collect(),
                None => txns,
            };
            self.loading = false;
            self.rx = None;
        }
    }

    /// Handles keyboard input directed at this pane.
    ///
    /// Returns `true` when the pane should be closed (Esc was pressed).
    /// The `toast_tx` callback is invoked to push toast notifications (e.g. after
    /// copying a transaction hash to the clipboard).
    pub(crate) fn handle_key(&mut self, key: KeyEvent, toast_tx: &mut impl FnMut(Toast)) -> bool {
        let len = self.transactions.len();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,

            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(selected) = self.table_state.selected()
                    && selected + 1 < len
                {
                    self.table_state.select(Some(selected + 1));
                }
            }

            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(selected) = self.table_state.selected()
                    && selected > 0
                {
                    self.table_state.select(Some(selected - 1));
                }
            }

            KeyCode::Home | KeyCode::Char('g') => {
                if len > 0 {
                    self.table_state.select(Some(0));
                }
            }

            KeyCode::End | KeyCode::Char('G') => {
                if len > 0 {
                    self.table_state.select(Some(len - 1));
                }
            }

            KeyCode::PageUp => {
                if let Some(selected) = self.table_state.selected() {
                    self.table_state.select(Some(selected.saturating_sub(10)));
                }
            }

            KeyCode::PageDown => {
                if let Some(selected) = self.table_state.selected() {
                    let new_pos = (selected + 10).min(len.saturating_sub(1));
                    self.table_state.select(Some(new_pos));
                }
            }

            KeyCode::Enter => {
                if let Some(idx) = self.table_state.selected()
                    && let Some(tx_summary) = self.transactions.get(idx)
                    && let Some(ref base_url) = self.explorer_base_url
                {
                    let url = format!("{base_url}/tx/{:#x}", tx_summary.hash);
                    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                    match std::process::Command::new(cmd).arg(&url).spawn() {
                        Ok(mut child) => {
                            std::thread::spawn(move || {
                                let _ = child.wait();
                            });
                            toast_tx(Toast::info(format!("Opening {url}")));
                        }
                        Err(e) => {
                            toast_tx(Toast::warning(format!("Failed to open browser: {e}")));
                        }
                    }
                }
            }

            KeyCode::Char('y') => {
                if let Some(idx) = self.table_state.selected()
                    && let Some(tx_summary) = self.transactions.get(idx)
                    && let Ok(mut clipboard) = Clipboard::new()
                {
                    let hash_str = format!("{:#x}", tx_summary.hash);
                    if clipboard.set_text(&hash_str).is_ok() {
                        toast_tx(Toast::info(format!("Copied {hash_str}")));
                    }
                }
            }

            _ => {}
        }

        false
    }

    /// Renders the transaction pane into the given area.
    ///
    /// When `is_focused` is true the border is highlighted and the selected row
    /// receives a distinct background color.
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, is_focused: bool) {
        let border_color = if is_focused { COLOR_ACTIVE_BORDER } else { Color::DarkGray };

        let title = if self.loading {
            format!(" {} - Loading... ", self.title_prefix)
        } else {
            let base_fee_str = self
                .transactions
                .first()
                .and_then(|tx| tx.base_fee_per_gas)
                .map(|fee| format!(" | Base: {} Mwei", fee as f64 / 1_000_000.0))
                .unwrap_or_default();
            format!(" {} - {} txns{} ", self.title_prefix, self.transactions.len(), base_fee_str,)
        };

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.loading {
            let para = Paragraph::new("Fetching transactions...")
                .style(Style::default().fg(Color::Yellow));
            frame.render_widget(para, inner);
            return;
        }

        if self.transactions.is_empty() {
            let para =
                Paragraph::new("No transactions").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(para, inner);
            return;
        }

        let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let header = Row::new(vec![
            Cell::from("Tx Hash").style(header_style),
            Cell::from("From").style(header_style),
            Cell::from("To").style(header_style),
            Cell::from("Pri. Fee (Mwei)").style(header_style),
        ]);

        let selected_row = self.table_state.selected();

        // Use proportional widths so hex columns expand with available space.
        let widths = [
            Constraint::Fill(3),    // Tx Hash (widest)
            Constraint::Fill(2),    // From
            Constraint::Fill(2),    // To
            Constraint::Length(15), // Pri. Fee (fixed, short values)
        ];

        // Pre-compute actual column widths so we can truncate intelligently.
        let col_rects = Layout::horizontal(widths).split(inner);
        let hash_w = col_rects[0].width as usize;
        let from_w = col_rects[1].width as usize;
        let to_w = col_rects[2].width as usize;

        let rows: Vec<Row<'_>> = self
            .transactions
            .iter()
            .enumerate()
            .map(|(idx, tx_summary)| {
                let is_selected = is_focused && selected_row == Some(idx);

                let style = if is_selected {
                    Style::default().fg(Color::White).bg(COLOR_ROW_SELECTED)
                } else {
                    Style::default().fg(Color::White)
                };

                let to_str = tx_summary
                    .to
                    .map(|a| truncate_hex(&format!("{a:#x}"), to_w))
                    .unwrap_or_else(|| "Create".to_string());

                Row::new(vec![
                    Cell::from(truncate_hex(&format!("{:#x}", tx_summary.hash), hash_w)),
                    Cell::from(truncate_hex(&format!("{:#x}", tx_summary.from), from_w)),
                    Cell::from(to_str),
                    Cell::from(format_mwei(tx_summary.max_priority_fee_per_gas)),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(rows, widths).header(header);
        frame.render_stateful_widget(table, inner, &mut self.table_state);
    }
}

/// Truncates a hex string to fit within `max_width` characters, keeping the
/// `0x` prefix and last 4 chars visible with an ellipsis in the middle.
/// Shows the full string when there's enough space.
fn truncate_hex(hex: &str, max_width: usize) -> String {
    if hex.len() <= max_width {
        return hex.to_string();
    }
    // Need at least "0x" + 2 hex + "…" + 4 hex = 9 chars for meaningful truncation
    if max_width < 9 {
        return hex[..max_width].to_string();
    }
    let suffix_len = 4;
    let prefix_len = max_width - suffix_len - 1; // 1 for ellipsis char
    let prefix = &hex[..prefix_len];
    let suffix = &hex[hex.len() - suffix_len..];
    format!("{prefix}\u{2026}{suffix}")
}

/// Formats a wei-denominated fee value as Mwei with 2 decimal places.
fn format_mwei(wei: Option<u128>) -> String {
    match wei {
        None => "-".to_string(),
        Some(0) => "0".to_string(),
        Some(w) => {
            let mwei_whole = w / 1_000_000;
            let mwei_frac = (w % 1_000_000) / 10_000; // 2 decimal places
            format!("{mwei_whole}.{mwei_frac:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_hex_fits() {
        let hex = "0x1234567890ab";
        assert_eq!(truncate_hex(hex, 20), hex);
    }

    #[test]
    fn test_truncate_hex_needs_truncation() {
        let hex = "0x1234567890abcdef1234567890abcdef12345678";
        let result = truncate_hex(hex, 14);
        // Display width is 14: 9 prefix + 1 ellipsis + 4 suffix
        // Byte length is 16 because the ellipsis char is 3 bytes in UTF-8
        assert_eq!(result.chars().count(), 14);
        assert!(result.starts_with("0x1234567"));
        assert!(result.ends_with("5678"));
        assert!(result.contains('\u{2026}'));
    }

    #[test]
    fn test_truncate_hex_expands_with_width() {
        let hex = "0x1234567890abcdef1234567890abcdef12345678";
        let narrow = truncate_hex(hex, 14);
        let wide = truncate_hex(hex, 24);
        assert!(wide.chars().count() > narrow.chars().count());
        // width 24: prefix_len = 24 - 4 - 1 = 19 → "0x1234567890abcdef1"
        assert!(wide.starts_with("0x1234567890abcdef1"));
    }

    #[test]
    fn test_format_mwei_none() {
        assert_eq!(format_mwei(None), "-");
    }

    #[test]
    fn test_format_mwei_zero() {
        assert_eq!(format_mwei(Some(0)), "0");
    }

    #[test]
    fn test_format_mwei_whole() {
        // 5 Mwei = 5_000_000 wei
        assert_eq!(format_mwei(Some(5_000_000)), "5.00");
        // 10 Mwei = 10_000_000 wei
        assert_eq!(format_mwei(Some(10_000_000)), "10.00");
    }

    #[test]
    fn test_format_mwei_fractional() {
        // 2.57 Mwei = 2_570_000 wei
        assert_eq!(format_mwei(Some(2_570_000)), "2.57");
        // 0.50 Mwei = 500_000 wei
        assert_eq!(format_mwei(Some(500_000)), "0.50");
    }

    #[test]
    fn test_format_mwei_sub_mwei() {
        // 1 wei → 0.00 Mwei
        assert_eq!(format_mwei(Some(1)), "0.00");
        // 100_000 wei → 0.10 Mwei
        assert_eq!(format_mwei(Some(100_000)), "0.10");
    }
}
