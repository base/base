//! Red-line capability seal for `mev-trader-submit` (mirrors the spirit of the TS
//! `scripts/arb-dryrun/rung2-seal.test.ts` and the `base-mev-trader`
//! `capability_seal.rs`). Author ≠ reviewer: these are machine-checks a reviewer
//! can re-run.
//!
//! It enforces, by construction:
//!   (a) NO real private-key loader — no file/env/argv/keystore/mnemonic/homedir
//!       path; the ONLY key material is `SigningKey::random`.
//!   (b) NO real submission sink — production code opens no socket/URL; the only
//!       network egress anywhere is the e2e test's spawned loopback anvil.
//!   (c) the ephemeral key never escapes or is logged.
//!   (d) the rung-2/rung-3 blocked boundaries are present and error.
//!   (e) the default build is clean — every dependency is optional and no crate
//!       in the workspace links this crate, so the node binary can never contain
//!       a signer/submit code path.
#![cfg(feature = "phase-b")]

use std::{collections::BTreeSet, path::PathBuf, process::Command};

use mev_trader_submit::{
    assembler::{BlockedBoundary, sign_blocked, submit_blocked},
    signer::{SignerError, sign_ephemeral_atomic_tx, verify_ephemeral_signed_tx},
};

mod support;
use alloy_primitives::{U256, keccak256};
use base_mev_trader::ExactProtocol;
use mev_trader_submit::assembler::{
    AssembleInput, HopExecutionParams, assemble_unsigned_atomic_tx,
};
use support::{EXECUTOR, backrun_plan, victim_with_priority};

const PRODUCTION_FILES: [&str; 4] = ["lib.rs", "fee.rs", "assembler.rs", "signer.rs"];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Strip `//` line and `/* */` block comments while KEEPING string/char literals,
/// so capability scans see real code (including any URL string literal) but never
/// trip on explanatory doc comments (which legitimately name the forbidden things
/// they forbid).
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                out.push('"');
                i += 1;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    match bytes[i] {
                        b'\\' if i + 1 < bytes.len() => {
                            out.push(bytes[i + 1] as char);
                            i += 2;
                        }
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            other => {
                out.push(other as char);
                i += 1;
            }
        }
    }
    out
}

fn read_production_code(file: &str) -> String {
    strip_comments(&std::fs::read_to_string(manifest_dir().join("src").join(file)).expect("source"))
}

fn read_production_source() -> String {
    let mut combined = String::new();
    for file in PRODUCTION_FILES {
        combined.push_str(&read_production_code(file));
        combined.push('\n');
    }
    combined
}

/// Fail closed if a new production `.rs` file appears in `src/` that this seal does
/// not scan — it must be added here (and re-reviewed) before it can ship.
#[test]
fn production_source_is_exactly_the_scanned_set() {
    let src = manifest_dir().join("src");
    let actual: BTreeSet<String> = std::fs::read_dir(&src)
        .expect("src dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    let expected: BTreeSet<String> =
        PRODUCTION_FILES.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(actual, expected, "unscanned production source file present in src/");
}

#[test]
fn no_real_key_loader_or_persistence_in_production() {
    let source = read_production_source();
    // The ephemeral generator must be present and is the ONLY key source.
    assert!(source.contains("SigningKey::random("), "ephemeral key generator missing");
    // No key-import / wallet / keystore / mnemonic loader of any kind.
    for forbidden in [
        "SigningKey::from",
        "SecretKey::from",
        "from_bytes",
        "from_str",
        "from_mnemonic",
        "MnemonicBuilder",
        "Mnemonic",
        "PrivateKeySigner",
        "LocalSigner",
        "AwsSigner",
        "GcpSigner",
        "LedgerSigner",
        "keystore",
        "Keystore",
        "decrypt",
        "PRIVATE_KEY",
        "MNEMONIC",
        "private_key",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden key-loader token in production: {forbidden}"
        );
    }
    // No filesystem / environment / argv access — a persistent key can enter only
    // through one of these, and the safe-prefix touches none of them.
    for forbidden in [
        "std::fs",
        "std::env",
        "fs::",
        "env::",
        "File::",
        "OpenOptions",
        "read_to_string",
        "read_dir",
        "home_dir",
        "var_os",
        "args_os",
        "env!",
        "option_env!",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden fs/env/argv access in production: {forbidden}"
        );
    }
}

#[test]
fn no_real_submission_sink_in_production() {
    let source = read_production_source();
    // No socket / HTTP / websocket transport primitive of any kind.
    for forbidden in [
        "std::net",
        "TcpStream",
        "TcpListener",
        "UdpSocket",
        "reqwest",
        "hyper",
        "ureq",
        "tungstenite",
        "WebSocket",
        "connect(",
        "::connect",
        "Command",
        "spawn(",
    ] {
        assert!(
            !source.contains(forbidden),
            "forbidden network/process sink in production: {forbidden}"
        );
    }
    // No real endpoints. `://` catches any URL scheme in a real string literal
    // (comments are stripped, so the doc text is not scanned). The struct
    // method-name literal "eth_sendBundle" is a payload field, not a call; there
    // is no transport to carry it.
    for forbidden in ["://", "blinklabs", "baseauction", "SEQUENCER_URL", "BLINK_ENDPOINT"] {
        assert!(
            !source.contains(forbidden),
            "forbidden endpoint literal in production: {forbidden}"
        );
    }
    // No logging surface at all — nothing can leak the key or bytes to a log.
    for forbidden in ["println!", "print!", "eprintln!", "eprint!", "dbg!", "tracing::", "log::"] {
        assert!(
            !source.contains(forbidden),
            "forbidden logging surface in production: {forbidden}"
        );
    }
}

