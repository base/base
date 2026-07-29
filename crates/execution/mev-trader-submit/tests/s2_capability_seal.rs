//! S2 simulation-rung, unified-entrypoint, and local-persistence mutation seals.
#![cfg(feature = "arm")]

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

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

fn production_metadata() -> serde_json::Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps", "--offline"])
        .current_dir(manifest_dir().join("../../.."))
        .output()
        .expect("execute offline cargo metadata");
    assert!(
        output.status.success(),
        "offline cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata JSON")
}

fn production_submit_arm_tree() -> String {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--edges",
            "normal",
            "--package",
            "mev-trader-submit",
            "--no-default-features",
            "--features",
            "arm",
            "--offline",
        ])
        .current_dir(manifest_dir().join("../../.."))
        .output()
        .expect("execute offline cargo tree");
    assert!(
        output.status.success(),
        "offline cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("cargo tree UTF-8")
}

fn package<'a>(
    metadata: &'a serde_json::Value,
    name: &str,
) -> Result<&'a serde_json::Value, String> {
    let matches = metadata["packages"]
        .as_array()
        .ok_or_else(|| "metadata packages missing".to_owned())?
        .iter()
        .filter(|package| package["name"].as_str() == Some(name))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [package] => Ok(*package),
        _ => Err(format!("package {name} missing or duplicated")),
    }
}

fn feature_edges(
    metadata: &serde_json::Value,
    package_name: &str,
    feature: &str,
) -> Result<BTreeSet<String>, String> {
    package(metadata, package_name)?["features"][feature]
        .as_array()
        .ok_or_else(|| format!("{package_name}/{feature} missing or not an array"))?
        .iter()
        .map(|edge| {
            edge.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{package_name}/{feature} contains a non-string edge"))
        })
        .collect()
}

fn exact_feature_edges(
    metadata: &serde_json::Value,
    package_name: &str,
    feature: &str,
    expected: &[&str],
) -> Result<(), String> {
    let actual = feature_edges(metadata, package_name, feature)?;
    let expected = expected.iter().map(|edge| (*edge).to_owned()).collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!("{package_name}/{feature} edges differ"));
    }
    Ok(())
}

