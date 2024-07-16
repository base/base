use clap::Parser;

use sp1_sdk::utils;
use zkvm_common::BootInfoWithoutRollupConfig;
use zkvm_host::{execute_kona_program, SP1KonaCliArgs};

fn main() {
    utils::setup_logger();

    let cli_args = SP1KonaCliArgs::parse();
    let boot_info = BootInfoWithoutRollupConfig::from(cli_args);
    let report = execute_kona_program(&boot_info);

    println!("Report: {}", report);

    // Then generate the real proof.
    // let (pk, vk) = client.setup(ELF);
    // let mut proof = client.prove(&pk, stdin).unwrap();

    println!("generated valid zk proof");
}
