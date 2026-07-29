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
//!   (e) the default build is clean — every dependency is optional, and the sole
//!       unconditional workspace linker is a capability-minimal provisioning leaf.
//!       The existing CLI declaration remains optional and separately feature-sealed.
#![cfg(feature = "phase-b")]

use std::{collections::BTreeSet, path::PathBuf, process::Command};

use mev_trader_submit::{
    assembler::{BlockedBoundary, sign_blocked, submit_blocked},
    signer::{SignerError, sign_ephemeral_atomic_tx, verify_ephemeral_signed_tx},
};
use serde_json::Value;

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
        priority_economics: None,
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

#[test]
fn production_never_self_loads_or_aliases_production_arming_criteria() {
    // Alias-bypass guard for the arming self-load seal. `production_arming_criteria`
    // is a public producer in `base_mev_trader`; the arm seal forbids its token in
    // src/arm/*. A re-export/alias would instead have to live in the clean-4 surface
    // (lib/fee/assembler/signer), so that surface must not name the token either.
    // Together the two seals close every submit production file, so no submit code can
    // obtain its own armed criteria — B5 injects a verified value (a dependent cannot
    // self-forge). Comments are stripped, so an explanatory mention would not trip.
    let source = read_production_source();
    assert!(
        !source.contains("production_arming_criteria"),
        "clean-4 production source references production_arming_criteria \
         (submit must not self-load or alias arming criteria)"
    );
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
fn feature_surface_and_deps_are_pinned() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let package = packages
        .iter()
        .find(|package| package["name"] == "mev-trader-submit")
        .expect("mev-trader-submit package");

    // The exact gate-feature surface. `phase-b` = assembler + ephemeral signer;
    // `arm` = the B3-arm tier (key loader + witness + transport builders, no real
    // egress); `arm-live-egress` = the ONLY config that compiles a real reqwest
    // egress; `arm-provisioning` = the SuppressionEpochStore bootstrap surface.
    // Any NEW feature must be added here (and re-reviewed) before it can ship.
    let features = package["features"].as_object().expect("features map");
    let feature_names: BTreeSet<&str> = features.keys().map(String::as_str).collect();
    assert_eq!(
        feature_names,
        BTreeSet::from(["phase-b", "arm", "arm-live-egress", "arm-provisioning"]),
        "unexpected feature surface"
    );

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

const SUBMIT_PACKAGE: &str = "mev-trader-submit";
const PROVISION_PACKAGE: &str = "base-suppression-provision-bin";
const EXISTING_OPTIONAL_LINKER: &str = "base-execution-cli";

fn required_array<'a>(
    value: &'a Value,
    field: &str,
    context: &str,
) -> Result<&'a Vec<Value>, String> {
    value
        .get(field)
        .ok_or_else(|| format!("{context} is missing `{field}`"))?
        .as_array()
        .ok_or_else(|| format!("{context}.`{field}` is not an array"))
}

fn required_string<'a>(value: &'a Value, field: &str, context: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .ok_or_else(|| format!("{context} is missing `{field}`"))?
        .as_str()
        .ok_or_else(|| format!("{context}.`{field}` is not a string"))
}

fn required_bool(value: &Value, field: &str, context: &str) -> Result<bool, String> {
    value
        .get(field)
        .ok_or_else(|| format!("{context} is missing `{field}`"))?
        .as_bool()
        .ok_or_else(|| format!("{context}.`{field}` is not a boolean"))
}

fn require_null(value: &Value, field: &str, context: &str) -> Result<(), String> {
    let field_value = value.get(field).ok_or_else(|| format!("{context} is missing `{field}`"))?;
    if field_value.is_null() {
        Ok(())
    } else {
        Err(format!("{context}.`{field}` must be null, got {field_value}"))
    }
}

fn require_exact_strings(values: &[Value], expected: &[&str], context: &str) -> Result<(), String> {
    let actual: Result<Vec<&str>, String> = values
        .iter()
        .map(|value| value.as_str().ok_or_else(|| format!("{context} contains a non-string entry")))
        .collect();
    let actual = actual?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{context} must be exactly {expected:?}, got {actual:?}"))
    }
}

