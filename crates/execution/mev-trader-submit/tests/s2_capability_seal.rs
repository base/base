//! S2 simulation-rung, unified-entrypoint, and local-persistence mutation seals.
#![cfg(feature = "arm")]

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).expect("sealed source")
}

fn feature_block<'a>(manifest: &'a str, name: &str, next: &str) -> Result<&'a str, String> {
    manifest
        .split_once(&format!("{name} = ["))
        .and_then(|(_, rest)| rest.split_once(&format!("{next} = [")))
        .map(|(block, _)| block)
        .ok_or_else(|| format!("feature block {name} missing or duplicated"))
}

fn validate_sim_rung(cli: &str, node: &str) -> Result<(), String> {
    let cli_sim = feature_block(cli, "arm-sim", "arm-live-egress")?;
    if !cli_sim.contains("\"t4e-handoff\"")
        || !cli_sim.contains("\"mev-trader-submit/arm\"")
        || cli_sim.contains("arm-live-egress")
        || cli_sim.contains("reqwest")
    {
        return Err("CLI arm-sim rung changed".to_owned());
    }
    let node_sim = feature_block(node, "arm-sim", "arm-live-egress")?;
    if !node_sim.contains("base-execution-cli/arm-sim") || node_sim.contains("arm-live-egress") {
        return Err("node arm-sim rung changed".to_owned());
    }
    let node_default = feature_block(node, "default", "edge-measurement")?;
    if node_default.contains("arm-sim") || node_default.contains("arm-live-egress") {
        return Err("default node gained arm capability".to_owned());
    }
    Ok(())
}

#[test]
fn feature_s0_green_s1_live_rung_s3_default_reachability_red() {
    let cli_path = manifest_dir().join("../cli/Cargo.toml");
    let node_path = manifest_dir().join("../../../bin/node/Cargo.toml");
    let cli = read(cli_path);
    let node = read(node_path);
    validate_sim_rung(&cli, &node).expect("S0 exact sim rung");
    eprintln!("S0: GREEN");

    let needle = "    \"mev-trader-submit/arm\",\n";
    let mutant = cli.replacen(
        needle,
        "    \"mev-trader-submit/arm\",\n    \"mev-trader-submit/arm-live-egress\",\n",
        1,
    );
    assert_ne!(mutant, cli, "S1 patch did not change source");
    assert!(validate_sim_rung(&mutant, &node).is_err());
    eprintln!("S1: RED");

    let mutant =
        node.replacen("default = [ \"jemalloc\" ]", "default = [ \"jemalloc\", \"arm-sim\" ]", 1);
    assert_ne!(mutant, node, "S3 patch did not change source");
    assert!(validate_sim_rung(&cli, &mutant).is_err());
    eprintln!("S3: RED");
}

fn validate_submit_closure_fixture(value: &serde_json::Value) -> Result<(), String> {
    let object = value.as_object().ok_or_else(|| "fixture must be object".to_owned())?;
    if object.keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>()
        != std::collections::BTreeSet::from(["dependencies", "features"])
    {
        return Err("fixture keys differ".to_owned());
    }
    let features = object["features"].as_array().ok_or_else(|| "features array".to_owned())?;
    let dependencies =
        object["dependencies"].as_array().ok_or_else(|| "dependencies array".to_owned())?;
    if !features.iter().any(|value| value.as_str() == Some("arm"))
        || features.iter().any(|value| value.as_str() == Some("arm-live-egress"))
        || dependencies.iter().any(|value| value.as_str() == Some("reqwest"))
    {
        return Err("submit sim closure is live-capable".to_owned());
    }
    Ok(())
}

#[test]
fn closure_s2_reqwest_red() {
    let original = serde_json::json!({"features": ["arm", "t4e-handoff"], "dependencies": []});
    validate_submit_closure_fixture(&original).expect("sim closure control");
    let mut mutant = original.clone();
    mutant["dependencies"] = serde_json::json!(["reqwest"]);
    assert_ne!(mutant, original, "S2 patch did not change fixture");
    assert!(validate_submit_closure_fixture(&mutant).is_err());
    eprintln!("S2: RED");
}

fn validate_unified_entrypoint(entrypoint: &str, authority: &str) -> Result<(), String> {
    if entrypoint.matches("send_gated(").count() != 1
        || !entrypoint.contains("RuntimeBackend::simulated(&self.backend)")
        || entrypoint.contains("LiveEgressPermit")
        || entrypoint.contains("ProdBackend")
    {
        return Err("entrypoint escaped simulation backend".to_owned());
    }
    if authority.matches("evaluate(PriorityFilterInput").count() != 1
        || !authority.contains("economics: decision")
    {
        return Err("sole economics receipt edge changed".to_owned());
    }
    Ok(())
}