#[test]
fn ephemeral_key_is_confined_to_the_signer_and_never_escapes() {
    for file in ["lib.rs", "fee.rs", "assembler.rs"] {
        let source = read_production_code(file);
        assert!(!source.contains("SigningKey"), "SigningKey leaked into {file}");
    }
    let signer = read_production_code("signer.rs");
    // The key is a local binding only: it is never returned or stored publicly.
    assert!(signer.contains("let signing_key = SigningKey::random("));
    for escape in [
        "-> SigningKey",
        "-> &SigningKey",
        "Result<SigningKey",
        "pub signing_key",
        "signing_key:",
        "return signing_key",
    ] {
        assert!(!signer.contains(escape), "ephemeral key escape shape present: {escape}");
    }
    // The returned struct exposes no secret.
    assert!(signer.contains("pub struct EphemeralSignedTx"));
    let struct_block = signer
        .split_once("pub struct EphemeralSignedTx")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(block, _)| block)
        .expect("EphemeralSignedTx block");
    assert!(!struct_block.contains("SigningKey"), "signed struct exposes a key type");
    assert!(!struct_block.contains("SecretKey"), "signed struct exposes a key type");
}

#[test]
fn blocked_boundaries_are_present_and_error() {
    let signed_error = sign_blocked().unwrap_err();
    assert_eq!(signed_error, BlockedBoundary::Sign);
    assert!(
        signed_error.to_string().contains("Rung 2 boundary")
            && signed_error.to_string().contains("signing is intentionally unavailable")
    );
    let submit_error = submit_blocked().unwrap_err();
    assert_eq!(submit_error, BlockedBoundary::Submit);
    assert_eq!(
        submit_error.to_string(),
        "Rung 3 boundary: transaction submission is intentionally unavailable"
    );
}

#[test]
fn each_ephemeral_sign_uses_a_fresh_unfunded_keypair() {
    let (victim_raw, victim_hash) = victim_with_priority(37);
    let plan = backrun_plan([ExactProtocol::UniswapV2, ExactProtocol::UniswapV2], victim_hash);
    let input = AssembleInput {
        plan: &plan,
        current_frame: support::matching_frame(&plan),
        executor: EXECUTOR,
        hops: [
            HopExecutionParams { adapter: support::ADAPTER, min_amount_out: U256::from(1u64) },
            HopExecutionParams { adapter: support::ADAPTER, min_amount_out: U256::from(1u64) },
        ],
        chain_id: 8453,
        nonce: 0,
        gas: 2_000_000,
        max_fee_per_gas: 1_000_000_000,
        valid_until_block: 12_345_678,
        victim_raw_tx: &victim_raw,
        victim_tx_hash: victim_hash,
        expected_victim_priority_fee: Some(37),
    };
    let assembled = assemble_unsigned_atomic_tx(&input).expect("assembled");

    let first = sign_ephemeral_atomic_tx(&assembled.unsigned_tx).expect("first sign");
    let second = sign_ephemeral_atomic_tx(&assembled.unsigned_tx).expect("second sign");
    // Fresh random keypair each call → different signer addresses and bytes.
    assert_ne!(first.signer_address, second.signer_address, "ephemeral key was reused");
    assert_ne!(first.raw_backrun, second.raw_backrun);
    for signed in [&first, &second] {
        assert!(signed.verification.recovered_signer);
        assert!(signed.verification.canonical_low_s);
        assert!(signed.verification.non_dummy_signature);
    }

    // The rung-1 dummy envelope is rejected by rung-2 verification (high-s /
    // dummy signature) — it is not a real, recoverable signature.
    let rejected = verify_ephemeral_signed_tx(
        &assembled.unsigned_tx,
        &assembled.dummy_signed_raw_tx,
        first.signer_address,
    )
    .unwrap_err();
    assert!(
        matches!(rejected, SignerError::DummySignature | SignerError::HighS),
        "dummy envelope was not rejected: {rejected:?}"
    );
    // The victim binding is enforced.
    let mut wrong = input;
    wrong.victim_tx_hash = keccak256(b"not-the-victim");
    assert!(assemble_unsigned_atomic_tx(&wrong).is_err(), "victim hash binding not enforced");
}

// -- Default-build / dependency-shape machine-checks --------------------------

fn workspace_metadata() -> serde_json::Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline"])
        .current_dir(manifest_dir().join("../../.."))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata json")
}

#[test]
fn every_dependency_is_optional_and_the_only_feature_is_phase_b() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let package = packages
        .iter()
        .find(|package| package["name"] == "mev-trader-submit")
        .expect("mev-trader-submit package");

    // The single gate feature; optional deps use `dep:` so no implicit features leak.
    let features = package["features"].as_object().expect("features map");
    let feature_names: BTreeSet<&str> = features.keys().map(String::as_str).collect();
    assert_eq!(feature_names, BTreeSet::from(["phase-b"]), "unexpected feature surface");

    // Every NORMAL (non-dev) dependency must be optional: with `phase-b` off the
    // crate pulls nothing and compiles to an empty lib.
    for dependency in package["dependencies"].as_array().expect("dependencies") {
        let is_dev = dependency["kind"] == "dev";
        if is_dev {
            continue;
        }
        assert_eq!(dependency["kind"], serde_json::Value::Null, "unexpected build dependency");
        assert_eq!(
            dependency["optional"], true,
            "normal dependency {} is not optional",
            dependency["name"]
        );
    }
}

#[test]
fn no_workspace_crate_links_the_submit_crate() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    for package in packages {
        let name = package["name"].as_str().expect("name");
        if name == "mev-trader-submit" {
            continue;
        }
        for dependency in package["dependencies"].as_array().expect("dependencies") {
            assert_ne!(
                dependency["name"], "mev-trader-submit",
                "{name} depends on mev-trader-submit — it could reach the node binary"
            );
        }
    }
}
