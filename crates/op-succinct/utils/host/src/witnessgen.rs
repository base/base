use anyhow::Result;
use std::time::Duration;
use tokio::time::timeout;

use kona_host::HostCli;

/// Convert the HostCli to a vector of arguments that can be passed to a command.
pub fn convert_host_cli_to_args(host_cli: &HostCli) -> Vec<String> {
    let mut args = vec![
        format!("--l1-head={}", host_cli.l1_head),
        format!("--l2-head={}", host_cli.l2_head),
        format!("--l2-output-root={}", host_cli.l2_output_root),
        format!("--l2-claim={}", host_cli.l2_claim),
        format!("--l2-block-number={}", host_cli.l2_block_number),
        format!("--l2-chain-id={}", host_cli.l2_chain_id),
    ];
    // The verbosity should be passed as -v, -vv, -vvv, etc.
    if host_cli.v > 0 {
        args.push(format!("-{}", "v".repeat(host_cli.v as usize)));
    }

    if let Some(addr) = &host_cli.l2_node_address {
        args.push("--l2-node-address".to_string());
        args.push(addr.to_string());
    }
    if let Some(addr) = &host_cli.l1_node_address {
        args.push("--l1-node-address".to_string());
        args.push(addr.to_string());
    }
    if let Some(addr) = &host_cli.l1_beacon_address {
        args.push("--l1-beacon-address".to_string());
        args.push(addr.to_string());
    }
    if let Some(dir) = &host_cli.data_dir {
        args.push("--data-dir".to_string());
        args.push(dir.to_string_lossy().into_owned());
    }
    if let Some(exec) = &host_cli.exec {
        args.push("--exec".to_string());
        args.push(exec.to_string());
    }
    if host_cli.server {
        args.push("--server".to_string());
    }
    if let Some(rollup_config_path) = &host_cli.rollup_config_path {
        args.push("--rollup-config-path".to_string());
        args.push(rollup_config_path.to_string_lossy().into_owned());
    }
    args
}

/// Default timeout for witness generation.
pub const WITNESSGEN_TIMEOUT: Duration = Duration::from_secs(1200);

struct WitnessGenProcess {
    child: tokio::process::Child,
    exec: String,
}

/// Stateful executor for witness generation. Useful for executing several witness generation
/// processes in parallel.
pub struct WitnessGenExecutor {
    ongoing_processes: Vec<WitnessGenProcess>,
    timeout: Duration,
}

impl Default for WitnessGenExecutor {
    fn default() -> Self {
        Self::new(WITNESSGEN_TIMEOUT)
    }
}

impl WitnessGenExecutor {
    pub fn new(timeout: Duration) -> Self {
        Self { ongoing_processes: Vec::new(), timeout }
    }

    /// Spawn a witness generation process for the given host CLI, and adds it to the list of
    /// ongoing processes.
    pub async fn spawn_witnessgen(&mut self, host_cli: &HostCli) -> Result<()> {
        let metadata =
            cargo_metadata::MetadataCommand::new().exec().expect("Failed to get cargo metadata");
        let target_dir =
            metadata.target_directory.join("native_host_runner/release/native_host_runner");
        let args = convert_host_cli_to_args(host_cli);

        // Run the native host runner.
        let child =
            tokio::process::Command::new(target_dir).args(&args).env("RUST_LOG", "info").spawn()?;
        self.ongoing_processes
            .push(WitnessGenProcess { child, exec: host_cli.exec.clone().unwrap() });
        Ok(())
    }

    /// Wait for all ongoing witness generation processes to complete. If any process fails,
    /// kill all ongoing processes and return an error.
    pub async fn flush(&mut self) -> Result<()> {
        let binary_name = self.ongoing_processes[0].exec.split('/').last().unwrap().to_string();

        // TODO: If any process fails or a Ctrl+C is received, kill all ongoing processes. This is
        // quite involved, as the behavior differs between Unix and Windows. When using
        // Ctrl+C handler, you also need to be careful to restore the original behavior
        // after your custom behavior, otherwise you won't be able to terminate "normally".

        // Wait for all processes to complete.
        let result = self.wait_for_processes().await;
        let any_failed = result.is_err();

        if any_failed {
            self.kill_all(binary_name).await?;
            Err(anyhow::anyhow!("One or more child processes failed or timed out"))
        } else {
            Ok(())
        }
    }

    /// Wait for all ongoing witness generation processes to complete. If any process fails, return
    /// an error.
    async fn wait_for_processes(&mut self) -> Result<()> {
        for child in &mut self.ongoing_processes {
            match timeout(self.timeout, child.child.wait()).await {
                Ok(Ok(status)) if !status.success() => {
                    return Err(anyhow::anyhow!("Child process exited with non-zero status"));
                }
                Ok(Err(e)) => {
                    eprintln!("Child process error: {}", e);
                    return Err(anyhow::anyhow!("Child process error"));
                }
                Err(_) => {
                    eprintln!("Child process timed out");
                    return Err(anyhow::anyhow!("Child process timed out"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Kill all ongoing "native client" processes and the associated spawned witness gen
    /// programs. Specifically, whenever witness generation is spawned, there is a "native
    /// client" process that spawns a "witness gen" program. Just killing the "native client"
    /// process will not kill the "witness gen" program, so we need to explicitly kill the
    /// "witness gen" program as well.
    async fn kill_all(&mut self, binary_name: String) -> Result<()> {
        // Kill the "native client" processes.
        for mut child in self.ongoing_processes.drain(..) {
            child.child.kill().await?;
        }

        // Kill the spawned witness gen program.
        std::process::Command::new("pkill").arg("-f").arg(binary_name).output()?;
        Ok(())
    }
}
