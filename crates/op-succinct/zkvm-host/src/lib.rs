use anyhow::Result;
use std::{process::Command, time::Duration};

use kona_host::HostCli;
use log::error;
use revm::{
    precompile::Precompiles,
    primitives::{Address, Bytes, Precompile},
};

mod stats;
pub use stats::{BnStats, ExecutionStats};

pub mod utils;

/// Run the native host with a timeout. Use a binary to execute the native host, as opposed to
/// spawning a new thread in the same process due to the static cursors employed by the host.
pub async fn run_native_host(
    host_cli: &HostCli,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .expect("Failed to get cargo metadata");
    let target_dir = metadata.target_directory.join("release");
    let args = convert_host_cli_to_args(host_cli);
    let result = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(target_dir.join("native_host_runner"))
            .args(&args)
            .env("RUST_LOG", "info")
            .spawn()?
            .wait(),
    )
    .await;

    match result {
        Ok(status) => Ok(status?),
        Err(_) => {
            error!(
                "Native host runner process timed out after {} seconds",
                timeout.as_secs()
            );
            Command::new("pkill")
                .arg("-f")
                .arg("native_host_runner")
                .output()
                .expect("Failed to kill native_host_runner");
            Err(anyhow::anyhow!(
                "Native host runner process timed out after {} seconds",
                timeout.as_secs()
            ))
        }
    }
}

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
    args
}

/// This precompile hook substitutes the precompile with a custom one that can stub out the logic
/// for specific operations that we don't have precompiles for. Used in `create_hook_precompile`.
pub fn precompile_hook(_env: sp1_sdk::HookEnv, buf: &[u8]) -> Vec<Vec<u8>> {
    let addr: Address = buf[0..20].try_into().unwrap();
    let gas_limit = u64::from_le_bytes(buf[20..28].try_into().unwrap());
    let input: Bytes = buf[28..].to_vec().into();
    println!("[HOOK] Precompile addr {} called.", addr);

    // Note: Fetch the latest precompiles because the gas costs are different from older versions.
    // Otherwise, the hooked precompiles will fail.
    let precompiles = Precompiles::latest();

    let precompile = precompiles.inner().get(&addr).unwrap();
    let result = match precompile {
        Precompile::Standard(precompile) => precompile(&input, gas_limit),
        _ => panic!("Annotated precompile must be a standard precompile."),
    };

    let mut serialized_vec = vec![];
    match result {
        Ok(result) => {
            serialized_vec.push(0);
            serialized_vec.extend_from_slice(&result.gas_used.to_le_bytes());
            serialized_vec.extend_from_slice(&result.bytes);
        }
        Err(err) => {
            serialized_vec.push(1);
            match err {
                revm::precompile::PrecompileErrors::Error(_) => {
                    serialized_vec.push(0);
                }
                revm::precompile::PrecompileErrors::Fatal { .. } => {
                    serialized_vec.push(1);
                }
            }
        }
    }
    vec![serialized_vec]
}
