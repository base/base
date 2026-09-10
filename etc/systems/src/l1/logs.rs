//! Container log capture for L1 failure diagnostics.

use eyre::Result;
use testcontainers::{ContainerAsync, GenericImage};

/// Tails a container's output so a stalled L1 can be diagnosed from CI test output.
///
/// The L1 clients run in Docker, so their logs never reach the test harness's own output.
/// Without this, a devnet L1 that accepts RPC but never advances is indistinguishable from a
/// slow one.
#[derive(Debug, Clone, Copy)]
pub struct ContainerLogs;

impl ContainerLogs {
    /// Number of trailing lines kept from each stream.
    pub const TAIL_LINES: usize = 80;

    /// Returns the trailing stdout and stderr of a container, labelled by stream.
    pub async fn tail(container: &ContainerAsync<GenericImage>) -> Result<String> {
        let stdout = container.stdout_to_vec().await.unwrap_or_default();
        let stderr = container.stderr_to_vec().await.unwrap_or_default();
        Ok(format!(
            "stdout:\n{}\nstderr:\n{}",
            Self::tail_lines(&stdout),
            Self::tail_lines(&stderr)
        ))
    }

    /// Returns the last [`Self::TAIL_LINES`] lines of a captured stream.
    pub fn tail_lines(raw: &[u8]) -> String {
        let text = String::from_utf8_lossy(raw);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(Self::TAIL_LINES);
        lines[start..].join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::ContainerLogs;

    #[test]
    fn tail_lines_keeps_only_the_trailing_window() {
        let raw = (0..200).map(|line| format!("line {line}")).collect::<Vec<_>>().join("\n");

        let tailed = ContainerLogs::tail_lines(raw.as_bytes());

        assert_eq!(tailed.lines().count(), ContainerLogs::TAIL_LINES);
        assert!(tailed.starts_with("line 120"));
        assert!(tailed.ends_with("line 199"));
    }

    #[test]
    fn tail_lines_keeps_short_output_intact() {
        let tailed = ContainerLogs::tail_lines(b"only line");

        assert_eq!(tailed, "only line");
    }
}