fn validate_submit_linkers(metadata: &Value) -> Result<(), String> {
    let packages = required_array(metadata, "packages", "metadata")?;
    let mut declared_submit_dependers = Vec::new();
    let mut unconditional_linkers = Vec::new();
    let mut provisioning_package = None;
    let mut provisioning_dependers = BTreeSet::new();

    for package in packages {
        let name = required_string(package, "name", "package")?;
        let dependencies = required_array(package, "dependencies", &format!("package `{name}`"))?;
        if name == PROVISION_PACKAGE {
            if provisioning_package.replace(package).is_some() {
                return Err(format!("duplicate workspace package `{PROVISION_PACKAGE}`"));
            }
        }

        for dependency in dependencies {
            let dependency_context = format!("dependency of `{name}`");
            let dependency_name = required_string(dependency, "name", &dependency_context)?;
            if dependency_name == SUBMIT_PACKAGE {
                declared_submit_dependers.push(name.to_owned());
                if !required_bool(dependency, "optional", &dependency_context)? {
                    unconditional_linkers.push(name.to_owned());
                }
            }
            if dependency_name == PROVISION_PACKAGE {
                provisioning_dependers.insert(name.to_owned());
            }
        }
    }
    declared_submit_dependers.sort();
    unconditional_linkers.sort();

    let expected_declared = vec![EXISTING_OPTIONAL_LINKER.to_owned(), PROVISION_PACKAGE.to_owned()];
    if declared_submit_dependers != expected_declared {
        return Err(format!(
            "workspace packages declaring `{SUBMIT_PACKAGE}` must be exactly \
             {expected_declared:?}, got {declared_submit_dependers:?}"
        ));
    }
    if !unconditional_linkers.is_empty() {
        return Err(format!(
            "unconditional workspace linkers of `{SUBMIT_PACKAGE}` must be empty, got \
             {unconditional_linkers:?}"
        ));
    }
    if !provisioning_dependers.is_empty() {
        return Err(format!(
            "`{PROVISION_PACKAGE}` must be a leaf package, depended on by \
             {provisioning_dependers:?}"
        ));
    }

    let package = provisioning_package
        .ok_or_else(|| format!("workspace package `{PROVISION_PACKAGE}` is missing"))?;
    let dependencies =
        required_array(package, "dependencies", &format!("package `{PROVISION_PACKAGE}`"))?;
    if dependencies.len() != 1 {
        return Err(format!(
            "`{PROVISION_PACKAGE}` must have exactly one dependency, got {}",
            dependencies.len()
        ));
    }

    let dependency = &dependencies[0];
    let context = format!("`{PROVISION_PACKAGE}` dependency");
    let dependency_name = required_string(dependency, "name", &context)?;
    if dependency_name != SUBMIT_PACKAGE {
        return Err(format!(
            "`{PROVISION_PACKAGE}` must depend only on `{SUBMIT_PACKAGE}`, got `{dependency_name}`"
        ));
    }

    let features = required_array(dependency, "features", &context)?;
    let feature_names: Result<Vec<&str>, String> = features
        .iter()
        .map(|feature| {
            feature
                .as_str()
                .ok_or_else(|| format!("{context}.`features` contains a non-string entry"))
        })
        .collect();
    let feature_names = feature_names?;
    if feature_names.contains(&"arm-live-egress") {
        return Err(format!("`{PROVISION_PACKAGE}` must never enable `arm-live-egress`"));
    }
    if feature_names != ["arm-provisioning"] {
        return Err(format!(
            "`{PROVISION_PACKAGE}` must enable exactly [\"arm-provisioning\"], got {feature_names:?}"
        ));
    }
    if !required_bool(dependency, "uses_default_features", &context)? {
        return Err(format!("`{PROVISION_PACKAGE}` must use default features"));
    }
    if !required_bool(dependency, "optional", &context)? {
        return Err(format!("`{PROVISION_PACKAGE}` dependency must remain optional"));
    }
    for field in ["kind", "rename", "target"] {
        require_null(dependency, field, &context)?;
    }

    let package_features = package
        .get("features")
        .ok_or_else(|| format!("package `{PROVISION_PACKAGE}` is missing `features`"))?
        .as_object()
        .ok_or_else(|| format!("package `{PROVISION_PACKAGE}`.`features` is not an object"))?;
    let provision_feature = package_features
        .get("provision")
        .ok_or_else(|| format!("package `{PROVISION_PACKAGE}` is missing feature `provision`"))?
        .as_array()
        .ok_or_else(|| {
            format!("package `{PROVISION_PACKAGE}` feature `provision` is not an array")
        })?;
    require_exact_strings(
        provision_feature,
        &["dep:mev-trader-submit"],
        &format!("`{PROVISION_PACKAGE}` feature `provision`"),
    )?;

    let targets = required_array(package, "targets", &format!("package `{PROVISION_PACKAGE}`"))?;
    let mut provisioning_bins = Vec::new();
    for target in targets {
        let target_name = required_string(target, "name", "provisioning package target")?;
        if target_name == "base-mev-suppression-provision" {
            provisioning_bins.push(target);
        }
    }
    if provisioning_bins.len() != 1 {
        return Err(format!(
            "`{PROVISION_PACKAGE}` must have exactly one `base-mev-suppression-provision` target, \
             got {}",
            provisioning_bins.len()
        ));
    }
    let required_features =
        required_array(provisioning_bins[0], "required-features", "provisioning binary target")?;
    require_exact_strings(
        required_features,
        &["provision"],
        "provisioning binary `required-features`",
    )?;

    Ok(())
}

