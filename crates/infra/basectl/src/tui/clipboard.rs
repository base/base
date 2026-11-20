use std::{
    env,
    io::{self, IsTerminal, Write},
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Copies plain text to the user's clipboard without native GUI bindings.
///
/// For local sessions, tries the platform clipboard utility (`pbcopy` on macOS,
/// `wl-copy` / `xclip` / `xsel` on Linux, `clip` on Windows). Over SSH, or when
/// no local utility is available, emits an OSC 52 escape sequence to stdout so
/// supporting terminals (including remote SSH and tmux sessions) update the
/// clipboard directly.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clipboard;

impl Clipboard {
    /// Copies `text` to the system clipboard.
    pub(crate) fn copy(text: &str) -> io::Result<()> {
        if !Self::is_ssh() && Self::copy_via_command(text) {
            return Ok(());
        }
        Self::copy_via_osc52(text)
    }

    fn is_ssh() -> bool {
        env::var_os("SSH_TTY").is_some() || env::var_os("SSH_CONNECTION").is_some()
    }

    fn copy_via_command(text: &str) -> bool {
        let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
            &[("pbcopy", &[])]
        } else if cfg!(target_os = "windows") {
            &[("clip", &[])]
        } else {
            &[
                ("wl-copy", &[]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["--clipboard", "--input"]),
            ]
        };

        candidates.iter().any(|&(program, args)| Self::run_command(program, args, text))
    }

    fn run_command(program: &str, args: &[&str], text: &str) -> bool {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };

        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.wait();
            return false;
        };

        let write_ok = stdin.write_all(text.as_bytes()).is_ok();
        drop(stdin);
        let wait_ok = child.wait().is_ok_and(|status| status.success());
        write_ok && wait_ok
    }

    fn copy_via_osc52(text: &str) -> io::Result<()> {
        let mut stdout = io::stdout();
        if !stdout.is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdout is not a terminal; cannot emit OSC 52 clipboard sequence",
            ));
        }

        let seq = Self::format_osc52(text, env::var_os("TMUX").is_some());
        stdout.write_all(seq.as_bytes())?;
        stdout.flush()
    }

    fn format_osc52(text: &str, in_tmux: bool) -> String {
        let encoded = STANDARD.encode(text.as_bytes());
        if in_tmux {
            format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
        } else {
            format!("\x1b]52;c;{encoded}\x07")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Clipboard;

    #[test]
    fn formats_plain_osc52_sequence() {
        assert_eq!(Clipboard::format_osc52("0xdeadbeef", false), "\x1b]52;c;MHhkZWFkYmVlZg==\x07");
    }

    #[test]
    fn formats_tmux_passthrough_osc52_sequence() {
        assert_eq!(
            Clipboard::format_osc52("12345", true),
            "\x1bPtmux;\x1b\x1b]52;c;MTIzNDU=\x07\x1b\\"
        );
    }
}
