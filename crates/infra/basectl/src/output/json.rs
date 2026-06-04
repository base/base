//! JSON output for non-TUI commands.

use std::io::{self, Write};

use anyhow::Result;
use serde::Serialize;

/// JSON writer for non-TUI command output.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonOutput;

impl JsonOutput {
    /// Pretty-prints `value` as JSON to stdout, terminating with a newline.
    pub fn print<T: Serialize>(value: &T) -> Result<()> {
        let mut stdout = io::stdout().lock();
        Self::write(&mut stdout, value)?;
        Ok(())
    }

    /// Pretty-prints `value` as JSON to `writer`, terminating with a newline.
    pub fn write<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
        serde_json::to_writer_pretty(&mut *writer, value)?;
        writer.write_all(b"\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::JsonOutput;

    #[test]
    fn write_emits_pretty_json_with_trailing_newline() {
        let mut buf = Vec::new();
        JsonOutput::write(&mut buf, &json!({"k": 1})).unwrap();
        let rendered = String::from_utf8(buf).unwrap();

        assert_eq!(rendered, "{\n  \"k\": 1\n}\n");
    }
}