fn package_mut<'a>(metadata: &'a mut Value, name: &str) -> &'a mut Value {
    metadata["packages"]
        .as_array_mut()
        .expect("packages")
        .iter_mut()
        .find(|package| package["name"] == name)
        .unwrap_or_else(|| panic!("package `{name}`"))
}

fn provision_dependency_mut(metadata: &mut Value) -> &mut Value {
    package_mut(metadata, PROVISION_PACKAGE)["dependencies"]
        .as_array_mut()
        .expect("dependencies")
        .first_mut()
        .expect("provision dependency")
}

fn cli_dependency_mut(metadata: &mut Value) -> &mut Value {
    package_mut(metadata, EXISTING_OPTIONAL_LINKER)["dependencies"]
        .as_array_mut()
        .expect("dependencies")
        .iter_mut()
        .find(|dependency| dependency["name"] == SUBMIT_PACKAGE)
        .expect("CLI submit dependency")
}

fn submit_dependency(metadata: &Value) -> Value {
    metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == PROVISION_PACKAGE)
        .expect("provision package")["dependencies"][0]
        .clone()
}

fn add_dependency_to_other_package(metadata: &mut Value, dependency: Value) {
    let package = metadata["packages"]
        .as_array_mut()
        .expect("packages")
        .iter_mut()
        .find(|package| package["name"] != PROVISION_PACKAGE && package["name"] != SUBMIT_PACKAGE)
        .expect("other workspace package");
    package["dependencies"].as_array_mut().expect("dependencies").push(dependency);
}

fn assert_mutant_rejected(original: &Value, mutant: &Value, mutant_name: &str) {
    assert_ne!(mutant, original, "{mutant_name} patch did not change metadata");
    let error = match validate_submit_linkers(mutant) {
        Ok(()) => panic!("{mutant_name} unexpectedly passed the linker seal"),
        Err(error) => error,
    };
    eprintln!("{mutant_name}: RED ({error})");
}

#[test]
fn submit_linker_seal_m0_unmodified_metadata_is_green() {
    let metadata = workspace_metadata();
    validate_submit_linkers(&metadata).expect("M0 metadata must satisfy the linker seal");
    eprintln!("M0: GREEN");
}

#[test]
fn submit_linker_seal_m1_rejects_a_second_linker() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    add_dependency_to_other_package(&mut mutant, submit_dependency(&original));
    assert_mutant_rejected(&original, &mutant, "M1");
}

#[test]
fn submit_linker_seal_m2_rejects_live_egress_feature() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    provision_dependency_mut(&mut mutant)["features"]
        .as_array_mut()
        .expect("features")
        .push(Value::String("arm-live-egress".to_owned()));
    assert_mutant_rejected(&original, &mutant, "M2");
}

#[test]
fn submit_linker_seal_m3_rejects_any_additional_feature() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    provision_dependency_mut(&mut mutant)["features"]
        .as_array_mut()
        .expect("features")
        .push(Value::String("arm".to_owned()));
    assert_mutant_rejected(&original, &mutant, "M3");
}

#[test]
fn submit_linker_seal_m4_rejects_unconditional_dependency() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    provision_dependency_mut(&mut mutant)["optional"] = Value::Bool(false);
    assert_mutant_rejected(&original, &mutant, "M4");
}

#[test]
fn submit_linker_seal_m5_rejects_renamed_dependency() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    provision_dependency_mut(&mut mutant)["rename"] = Value::String("renamed-submit".to_owned());
    assert_mutant_rejected(&original, &mutant, "M5");
}

#[test]
fn submit_linker_seal_m6_rejects_non_leaf_provisioner() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    let mut dependency = submit_dependency(&original);
    dependency["name"] = Value::String(PROVISION_PACKAGE.to_owned());
    add_dependency_to_other_package(&mut mutant, dependency);
    assert_mutant_rejected(&original, &mutant, "M6");
}

#[test]
fn submit_linker_seal_m7_rejects_a_second_provisioner_dependency() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    let mut dependency = submit_dependency(&original);
    dependency["name"] = Value::String("mutant-extra-dependency".to_owned());
    package_mut(&mut mutant, PROVISION_PACKAGE)["dependencies"]
        .as_array_mut()
        .expect("dependencies")
        .push(dependency);
    assert_mutant_rejected(&original, &mutant, "M7");
}

#[test]
fn submit_linker_seal_m8_rejects_target_scoped_duplicate_edge() {
    let original = workspace_metadata();
    let mut mutant = original.clone();
    let mut duplicate = cli_dependency_mut(&mut mutant).clone();
    duplicate["target"] = Value::String("cfg(target_os = \"linux\")".to_owned());
    package_mut(&mut mutant, EXISTING_OPTIONAL_LINKER)["dependencies"]
        .as_array_mut()
        .expect("dependencies")
        .push(duplicate);
    assert_mutant_rejected(&original, &mutant, "M8");
}
