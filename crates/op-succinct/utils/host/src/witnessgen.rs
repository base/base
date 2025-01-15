use anyhow::Result;
use std::{path::PathBuf, time::Duration};
use sysinfo::System;

use kona_host::HostCli;

use crate::fetcher::RunContext;

/// Convert the HostCli to a vector of arguments that can be passed to a command.
pub fn convert_host_cli_to_args(host_cli: &HostCli) -> Vec<String> {
    let mut args = vec![
        format!("--l1-head={}", host_cli.l1_head),
        format!("--agreed-l2-head-hash={}", host_cli.agreed_l2_head_hash),
        format!("--agreed-l2-output-root={}", host_cli.agreed_l2_output_root),
        format!(
            "--claimed-l2-output-root={}",
            host_cli.claimed_l2_output_root
        ),
        format!(
            "--claimed-l2-block-number={}",
            host_cli.claimed_l2_block_number
        ),
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

/// Set to 12 hours to allow for long witness generation.
pub const WITNESSGEN_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 12);

struct WitnessGenProcess {
    child: tokio::process::Child,
    host_cli: HostCli,
}

/// Stateful executor for witness generation. Useful for executing several witness generation
/// processes in parallel.
pub struct WitnessGenExecutor {
    ongoing_processes: Vec<WitnessGenProcess>,
    timeout: Duration,
    run_context: RunContext,
}

impl Default for WitnessGenExecutor {
    fn default() -> Self {
        Self::new(WITNESSGEN_TIMEOUT, RunContext::Dev)
    }
}

impl WitnessGenExecutor {
    pub fn new(timeout: Duration, run_context: RunContext) -> Self {
        Self {
            ongoing_processes: Vec::new(),
            timeout,
            run_context,
        }
    }

    /// Spawn a witness generation process for the given host CLI, and adds it to the list of
    /// ongoing processes.
    pub async fn spawn_witnessgen(&mut self, host_cli: &HostCli) -> Result<()> {
        let target_dir = match self.run_context {
            RunContext::Dev => {
                let metadata = cargo_metadata::MetadataCommand::new()
                    .exec()
                    .expect("Failed to get cargo metadata");
                metadata
                    .target_directory
                    .join("native_host_runner/release/native_host_runner")
                    .into()
            }
            RunContext::Docker => PathBuf::from("/usr/local/bin/native_host_runner"),
        };
        let args = convert_host_cli_to_args(host_cli);

        // Run the native host runner.
        let child = tokio::process::Command::new(target_dir)
            .args(&args)
            .env("RUST_LOG", "info")
            .spawn()?;
        self.ongoing_processes.push(WitnessGenProcess {
            child,
            host_cli: host_cli.clone(),
        });
        Ok(())
    }

    /// Wait for all ongoing witness generation processes to complete. If any process fails,
    /// kill all ongoing processes and return an error.
    pub async fn flush(&mut self) -> Result<()> {
        // FIXME: If any process fails or a Ctrl+C is received, kill all ongoing processes. This is
        // quite involved, as the behavior differs between Unix and Windows. When using
        // Ctrl+C handler, you also need to be careful to restore the original behavior
        // after your custom behavior, otherwise you won't be able to terminate "normally".

        // Wait for all processes to complete.

        if let Some(err) = self.wait_for_processes().await.err() {
            self.kill_all().await?;
            Err(anyhow::anyhow!(
                "Killed all witness generation processes because one failed. Error: {}",
                err
            ))
        } else {
            Ok(())
        }
    }

    /// Wait for all ongoing witness generation processes to complete. If any process fails, return
    /// an error.
    async fn wait_for_processes(&mut self) -> Result<()> {
        for child in &mut self.ongoing_processes {
            tokio::select! {
                result = child.child.wait() => {
                    match result {
                        Ok(status) if !status.success() => {
                            return Err(anyhow::anyhow!(
                                "Witness generation process for end block {} failed.",
                                child.host_cli.claimed_l2_block_number
                            ));
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "Failed to get witness generation process status for end block {}: {}",
                                child.host_cli.claimed_l2_block_number, e
                            ));
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(self.timeout) => {
                    return Err(anyhow::anyhow!(
                        "Witness generation process for end block {} timed out.",
                        child.host_cli.claimed_l2_block_number
                    ));
                }
            }
        }
        Ok(())
    }

    /// Kill all ongoing "native client" processes and the associated spawned witness gen
    /// programs. Specifically, whenever witness generation is spawned, there is a "native
    /// client" process that spawns a "witness gen" program. Just killing the "native client"
    /// process will not kill the "witness gen" program, so we need to explicitly kill the
    /// "witness gen" program as well.
    async fn kill_all(&mut self) -> Result<()> {
        let mut sys = System::new();
        sys.refresh_all();

        // Kill the "native client" processes, and the associated spawned native program process from start_server_and_native_client.
        for mut child in self.ongoing_processes.drain(..) {
            if let Some(pid) = child.child.id() {
                // Kill all child processes
                for process in sys.processes().values() {
                    if let Some(parent_pid) = process.parent() {
                        if parent_pid.as_u32() == pid {
                            process.kill();
                        }
                    }
                }
                // Kill the parent process.
                child.child.kill().await?;
            }
        }

        Ok(())
    }
}

/// Concurrently run the native data generation process for each split range.
pub async fn run_native_data_generation(host_clis: &[HostCli]) {
    const CONCURRENT_NATIVE_HOST_RUNNERS: usize = 5;

    // Split the entire range into chunks of size CONCURRENT_NATIVE_HOST_RUNNERS and process chunks
    // serially. Generate witnesses within each chunk in parallel. This prevents the RPC from
    // being overloaded with too many concurrent requests, while also improving witness generation
    // throughput.
    for chunk in host_clis.chunks(CONCURRENT_NATIVE_HOST_RUNNERS) {
        let mut witnessgen_executor = WitnessGenExecutor::default();

        for host_cli in chunk {
            witnessgen_executor
                .spawn_witnessgen(host_cli)
                .await
                .expect("Failed to spawn witness generation process");
        }

        witnessgen_executor
            .flush()
            .await
            .expect("Failed to generate witnesses");
    }
}
