//! Child process lifecycle management with graceful SIGINT shutdown.

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::error::BenchmarkError;

/// A running child process with stdout/stderr redirected to files.
pub struct ProcessHandle {
    binary: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdout_file: File,
    stderr_file: File,
    child: Option<Child>,
}

impl ProcessHandle {
    /// Create a new handle. Call [`start`](Self::start) to spawn the process.
    pub fn new(
        binary: PathBuf,
        args: Vec<String>,
        env: Vec<(String, String)>,
        stdout_file: File,
        stderr_file: File,
    ) -> Self {
        Self { binary, args, env, stdout_file, stderr_file, child: None }
    }

    /// Spawn the child process.
    pub async fn start(&mut self) -> Result<(), BenchmarkError> {
        let stdout: Stdio = self.stdout_file.try_clone()?.into();
        let stderr: Stdio = self.stderr_file.try_clone()?.into();

        let mut cmd = Command::new(&self.binary);
        cmd.args(&self.args).stdout(stdout).stderr(stderr).kill_on_drop(false);
        for (key, val) in &self.env {
            cmd.env(key, val);
        }

        let child = cmd.spawn().map_err(|e| {
            BenchmarkError::Client(format!(
                "failed to spawn {}: {e}",
                self.binary.display()
            ))
        })?;

        let name = self.binary.file_name().unwrap_or_default().to_string_lossy();
        info!(pid = %child.id().unwrap_or(0), binary = %name, "process started");

        self.child = Some(child);
        Ok(())
    }

    /// Send SIGINT and wait up to 5 seconds; escalate to SIGKILL if needed.
    pub async fn stop(&mut self) -> Result<(), BenchmarkError> {
        let Some(child) = self.child.as_mut() else { return Ok(()) };
        let name = self.binary.file_name().unwrap_or_default().to_string_lossy().to_string();

        if let Some(pid) = child.id() {
            if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGINT) {
                warn!(error = %e, binary = %name, "failed to send SIGINT");
            }
        }

        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(status)) => {
                info!(binary = %name, status = %status, "process exited after SIGINT");
            }
            Ok(Err(e)) => {
                warn!(error = %e, binary = %name, "error waiting for process after SIGINT");
            }
            Err(_) => {
                warn!(binary = %name, "process did not exit within 5s, sending SIGKILL");
                let _ = child.kill().await;
            }
        }

        self.child = None;
        Ok(())
    }

    /// Wait for the process to exit. Returns an error if it exits with a
    /// non-zero code or a signal.
    pub async fn wait(&mut self) -> Result<(), BenchmarkError> {
        let Some(child) = self.child.as_mut() else { return Ok(()) };
        let name =
            self.binary.file_name().unwrap_or_default().to_string_lossy().to_string();

        let status = child.wait().await.map_err(|e| {
            BenchmarkError::Client(format!("failed to wait for {name}: {e}"))
        })?;

        if status.success() {
            return Ok(());
        }

        let exit_code = status.code().or_else(|| status.signal().map(|s| -(s as i32)));
        Err(BenchmarkError::ProcessCrash { binary: name, exit_code })
    }

    /// Returns the PID of the running process, if any.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }
}
