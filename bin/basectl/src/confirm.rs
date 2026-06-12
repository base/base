//! Reusable confirmation helper for destructive CLI commands.

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};

/// Prints `prompt` and reads a `y`/`yes` answer from stdin.
pub(crate) fn confirm(prompt: &str, skip: bool) -> Result<bool> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    confirm_with_io(prompt, skip, &mut stdin.lock(), &mut stdout.lock())
}

/// Confirms a destructive action and prints `aborted` when declined.
pub(crate) fn confirm_or_abort(prompt: &str, skip: bool) -> Result<bool> {
    let confirmed = confirm(prompt, skip)?;
    if !confirmed {
        println!("aborted");
    }
    Ok(confirmed)
}

/// Confirms a destructive action by requiring an exact typed value.
pub(crate) fn confirm_typed(prompt: &str, expected: &str, skip: bool) -> Result<bool> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    confirm_typed_with_io(prompt, expected, skip, &mut stdin.lock(), &mut stdout.lock())
}

/// Confirms a typed destructive action and prints `aborted` when declined.
pub(crate) fn confirm_typed_or_abort(prompt: &str, expected: &str, skip: bool) -> Result<bool> {
    let confirmed = confirm_typed(prompt, expected, skip)?;
    if !confirmed {
        println!("aborted");
    }
    Ok(confirmed)
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

fn confirm_typed_with_io<R: BufRead, W: Write>(
    prompt: &str,
    expected: &str,
    skip: bool,
    reader: &mut R,
    writer: &mut W,
) -> Result<bool> {
    if skip {
        return Ok(true);
    }

    write!(writer, "{prompt}").context("writing typed confirmation prompt")?;
    writer.flush().context("flushing typed confirmation prompt")?;

    let mut answer = String::new();
    reader.read_line(&mut answer).context("reading typed confirmation answer")?;
    Ok(answer.trim() == expected)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{confirm_typed_with_io, confirm_with_io};

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

    #[test]
    fn confirm_typed_accepts_exact_expected_value() {
        let mut reader = Cursor::new(b"devnet\n".as_slice());
        let mut writer = Vec::new();

        assert!(
            confirm_typed_with_io("Type devnet: ", "devnet", false, &mut reader, &mut writer)
                .unwrap()
        );
    }

    #[test]
    fn confirm_typed_rejects_mismatched_value() {
        let mut reader = Cursor::new(b"mainnet\n".as_slice());
        let mut writer = Vec::new();

        assert!(
            !confirm_typed_with_io("Type devnet: ", "devnet", false, &mut reader, &mut writer)
                .unwrap()
        );
    }

    #[test]
    fn confirm_typed_skip_does_not_prompt() {
        let mut reader = Cursor::new(b"".as_slice());
        let mut writer = Vec::new();

        assert!(
            confirm_typed_with_io("Type devnet: ", "devnet", true, &mut reader, &mut writer)
                .unwrap()
        );
        assert!(writer.is_empty());
    }
}
