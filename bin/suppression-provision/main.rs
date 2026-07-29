//! Provisions the compile-pinned suppression rollback anchor.
//!
//! Provisioning is create-if-absent and idempotent: re-running preserves an existing
//! high-water mark and must never rewind the rollback anchor.

use std::{env, process::ExitCode};

use mev_trader_submit::provision_suppression_anchor;

fn main() -> ExitCode {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "base-mev-suppression-provision".to_owned());
    if args.next().is_some() {
        eprintln!("usage: {program}");
        return ExitCode::FAILURE;
    }

    match provision_suppression_anchor() {
        Ok(()) => {
            eprintln!("suppression rollback anchor provisioned");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("suppression rollback anchor provisioning failed: {error:?}: {error}");
            ExitCode::FAILURE
        }
    }
}
