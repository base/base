#![doc = include_str!("../README.md")]

use std::{
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    path::PathBuf,
    sync::Arc,
};

use alloy_provider::RootProvider;
use base_common_network::Base;
use base_optimism_rpc::OptimismRollupProviderExt;
use base_proof_zk_backend::{L1HeadSource, OpSuccinctWitnessProvider, WitnessParams};
use base_proof_zk_witness::fetcher::{OPSuccinctDataFetcher, RPCConfig};
use clap::Parser;
use eyre::{Result, WrapErr, ensure};
use tempfile::TempDir;
use tracing::info;
use url::Url;

/// `BabyBear` modulus, `2^31 - 2^27 + 1`.
const BABY_BEAR_MODULUS: u64 = 0x7800_0001;

/// Dump a 1-block `OpenVM` range witness from live RPCs.
#[derive(Debug, Clone, Parser)]
#[command(name = "openvm-dump")]
pub struct OpenVmRangeDump {
    /// L1 execution RPC.
    #[arg(long, env = "OPENVM_L1_RPC")]
    pub l1_rpc: Url,
    /// L1 beacon HTTP endpoint.
    #[arg(long, env = "OPENVM_L1_BEACON_RPC")]
    pub l1_beacon_rpc: Url,
    /// L2 execution RPC.
    #[arg(long, env = "OPENVM_L2_RPC")]
    pub l2_rpc: Url,
    /// L2 consensus (op-node) RPC.
    #[arg(long, env = "OPENVM_L2_NODE_RPC")]
    pub l2_node_rpc: Url,
    /// Inclusive start block. Defaults to `end_block - 1`.
    #[arg(long, env = "OPENVM_START_BLOCK")]
    pub start_block: Option<u64>,
    /// Inclusive end block. Defaults to the current safe L2 head.
    #[arg(long, env = "OPENVM_END_BLOCK")]
    pub end_block: Option<u64>,
    /// Directory that receives `input.json` and `input.bin`.
    #[arg(long)]
    pub out_dir: PathBuf,
}

impl OpenVmRangeDump {
    /// Resolves the range from op-node (if unset) and writes `<out_dir>/input.json`
    /// for `cargo openvm` and `<out_dir>/input.bin` for an Axiom Edge manager.
    ///
    /// Prints `OPENVM_WITNESS start=.. end=.. bytes=..` to stdout once the range is
    /// chosen (`bytes=0`) and again after the witness is written. The basectl
    /// `OpenVM` view parses these lines.
    pub async fn dump(&self) -> Result<()> {
        let rollup_provider = RootProvider::<Base>::new_http(self.l2_node_rpc.clone());
        let status = rollup_provider.optimism_sync_status().await?;
        let safe_head = status.safe_l2.number;
        let end_block = self.end_block.unwrap_or(safe_head);
        ensure!(
            end_block <= safe_head,
            "end_block {end_block} is past the safe L2 head {safe_head}"
        );
        let start_block = match self.start_block {
            Some(start) => start,
            None => end_block
                .checked_sub(1)
                .ok_or_else(|| eyre::eyre!("cannot prove genesis as a one-block range"))?,
        };
        ensure!(
            start_block < end_block,
            "start_block {start_block} must be less than end_block {end_block}"
        );
        Self::announce(start_block, end_block, 0);

        let l1_head = status.head_l1.hash;
        info!(start_block, end_block, l1_head = %l1_head, "dumping OpenVM range witness");
        let config_dir = TempDir::new().wrap_err("failed to create succinct config directory")?;
        let fetcher = OPSuccinctDataFetcher::from_rpc_config_with_rollup_config(RPCConfig {
            l1_rpc: self.l1_rpc.clone(),
            l1_beacon_rpc: Some(self.l1_beacon_rpc.clone()),
            l2_rpc: self.l2_rpc.clone(),
            l2_node_rpc: self.l2_node_rpc.clone(),
            l1_config_dir: Some(config_dir.path().join("L1")),
            l2_config_dir: Some(config_dir.path().join("L2")),
        })
        .await
        .map_err(|error| eyre::eyre!("failed to initialize Succinct data fetcher: {error}"))?;
        let stdin = OpSuccinctWitnessProvider::new(Arc::new(fetcher))
            .generate_witness(WitnessParams {
                start_block,
                end_block,
                l1_head: L1HeadSource::Pinned(l1_head),
                schedule_l2_block_number: None,
            })
            .await
            .map_err(|error| eyre::eyre!("failed to generate range witness: {error}"))?;
        let rkyv = stdin.buffer.first().filter(|rkyv| !rkyv.is_empty());
        let rkyv = rkyv.ok_or_else(|| eyre::eyre!("generated rkyv witness was empty"))?;

        fs::create_dir_all(&self.out_dir).wrap_err("failed to create OpenVM elf directory")?;
        let input_path = self.out_dir.join("input.json");
        fs::write(&input_path, Self::input_json(rkyv))
            .wrap_err("failed to write OpenVM input JSON")?;
        fs::write(self.out_dir.join("input.bin"), Self::input_stdin(rkyv))
            .wrap_err("failed to write OpenVM StdIn input")?;
        info!(rkyv_bytes = rkyv.len(), input_path = %input_path.display(), "wrote OpenVM range input");
        Self::announce(start_block, end_block, rkyv.len());
        Ok(())
    }

    /// `OpenVM` `--input` JSON: one hex string, `0x01` framing + rkyv bytes.
    pub fn input_json(rkyv: &[u8]) -> String {
        let mut hex = String::with_capacity(4 + rkyv.len() * 2);
        hex.push_str("0x01");
        for byte in rkyv {
            write!(hex, "{byte:02x}").expect("writing hex into String is infallible");
        }
        serde_json::json!({ "input": [hex] }).to_string()
    }

    /// Bincode `openvm_sdk::StdIn<BabyBear>` holding the rkyv bytes as one stream.
    ///
    /// Layout: stream count, then per stream its length and one Montgomery-form
    /// `BabyBear` element per byte, then an empty deferral list. All integers are
    /// little-endian; lengths are `u64`, elements `u32`.
    pub fn input_stdin(rkyv: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + rkyv.len() * 4);
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&(rkyv.len() as u64).to_le_bytes());
        for &byte in rkyv {
            let montgomery = (u64::from(byte) << 32) % BABY_BEAR_MODULUS;
            out.extend_from_slice(&(montgomery as u32).to_le_bytes());
        }
        out.extend_from_slice(&0u64.to_le_bytes());
        out
    }

    fn announce(start_block: u64, end_block: u64, bytes: usize) {
        println!("OPENVM_WITNESS start={start_block} end={end_block} bytes={bytes}");
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_json_frames_rkyv_with_0x01() {
        let json = OpenVmRangeDump::input_json(&[0xab, 0xcd]);
        assert_eq!(json, r#"{"input":["0x01abcd"]}"#);
    }

    #[test]
    fn input_stdin_matches_edge_convert_fixtures() {
        // `convert_fixtures json-to-stdin` output for `{"input":["0x0100017fff"]}`.
        let expected = [
            "0100000000000000",
            "0400000000000000",
            "00000000feffff0ff2feff6fe1fdff77",
            "0000000000000000",
        ]
        .concat();
        let stdin = OpenVmRangeDump::input_stdin(&[0x00, 0x01, 0x7f, 0xff]);
        let hex: String = stdin.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, expected);
    }
}
