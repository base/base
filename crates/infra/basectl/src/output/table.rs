//! Aligned key-value table renderer for non-TUI command output.

use std::io::{self, Write};

/// Aligned key-value rows for pretty CLI output.
#[derive(Debug, Default, Clone)]
pub struct KeyValueTable {
    rows: Vec<(String, String)>,
}

impl KeyValueTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a key-value row.
    pub fn row(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.rows.push((key.into(), value.into()));
        self
    }

    /// Writes the rows to `writer`, padding keys to the longest key width.
    pub fn render<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let width = self.rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (key, value) in &self.rows {
            writeln!(writer, "{key:<width$}  {value}")?;
        }
        Ok(())
    }

    /// Writes the rows to stdout, padding keys to the longest key width.
    pub fn print(&self) -> io::Result<()> {
        self.render(&mut io::stdout().lock())
    }
}

#[cfg(test)]
mod tests {
    use super::KeyValueTable;

    #[test]
    fn render_pads_keys_to_longest_width() {
        let mut table = KeyValueTable::new();
        table.row("number", "123").row("hash", "0xabc").row("transaction count", "5");

        let mut buf = Vec::new();
        table.render(&mut buf).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        assert_eq!(
            rendered,
            "number             123\n\
             hash               0xabc\n\
             transaction count  5\n",
        );
    }

    #[test]
    fn render_empty_table_writes_nothing() {
        let table = KeyValueTable::new();
        let mut buf = Vec::new();
        table.render(&mut buf).unwrap();
        assert!(buf.is_empty());
    }
}
