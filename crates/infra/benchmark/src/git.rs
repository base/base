//! Git metadata capture via `std::process::Command`.

use std::process::Command;

/// Git commit SHA and branch name captured at process startup.
#[derive(Debug, Clone)]
pub struct GitInfo {
    /// Full commit SHA (or `"unknown"` if git is unavailable).
    pub sha: String,
    /// Current branch name (or `"unknown"` for detached HEAD / unavailable).
    pub branch: String,
}

impl GitInfo {
    /// Capture git info from the current working directory.
    ///
    /// Returns `"unknown"` for any field that cannot be determined (e.g. not a
    /// git repo, git not installed).
    pub fn from_cwd() -> Self {
        Self {
            sha: git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string()),
            branch: git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap_or_else(|| "unknown".to_string()),
        }
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_cwd_returns_non_empty_strings() {
        let info = GitInfo::from_cwd();
        assert!(!info.sha.is_empty());
        assert!(!info.branch.is_empty());
    }
}
