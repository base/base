//! Provisions validated, externally owner-signed T4e simulation artifacts.

use std::{env, path::Path, process::ExitCode};

use mev_trader_submit::{ProducerConformance, T4eProvisioningTool};

const USAGE: &str = "<prepare|claim-store|prepare-population|attach-population|prepare-projection|attach-projection|prepare-install-bundle|attach-install-bundle|publish-population|publish-projection|publish-install-bundle>";

fn main() -> ExitCode {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "base-mev-t4e-provision".to_owned());
    let Some(command) = args.next() else {
        eprintln!("usage: {program} {USAGE} [arguments]");
        return ExitCode::FAILURE;
    };

    let result = match command.as_str() {
        "prepare" if args.next().is_none() => ProducerConformance::prepare_directories()
            .map(|()| "T4e private artifact directories prepared".to_owned())
            .map_err(|error| format!("T4e directory preparation failed: {error:?}")),
        "claim-store" if args.next().is_none() => ProducerConformance::provision_claim_store()
            .map(|identity| identity.iter().map(|byte| format!("{byte:02x}")).collect())
            .map_err(|error| format!("T4e claim-store provisioning failed: {error:?}")),
        "prepare-population" => match (args.next(), args.next(), args.next()) {
            (Some(export), Some(request), None) => {
                T4eProvisioningTool::prepare_population(Path::new(&export), Path::new(&request))
                    .map(|()| "prepare-population complete".to_owned())
                    .map_err(|error| error.to_string())
            }
            _ => Err(format!("usage: {program} prepare-population <export> <request-dir>")),
        },
        "prepare-projection" => {
            match (args.next(), args.next(), args.next(), args.next(), args.next()) {
                (Some(export), Some(population), Some(fields), Some(request), None) => {
                    T4eProvisioningTool::prepare_projection(
                        Path::new(&export),
                        Path::new(&population),
                        Path::new(&fields),
                        Path::new(&request),
                    )
                    .map(|()| "prepare-projection complete".to_owned())
                    .map_err(|error| error.to_string())
                }
                _ => Err(format!(
                    "usage: {program} prepare-projection <export> <signed-population> <fields-json> <request-dir>"
                )),
            }
        }
        "prepare-install-bundle" => match (args.next(), args.next(), args.next()) {
            (Some(fields), Some(request), None) => {
                T4eProvisioningTool::prepare_install_bundle(Path::new(&fields), Path::new(&request))
                    .map(|()| "prepare-install-bundle complete".to_owned())
                    .map_err(|error| error.to_string())
            }
            _ => {
                Err(format!("usage: {program} prepare-install-bundle <fields-json> <request-dir>"))
            }
        },
        "attach-population" | "attach-projection" | "attach-install-bundle" => {
            match (args.next(), args.next(), args.next(), args.next()) {
                (Some(request), Some(signature), Some(output), None) => {
                    let kind = command.strip_prefix("attach-").expect("matched attach command");
                    T4eProvisioningTool::attach(
                        kind,
                        Path::new(&request),
                        Path::new(&signature),
                        Path::new(&output),
                    )
                    .map(|()| format!("{command} complete"))
                    .map_err(|error| error.to_string())
                }
                _ => Err(format!(
                    "usage: {program} {command} <request-dir> <signature-file> <signed-file>"
                )),
            }
        }
        "publish-population" | "publish-projection" | "publish-install-bundle" => {
            match (args.next(), args.next()) {
                (Some(source), None) => {
                    let publication = match command.as_str() {
                        "publish-population" => {
                            ProducerConformance::publish_population_file(Path::new(&source))
                        }
                        "publish-projection" => {
                            ProducerConformance::publish_projection_file(Path::new(&source))
                        }
                        "publish-install-bundle" => {
                            ProducerConformance::publish_install_bundle_file(Path::new(&source))
                        }
                        _ => unreachable!("matched publication command"),
                    };
                    publication
                        .map(|()| format!("{command} complete"))
                        .map_err(|error| format!("{command} failed: {error:?}"))
                }
                _ => Err(format!("usage: {program} {command} <signed-file>")),
            }
        }
        _ => Err(format!("usage: {program} {USAGE} [arguments]")),
    };

    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
