//! Applies a verify-only owner attestation to the production kill-state store.

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

use base_mev_trader::{KillStateStore, KillStoreError, ResetAttestation, open_anchored_killstate};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "base-mev-kill-reset".to_owned());
    let engagement_epoch = required_arg(&mut args, &program, "engagement_epoch")?
        .parse::<u64>()
        .map_err(|_| "invalid engagement_epoch: expected an unsigned 64-bit integer".to_owned())?;
    let nonce = required_arg(&mut args, &program, "nonce")?
        .parse::<u64>()
        .map_err(|_| "invalid nonce: expected an unsigned 64-bit integer".to_owned())?;
    let signature_hex = required_arg(&mut args, &program, "signature_hex")?;
    if args.next().is_some() {
        return Err(usage(&program));
    }

    let message = ResetAttestation::message_for(engagement_epoch, nonce);
    println!("{message}");
    io::stdout().flush().map_err(|error| format!("failed to emit reset message: {error}"))?;

    let attestation = ResetAttestation::from_signature_hex(engagement_epoch, nonce, &signature_hex)
        .map_err(format_kill_store_error)?;
    let store = open_anchored_killstate()
        .map_err(|error| format!("kill-reset store open failed: {error:?}: {error}"))?;
    store.owner_reset(&attestation).map_err(format_kill_store_error)?;
    eprintln!("kill-reset applied for engagement epoch {engagement_epoch}");
    Ok(())
}

fn required_arg(
    args: &mut impl Iterator<Item = String>,
    program: &str,
    name: &str,
) -> Result<String, String> {
    args.next().ok_or_else(|| format!("missing {name}\n{}", usage(program)))
}

fn usage(program: &str) -> String {
    format!("usage: {program} <engagement_epoch> <nonce> <130-char-lowercase-signature-hex>")
}

fn format_kill_store_error(error: KillStoreError) -> String {
    format!("kill-reset check failed: {error:?}: {error}")
}
