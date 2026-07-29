//! Applies a verify-only owner attestation to the production kill-state store.

use std::{
    env,
    io::{self, Write},
    process::ExitCode,
};

use base_mev_trader::{
    KillStateStore, KillStoreError, OWNER_ATTEST_ADDRESS, ResetAttestation, open_anchored_killstate,
};

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
    let first = required_arg(&mut args, &program, "engagement_epoch or --prepare")?;
    if first == "--prepare" {
        return prepare(&mut args, &program);
    }

    let engagement_epoch = parse_u64(&first, "engagement_epoch")?;
    let nonce = parse_u64(&required_arg(&mut args, &program, "nonce")?, "nonce")?;
    let signature_hex = required_arg(&mut args, &program, "signature_hex")?;
    if args.next().is_some() {
        return Err(usage(&program));
    }

    emit_message(engagement_epoch, nonce)?;
    let attestation = ResetAttestation::from_signature_hex(engagement_epoch, nonce, &signature_hex)
        .map_err(format_kill_store_error)?;
    let store = open_anchored_killstate()
        .map_err(|error| format!("kill-reset store open failed: {error:?}: {error}"))?;
    store.owner_reset(&attestation).map_err(format_kill_store_error)?;
    eprintln!("kill-reset applied for engagement epoch {engagement_epoch}");
    Ok(())
}

fn prepare(args: &mut impl Iterator<Item = String>, program: &str) -> Result<(), String> {
    let engagement_epoch =
        parse_u64(&required_arg(args, program, "engagement_epoch")?, "engagement_epoch")?;
    let nonce = parse_u64(&required_arg(args, program, "nonce")?, "nonce")?;
    if args.next().is_some() {
        return Err(usage(program));
    }

    emit_message(engagement_epoch, nonce)?;
    let owner = OWNER_ATTEST_ADDRESS
        .ok_or_else(|| format_kill_store_error(KillStoreError::OwnerAddressUnset))?;
    println!("{owner}");
    io::stdout().flush().map_err(|error| format!("failed to emit owner address: {error}"))
}

fn emit_message(engagement_epoch: u64, nonce: u64) -> Result<(), String> {
    let message = ResetAttestation::message_for(engagement_epoch, nonce);
    println!("{message}");
    io::stdout().flush().map_err(|error| format!("failed to emit reset message: {error}"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|_| format!("invalid {name}: expected an unsigned 64-bit integer"))
}

fn required_arg(
    args: &mut impl Iterator<Item = String>,
    program: &str,
    name: &str,
) -> Result<String, String> {
    args.next().ok_or_else(|| format!("missing {name}\n{}", usage(program)))
}

fn usage(program: &str) -> String {
    format!(
        "usage: {program} <engagement_epoch> <nonce> <130-char-lowercase-signature-hex>\n       {program} --prepare <engagement_epoch> <nonce>"
    )
}

fn format_kill_store_error(error: KillStoreError) -> String {
    format!("kill-reset check failed: {error:?}: {error}")
}
