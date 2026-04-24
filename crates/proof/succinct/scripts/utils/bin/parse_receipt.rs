//! Binary for parsing transaction receipts from proof data.

use std::{fs, path::PathBuf};

use alloy_primitives::hex;
use anyhow::Result;
use base_proof_succinct_client_utils::boot::BootInfoStruct;
use base_proof_succinct_elfs::AGGREGATION_ELF;
use clap::Parser;
use sp1_sdk::{
    Elf, HashableKey, ProvingKey, SP1ProofWithPublicValues,
    blocking::{CpuProver, Prover as _},
};

#[derive(Parser, Debug)]
#[command(about = "Parse and display proof receipt public values")]
struct Args {
    /// Path to the bincode-serialized `SP1ProofWithPublicValues` receipt file.
    #[arg(long)]
    receipt_file: PathBuf,

    /// Receipt type: "stark" (range proof) or "snark" (aggregation proof).
    #[arg(long = "type", default_value = "snark")]
    receipt_type: String,

    /// Optional path to write a Foundry-compatible JSON fixture with hex proof bytes, vkey, and journalHash.
    #[arg(long)]
    export_json: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let receipt_bytes = fs::read(&args.receipt_file)?;

    let (mut proof_with_pv, _): (SP1ProofWithPublicValues, _) =
        bincode::serde::decode_from_slice(&receipt_bytes, bincode::config::standard())
            .map_err(|e| anyhow::anyhow!("Failed to deserialize receipt: {e}"))?;

    let raw_pv = proof_with_pv.public_values.as_slice().to_vec();
    println!("Receipt size:        {} bytes", receipt_bytes.len());
    println!("Public values size:  {} bytes", raw_pv.len());
    println!("Public values (hex): 0x{}", hex::encode(&raw_pv));
    println!();

    match args.receipt_type.to_lowercase().as_str() {
        "snark" | "aggregation" => {
            parse_aggregation_outputs(&proof_with_pv, &raw_pv, args.export_json.as_deref())?;
        }
        "stark" | "range" => parse_range_outputs(&mut proof_with_pv)?,
        other => anyhow::bail!("Unknown type '{other}'. Use 'stark' or 'snark'."),
    }

    Ok(())
}

fn parse_aggregation_outputs(
    proof_with_pv: &SP1ProofWithPublicValues,
    raw_pv: &[u8],
    export_json: Option<&std::path::Path>,
) -> Result<()> {
    println!("=== Aggregation Proof (SNARK) Public Values (keccak256 digest) ===");
    println!();

    anyhow::ensure!(
        raw_pv.len() == 32,
        "expected 32-byte keccak256 digest, got {} bytes",
        raw_pv.len()
    );

    let journal_hash = format!("0x{}", hex::encode(raw_pv));
    println!("journal digest (keccak256): {journal_hash}");
    println!();

    let cpu_prover = CpuProver::new();
    let agg_pk = cpu_prover.setup(Elf::Static(AGGREGATION_ELF))?;
    let agg_vk = agg_pk.verifying_key();
    let vkey = agg_vk.bytes32();
    println!("aggregation vkey (imageId): {vkey}");

    let proof_bytes = proof_with_pv.bytes();
    let proof_hex = format!("0x{}", hex::encode(&proof_bytes));
    println!("proof bytes (on-chain):     {} bytes", proof_bytes.len());
    println!("proof bytes (hex):          {proof_hex}");

    if let Some(path) = export_json {
        let json = serde_json::json!({
            "proof": proof_hex,
            "vkey": vkey,
            "journalHash": journal_hash,
        });
        fs::write(path, serde_json::to_string_pretty(&json)?)?;
        println!();
        println!("Exported Foundry fixture to {}", path.display());
    }

    Ok(())
}

fn parse_range_outputs(proof: &mut SP1ProofWithPublicValues) -> Result<()> {
    println!("=== Range Proof (STARK) Public Values ===");
    println!();

    let boot_info: BootInfoStruct = proof.public_values.read();

    println!("l1Head:            {}", boot_info.l1Head);
    println!("l2PreRoot:         {}", boot_info.l2PreRoot);
    println!("l2PostRoot:        {}", boot_info.l2PostRoot);
    println!("l2PreBlockNumber:  {}", boot_info.l2PreBlockNumber);
    println!("l2BlockNumber:     {}", boot_info.l2BlockNumber);
    println!("rollupConfigHash:  {}", boot_info.rollupConfigHash);
    println!(
        "intermediateRoots: {} bytes ({} roots)",
        boot_info.intermediateRoots.len(),
        boot_info.intermediateRoots.len() / 32
    );
    if !boot_info.intermediateRoots.is_empty() {
        for (i, chunk) in boot_info.intermediateRoots.chunks(32).enumerate() {
            println!("  root[{}]: 0x{}", i, hex::encode(chunk));
        }
    }

    Ok(())
}
