//! Provisions validated, externally owner-signed T4e simulation artifacts.

use std::{env, path::Path, process::ExitCode};

use mev_trader_submit::ProducerConformance;

fn main() -> ExitCode {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "base-mev-t4e-provision".to_owned());
    let Some(command) = args.next() else {
        eprintln!(
            "usage: {program} <prepare|claim-store|publish-population|publish-projection|publish-install-bundle> [signed-file]"
        );
        return ExitCode::FAILURE;
    };

    match command.as_str() {
        "prepare" if args.next().is_none() => match ProducerConformance::prepare_directories() {
            Ok(()) => {
                println!("T4e private artifact directories prepared");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("T4e directory preparation failed: {error:?}");
                ExitCode::FAILURE
            }
        },
        "claim-store" if args.next().is_none() => {
            match ProducerConformance::provision_claim_store() {
                Ok(identity) => {
                    for byte in identity {
                        print!("{byte:02x}");
                    }
                    println!();
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("T4e claim-store provisioning failed: {error:?}");
                    ExitCode::FAILURE
                }
            }
        }
        "publish-population" | "publish-projection" | "publish-install-bundle" => {
            let Some(source) = args.next() else {
                eprintln!("usage: {program} {command} <signed-file>");
                return ExitCode::FAILURE;
            };
            if args.next().is_some() {
                eprintln!("usage: {program} {command} <signed-file>");
                return ExitCode::FAILURE;
            }
            let result = match command.as_str() {
                "publish-population" => {
                    ProducerConformance::publish_population_file(Path::new(&source))
                }
                "publish-projection" => {
                    ProducerConformance::publish_projection_file(Path::new(&source))
                }
                "publish-install-bundle" => {
                    ProducerConformance::publish_install_bundle_file(Path::new(&source))
                }
                _ => unreachable!("matched provisioning command"),
            };
            match result {
                Ok(()) => {
                    println!("{command} complete");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{command} failed: {error:?}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "usage: {program} <prepare|claim-store|publish-population|publish-projection|publish-install-bundle> [signed-file]"
            );
            ExitCode::FAILURE
        }
    }
}
