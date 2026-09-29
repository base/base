//! CLI for reporting deterministic synthetic channel-compression measurements.

use std::{env, process::ExitCode};

use base_comp::{
    CompressionBenchmark, CompressionScenario, DEFAULT_TRANSACTION_PROFILES, InputPattern,
    TransactionKind, TransactionProfile,
};

const DEFAULT_TRANSACTIONS_PER_BATCH: usize = 128;
const DEFAULT_BATCHES_PER_CHANNEL: usize = 8;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut transactions_per_batch = DEFAULT_TRANSACTIONS_PER_BATCH;
    let mut batches_per_channel = DEFAULT_BATCHES_PER_CHANNEL;
    let mut incremental = false;
    let mut extra_profiles = Vec::new();
    let mut arguments = env::args().skip(1);

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--transactions-per-batch" => {
                transactions_per_batch = parse_usize(arguments.next(), "--transactions-per-batch")?;
            }
            "--batches-per-channel" => {
                batches_per_channel = parse_usize(arguments.next(), "--batches-per-channel")?;
            }
            "--profile" => extra_profiles.push(parse_profile(arguments.next())?),
            "--incremental" => incremental = true,
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            _ => return Err(format!("unrecognized argument `{argument}`")),
        }
    }

    if transactions_per_batch == 0 || batches_per_channel == 0 {
        return Err("batch and channel counts must be greater than zero".to_string());
    }

    let mut profiles = DEFAULT_TRANSACTION_PROFILES.to_vec();
    profiles.extend(extra_profiles);
    print_header(transactions_per_batch, batches_per_channel, incremental);

    for pattern in [InputPattern::Pseudorandom, InputPattern::Incrementing] {
        for profile in &profiles {
            let scenario = CompressionScenario {
                profile: *profile,
                pattern,
                transactions_per_batch,
                batches_per_channel,
            };
            let measurement =
                CompressionBenchmark.measure(scenario).map_err(|error| error.to_string())?;
            if incremental {
                for increment in CompressionBenchmark
                    .measure_incremental(scenario)
                    .map_err(|error| error.to_string())?
                {
                    println!(
                        "{},{},{},{},{},{},{}",
                        profile.kind.label(),
                        pattern.label(),
                        increment.transaction_index,
                        increment.uncompressed_bytes,
                        increment.compressed_bytes,
                        increment.includes_final_trailer,
                        measurement.compressed_bytes,
                    );
                }
            } else {
                println!(
                    "{},{},{},{},{},{},{},{}",
                    profile.kind.label(),
                    pattern.label(),
                    scenario.transaction_count(),
                    measurement.uncompressed_bytes,
                    measurement.compressed_bytes,
                    profile.encoded_bytes,
                    measurement.estimated_compressed_bytes_per_transaction(),
                    format_args!("{:.5}", measurement.compression_ratio()),
                );
            }
        }
    }

    Ok(())
}

fn parse_usize(value: Option<String>, flag: &str) -> Result<usize, String> {
    let value = value.ok_or_else(|| format!("{flag} requires an integer"))?;
    value.parse().map_err(|_| format!("{flag} requires an integer, got `{value}`"))
}

fn parse_profile(value: Option<String>) -> Result<TransactionProfile, String> {
    let value = value.ok_or_else(|| "--profile requires NAME:ENCODED_BYTES".to_string())?;
    let (_, encoded_bytes) =
        value.split_once(':').ok_or_else(|| "--profile must be NAME:ENCODED_BYTES".to_string())?;
    let encoded_bytes =
        encoded_bytes.parse().map_err(|_| format!("invalid encoded byte count in `{value}`"))?;
    if encoded_bytes == 0 {
        return Err("--profile requires a nonzero byte count".to_string());
    }

    Ok(TransactionProfile { kind: TransactionKind::CustomCall, encoded_bytes })
}

fn print_header(transactions_per_batch: usize, batches_per_channel: usize, incremental: bool) {
    eprintln!(
        "Synthetic raw-byte channel benchmark: {transactions_per_batch} tx/batch, {batches_per_channel} batches/channel, Brotli quality 10."
    );
    eprintln!(
        "Pseudorandom is the incompressible-data estimate; incrementing is a deliberately compressible control."
    );
    if incremental {
        println!(
            "profile,pattern,transaction_index,uncompressed_bytes,incremental_compressed_bytes,includes_final_trailer,total_channel_bytes"
        );
    } else {
        println!(
            "profile,pattern,transactions,uncompressed_bytes,compressed_channel_bytes,input_bytes_per_tx,estimated_compressed_bytes_per_tx,compression_ratio"
        );
    }
}

fn print_usage() {
    eprintln!(
        "Usage: cargo run -p base-comp --features benchmark --bin base-compression-benchmark -- [--transactions-per-batch N] [--batches-per-channel N] [--profile NAME:ENCODED_BYTES]... [--incremental]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_custom_transaction_profile() {
        let profile = parse_profile(Some("erc1155_transfer:512".to_string())).unwrap();

        assert_eq!(profile.kind, TransactionKind::CustomCall);
        assert_eq!(profile.encoded_bytes, 512);
    }

    #[test]
    fn rejects_malformed_transaction_profiles() {
        assert!(parse_profile(Some("missing_size".to_string())).is_err());
        assert!(parse_profile(Some("name:0".to_string())).is_err());
    }
}