fn validate_s2_metadata(metadata: &serde_json::Value) -> Result<(), String> {
    exact_feature_edges(
        metadata,
        "mev-trader-submit",
        "arm",
        &["phase-b", "dep:zeroize", "dep:redb", "dep:serde_json", "dep:sha2"],
    )?;
    exact_feature_edges(metadata, "mev-trader-submit", "arm-live-egress", &["arm", "dep:reqwest"])?;
    exact_feature_edges(
        metadata,
        "base-execution-cli",
        "arm-sim",
        &["t4e-handoff", "mev-trader-submit/arm"],
    )?;
    exact_feature_edges(
        metadata,
        "base-execution-cli",
        "arm-live-egress",
        &["arm-sim", "dep:mev-trader-submit", "mev-trader-submit/arm-live-egress"],
    )?;
    exact_feature_edges(metadata, "base-reth-node", "arm-sim", &["base-execution-cli/arm-sim"])?;
    exact_feature_edges(
        metadata,
        "base-reth-node",
        "arm-live-egress",
        &["arm-sim", "base-execution-cli/arm-live-egress"],
    )?;

    let submit = package(metadata, "mev-trader-submit")?;
    let dependencies = submit["dependencies"]
        .as_array()
        .ok_or_else(|| "submit dependencies missing".to_owned())?;
    let actual = dependencies
        .iter()
        .filter(|dependency| dependency["kind"].is_null())
        .map(|dependency| {
            dependency["rename"]
                .as_str()
                .or_else(|| dependency["name"].as_str())
                .ok_or_else(|| "submit dependency name missing".to_owned())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected = BTreeSet::from([
        "alloy-consensus",
        "alloy-eips",
        "alloy-primitives",
        "alloy-sol-types",
        "base-mev-trader",
        "k256",
        "rand_08",
        "redb",
        "reqwest",
        "serde_json",
        "sha2",
        "zeroize",
    ]);
    if actual != expected {
        return Err("submit direct dependency set differs".to_owned());
    }
    let reqwest = dependencies
        .iter()
        .filter(|dependency| dependency["name"].as_str() == Some("reqwest"))
        .collect::<Vec<_>>();
    if !matches!(reqwest.as_slice(), [dependency] if dependency["optional"].as_bool() == Some(true))
    {
        return Err("reqwest must remain one optional live-only dependency".to_owned());
    }
    let features =
        submit["features"].as_object().ok_or_else(|| "submit feature map missing".to_owned())?;
    for (feature, edges) in features {
        let edges =
            edges.as_array().ok_or_else(|| format!("submit/{feature} edges are not an array"))?;
        if feature != "arm-live-egress"
            && edges.iter().any(|edge| edge.as_str() == Some("dep:reqwest"))
        {
            return Err(format!("submit/{feature} gained direct reqwest capability"));
        }
    }
    Ok(())
}

fn validate_submit_arm_tree(tree: &str) -> Result<(), String> {
    if !tree.lines().any(|line| line.starts_with("mev-trader-submit ")) {
        return Err("submit root missing from cargo tree".to_owned());
    }
    let reqwest = tree
        .lines()
        .filter_map(|line| {
            let package = line
                .trim_start_matches(|character: char| {
                    matches!(character, ' ' | '│' | '├' | '└' | '─')
                })
                .strip_prefix("reqwest ")?;
            Some(package)
        })
        .collect::<Vec<_>>();
    if reqwest != ["v0.12.28", "v0.12.28 (*)"] {
        return Err("arm simulation tree reqwest baseline changed".to_owned());
    }
    Ok(())
}

fn validate_live_symbols_gated(source: &str) -> Result<(), String> {
    for symbol in ["LiveEgressPermit", "ProdBackend"] {
        let declaration = format!(
            "#[cfg(all(feature = \"arm-live-egress\", not(test)))]\n#[derive(Debug)]\npub struct {symbol}"
        );
        if source.matches(&declaration).count() != 1 {
            return Err(format!("{symbol} live-only cfg changed"));
        }
    }
    Ok(())
}

#[test]
fn closure_s0_green_unknown_feature_dependency_and_live_tree_red() {
    let metadata = production_metadata();
    let tree = production_submit_arm_tree();
    let transport = read(manifest_dir().join("src/arm/transport.rs"));
    validate_s2_metadata(&metadata).expect("production S2 feature edges");
    validate_submit_arm_tree(&tree).expect("production submit arm dependency closure");
    validate_live_symbols_gated(&transport).expect("live symbols remain cfg-excluded from arm-sim");
    eprintln!("S0: GREEN");

    let mut mutant = metadata.clone();
    package_mut(&mut mutant, "mev-trader-submit")["features"]["arm"]
        .as_array_mut()
        .expect("arm feature")
        .push(serde_json::json!("unknown-feature"));
    assert_ne!(mutant, metadata, "unknown feature mutant did not change metadata");
    assert!(validate_s2_metadata(&mutant).is_err());

    let mut mutant = metadata.clone();
    package_mut(&mut mutant, "mev-trader-submit")["dependencies"]
        .as_array_mut()
        .expect("submit dependencies")
        .push(serde_json::json!({
            "name": "unknown-network",
            "rename": null,
            "kind": null,
            "optional": true
        }));
    assert_ne!(mutant, metadata, "unknown dependency mutant did not change metadata");
    assert!(validate_s2_metadata(&mutant).is_err());
    let mut mutant = metadata.clone();
    package_mut(&mut mutant, "mev-trader-submit")["features"]["phase-b"]
        .as_array_mut()
        .expect("phase-b feature")
        .push(serde_json::json!("dep:reqwest"));
    assert_ne!(mutant, metadata, "non-live reqwest mutant did not change metadata");
    assert!(validate_s2_metadata(&mutant).is_err());

    let mutant = format!("{tree}\nreqwest v999.0.0\n");
    assert_ne!(mutant, tree, "live tree mutant did not change cargo tree");
    assert!(validate_submit_arm_tree(&mutant).is_err());

    let mutant = transport.replacen(
        "#[cfg(all(feature = \"arm-live-egress\", not(test)))]\n#[derive(Debug)]\npub struct ProdBackend",
        "#[derive(Debug)]\npub struct ProdBackend",
        1,
    );
    assert_ne!(mutant, transport, "ProdBackend cfg mutant did not change source");
    assert!(validate_live_symbols_gated(&mutant).is_err());
    eprintln!("S2: RED");
}

fn package_mut<'a>(metadata: &'a mut serde_json::Value, name: &str) -> &'a mut serde_json::Value {
    metadata["packages"]
        .as_array_mut()
        .expect("metadata packages")
        .iter_mut()
        .find(|package| package["name"].as_str() == Some(name))
        .expect("metadata package")
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
        "sender.try_send(AdmittedAttempt { attempt, _reservation: reservation })",
        "while let Ok(admitted) = receiver.recv()",
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
        "while let Ok(admitted) = receiver.recv()",
        "while let Ok(admitted) = receiver.recv() { let _per_candidate = std::thread::Builder::new();",
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