#[test]
fn entrypoint_e0_green_e1_economics_bypass_e2_duplicate_e4_live_permit_red() {
    let arm = manifest_dir().join("src/arm");
    let entrypoint = read(arm.join("simulation_entrypoint.rs"));
    let authority = read(manifest_dir().join("src/tx_authority.rs"));
    validate_unified_entrypoint(&entrypoint, &authority).expect("E0 unified path");
    eprintln!("E0: GREEN");

    let mutant = authority.replacen("economics: decision", "economics: synthetic_receipt", 1);
    assert_ne!(mutant, authority, "E1 patch did not change source");
    assert!(validate_unified_entrypoint(&entrypoint, &mutant).is_err());
    eprintln!("E1: RED");

    let mutant = authority.replacen(
        "evaluate(PriorityFilterInput",
        "evaluate(PriorityFilterInput /* evaluate(PriorityFilterInput */",
        1,
    );
    assert_ne!(mutant, authority, "E2 patch did not change source");
    assert!(validate_unified_entrypoint(&entrypoint, &mutant).is_err());
    eprintln!("E2: RED");

    let mutant = entrypoint.replacen(
        "RuntimeBackend::simulated(&self.backend)",
        "RuntimeBackend::from_explicit_flag(true, &self.backend, &ProdBackend::new().unwrap()) /* LiveEgressPermit */",
        1,
    );
    assert_ne!(mutant, entrypoint, "E4 patch did not change source");
    assert!(validate_unified_entrypoint(&mutant, &authority).is_err());
    eprintln!("E4: RED");
}

fn validate_worker_source(source: &str) -> Result<(), String> {
    for required in [
        "sync_channel(1)",
        ".name(\"base-mev-arm-egress\".to_owned())",
        "sender.try_send(attempt)",
        "while let Ok(attempt) = receiver.recv()",
        "runtime.freshness(&armed)",
    ] {
        if !source.contains(required) {
            return Err(format!("worker bound missing: {required}"));
        }
    }
    if source.matches("std::thread::Builder::new()").count() != 1 {
        return Err("worker thread topology changed".to_owned());
    }
    Ok(())
}

#[test]
fn worker_q0_green_q1_unbounded_q3_per_candidate_thread_red() {
    let source = read(manifest_dir().join("src/arm/simulation_entrypoint.rs"));
    validate_worker_source(&source).expect("Q0 bounded worker");
    eprintln!("Q0: GREEN");

    let mutant = source.replacen("sync_channel(1)", "std::sync::mpsc::channel()", 1);
    assert_ne!(mutant, source, "Q1 patch did not change source");
    assert!(validate_worker_source(&mutant).is_err());
    eprintln!("Q1: RED");

    let mutant = source.replacen(
        "while let Ok(attempt) = receiver.recv()",
        "while let Ok(attempt) = receiver.recv() { let _per_candidate = std::thread::Builder::new();",
        1,
    );
    assert_ne!(mutant, source, "Q3 patch did not change source");
    assert!(validate_worker_source(&mutant).is_err());
    eprintln!("Q3: RED");
}
fn validate_store_source(source: &str) -> Result<(), String> {
    for forbidden in ["std::net", "TcpStream", "UdpSocket", "reqwest", "http://", "https://"] {
        if source.contains(forbidden) {
            return Err(format!("network surface {forbidden}"));
        }
    }
    for required in [
        "SIMULATION_RECORD_CAPACITY: u64 = 262_144",
        "SIMULATION_RECORD_MAX_BYTES: usize = 16 * 1024",
        "file.sync_all()",
        "fs::hard_link",
        "self.directory_handle",
        ".sync_all()",
        "priorRecordHash",
        "correlationKey",
        "expectedEvWei",
    ] {
        if !source.contains(required) {
            return Err(format!("durability/economics edge missing: {required}"));
        }
    }
    Ok(())
}

#[test]
fn persistence_p1_socket_p2_unbounded_p4_no_fsync_p5_no_join_red() {
    let source = read(manifest_dir().join("src/arm/simulation_store.rs"));
    validate_store_source(&source).expect("persistence source control");

    let mutant = format!("use std::net::TcpStream;\n{source}");
    assert_ne!(mutant, source, "P1 patch did not change source");
    assert!(validate_store_source(&mutant).is_err());
    eprintln!("P1: RED");

    let mutant = source.replacen("262_144", "u64::MAX", 1);
    assert_ne!(mutant, source, "P2 patch did not change source");
    assert!(validate_store_source(&mutant).is_err());
    eprintln!("P2: RED");

    let mutant = source.replace("file.sync_all()", "Ok(())");
    assert_ne!(mutant, source, "P4 patch did not change source");
    assert!(validate_store_source(&mutant).is_err());
    eprintln!("P4: RED");

    let mutant = source.replace("\"correlationKey\"", "\"discardedJoinKey\"");
    assert_ne!(mutant, source, "P5 patch did not change source");
    assert!(validate_store_source(&mutant).is_err());
    eprintln!("P5: RED");
}

#[test]
fn unavailable_u0_green_u1_silent_fallback_red() {
    let entrypoint = read(manifest_dir().join("src/arm/simulation_entrypoint.rs"));
    let cli = read(manifest_dir().join("../cli/src/mev_trader.rs"));
    let valid = |entrypoint: &str, cli: &str| {
        entrypoint.contains("ArmRuntimeUnavailable")
            && entrypoint.contains("CommittedStateAuthorityUnavailable")
            && entrypoint.contains("Err(T4eHandoffError::Rejected)")
            && cli.contains("arm simulation entrypoint unavailable; rejecting candidate handoff")
            && cli.contains("status = ?self.config.arm_sim_status")
    };
    assert!(valid(&entrypoint, &cli));
    eprintln!("U0: GREEN");

    let mutant = entrypoint.replacen(
        "Err(T4eHandoffError::Rejected)",
        "Ok(()) /* silent candidate loss */",
        1,
    );
    assert_ne!(mutant, entrypoint, "U1 patch did not change source");
    assert!(!valid(&mutant, &cli));
    eprintln!("U1: RED");
}
