//! Reusable confirmation helper for destructive CLI commands.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};

/// Prints `prompt` and reads a `y`/`yes` answer from stdin.
pub(crate) fn confirm(prompt: &str, skip: bool) -> Result<bool> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    confirm_with_io(prompt, skip, &mut stdin.lock(), &mut stdout.lock())
}

fn confirm_with_io<R: BufRead, W: Write>(
    prompt: &str,
    skip: bool,
    reader: &mut R,
    writer: &mut W,
) -> Result<bool> {
    if skip {
        return Ok(true);
    }

    write!(writer, "{prompt}").context("writing confirmation prompt")?;
    writer.flush().context("flushing confirmation prompt")?;

    let mut answer = String::new();
    reader.read_line(&mut answer).context("reading confirmation answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::confirm_with_io;

    #[test]
    fn confirm_accepts_y_and_yes() {
        for input in ["y\n", "Y\n", "yes\n", "YES\n"] {
            let mut reader = Cursor::new(input.as_bytes());
            let mut writer = Vec::new();

            assert!(confirm_with_io("Proceed? [y/N] ", false, &mut reader, &mut writer).unwrap());
        }
    }

    #[test]
    fn confirm_defaults_to_no() {
        for input in ["\n", "n\n", "anything\n"] {
            let mut reader = Cursor::new(input.as_bytes());
            let mut writer = Vec::new();

            assert!(!confirm_with_io("Proceed? [y/N] ", false, &mut reader, &mut writer).unwrap());
        }
    }

    #[test]
    fn confirm_skip_does_not_prompt() {
        let mut reader = Cursor::new(b"".as_slice());
        let mut writer = Vec::new();

        assert!(confirm_with_io("Proceed? [y/N] ", true, &mut reader, &mut writer).unwrap());
        assert!(writer.is_empty());
    }
}
