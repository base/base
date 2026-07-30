//! S2 simulation-rung, unified-entrypoint, and local-persistence mutation seals.
#![cfg(feature = "arm")]

use std::{
    collections::{BTreeMap, BTreeSet},
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
        &[
            "phase-b",
            "dep:redb",
            "dep:serde_json",
            "dep:sha2",
            "dep:tracing",
            "dep:zeroize",
        ],
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
        "tracing",
        "zeroize",
    ]);
    if actual != expected {
        return Err("submit direct dependency set differs".to_owned());
    }
    for (name, owning_feature) in [("reqwest", "arm-live-egress"), ("tracing", "arm")] {
        let matches = dependencies
            .iter()
            .filter(|dependency| dependency["name"].as_str() == Some(name))
            .collect::<Vec<_>>();
        if !matches!(
            matches.as_slice(),
            [dependency] if dependency["optional"].as_bool() == Some(true)
        ) {
            return Err(format!("{name} must remain one optional dependency"));
        }
        let dependency_edge = format!("dep:{name}");
        for (feature, edges) in
            submit["features"].as_object().ok_or_else(|| "submit feature map missing".to_owned())?
        {
            let edges =
                edges.as_array().ok_or_else(|| format!("submit/{feature} edges are not an array"))?;
            if feature != owning_feature
                && edges.iter().any(|edge| edge.as_str() == Some(dependency_edge.as_str()))
            {
                return Err(format!("submit/{feature} gained direct {name} capability"));
            }
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
    let tracing = package_mut(&mut mutant, "mev-trader-submit")["dependencies"]
        .as_array_mut()
        .expect("submit dependencies")
        .iter_mut()
        .find(|dependency| dependency["name"].as_str() == Some("tracing"))
        .expect("tracing dependency");
    tracing["optional"] = serde_json::json!(false);
    assert_ne!(mutant, metadata, "required tracing mutant did not change metadata");
    assert!(validate_s2_metadata(&mutant).is_err());

    let mut mutant = metadata.clone();
    package_mut(&mut mutant, "mev-trader-submit")["features"]["arm-live-egress"]
        .as_array_mut()
        .expect("arm-live-egress feature")
        .push(serde_json::json!("dep:tracing"));
    assert_ne!(mutant, metadata, "live-owned tracing mutant did not change metadata");
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

fn sealed_region<'a>(source: &'a str, start: &str, end: &str) -> Result<&'a str, String> {
    let (_, remainder) =
        source.split_once(start).ok_or_else(|| format!("sealed region start missing: {start}"))?;
    let (region, _) =
        remainder.split_once(end).ok_or_else(|| format!("sealed region end missing: {end}"))?;
    if region.contains(start) {
        return Err(format!("sealed region start duplicated: {start}"));
    }
    Ok(region)
}

fn validate_head_contract(source: &str) -> Result<(), String> {
    let source = source
        .split_once("\n#[cfg(test)]\nmod tests {")
        .map(|(production, _)| production)
        .ok_or_else(|| "H terminal test module marker changed".to_owned())?;
    for required in [
        "struct SimulationLedgerHead {",
        "const ENCODED_LEN: usize = 72;",
        "bytes[..32].copy_from_slice(self.ledger_epoch.as_bytes());",
        "bytes[32..40].copy_from_slice(&self.next_sequence.to_be_bytes());",
        "bytes[40..].copy_from_slice(self.latest_record_hash.as_slice());",
        "let bytes: &[u8; Self::ENCODED_LEN] =",
        "if epoch.iter().all(|byte| *byte == 0)",
        "latest_record_hash: B256::from_slice(&bytes[40..]),",
    ] {
        if !source.contains(required) {
            return Err(format!("H head contract missing: {required}"));
        }
    }
    if source.matches("struct SimulationLedgerHead {").count() != 1
        || source.contains("const EPOCH_FILE")
        || source.contains("join(\"epoch\")")
        || source.contains("to_le_bytes()")
    {
        return Err("H head gained a second authority or non-canonical encoding".to_owned());
    }
    let head = sealed_region(
        source,
        "struct SimulationLedgerHead {",
        "/// Typed failure while opening a ledger.",
    )?;
    for field in ["ledger_epoch:", "next_sequence:", "latest_record_hash:"] {
        if head.matches(field).count() != 2 {
            return Err(format!("H exact head field flow changed: {field}"));
        }
    }
    Ok(())
}

#[test]
fn ledger_head_h0_control_h1_layout_and_epoch_mutants_red() {
    let source = read(manifest_dir().join("src/arm/simulation_store.rs"));
    validate_head_contract(&source).expect("H0 exact 72-byte epoch||BE sequence||hash head");
    eprintln!("H0: GREEN");

    for (label, mutant) in [
        ("length", source.replacen("ENCODED_LEN: usize = 72", "ENCODED_LEN: usize = 40", 1)),
        ("endianness", source.replacen("to_be_bytes()", "to_le_bytes()", 1)),
        (
            "order",
            source.replacen(
                "bytes[..32].copy_from_slice(self.ledger_epoch.as_bytes());",
                "bytes[..32].copy_from_slice(self.latest_record_hash.as_slice());",
                1,
            ),
        ),
        (
            "separate epoch file",
            source.replacen(
                "const HEAD_FILE: &str = \"head\";",
                "const HEAD_FILE: &str = \"head\";\nconst EPOCH_FILE: &str = \"epoch\";",
                1,
            ),
        ),
        (
            "zero epoch",
            source.replacen(
                "if epoch.iter().all(|byte| *byte == 0) {",
                "if false && epoch.iter().all(|byte| *byte == 0) {",
                1,
            ),
        ),
    ] {
        assert_ne!(mutant, source, "H1 {label} mutant did not change input");
        assert!(validate_head_contract(&mutant).is_err(), "H1 {label} mutant survived");
    }
    eprintln!("H1: RED");
}

fn validate_ledger_lifecycle(source: &str) -> Result<(), String> {
    for required in [
        "if is_empty {\n                    return Err(SimulationStoreOpenError::InvalidExistingLedger {",
        "let state = if created {\n            Self::initialize(directory, &directory_handle)?\n        } else {\n            Self::inspect(directory)?\n        };",
        "fs::rename(&open_path, directory.join(HEAD_FILE))",
        "directory_handle\n            .sync_all()",
        "let head = Self::read_head(directory)?;",
        "SimulationLedgerHead::decode(&bytes)",
        "if head.next_sequence != next_sequence || head.latest_record_hash != prior_hash",
    ] {
        if !source.contains(required) {
            return Err(format!("L lifecycle contract missing: {required}"));
        }
    }

    let create = sealed_region(source, "    fn create_directory_with<", "    fn initialize(")?;
    let opened = create
        .find("let parent_handle = open_parent(parent)?;")
        .ok_or_else(|| "L parent directory is not opened fail-closed".to_owned())?;
    let made = create
        .find("create_child(directory)?;")
        .ok_or_else(|| "L child directory creation is not fail-closed".to_owned())?;
    let parent_durable = create
        .find("sync_parent(&parent_handle)\n    }")
        .ok_or_else(|| "L child namespace lacks fail-closed parent fsync".to_owned())?;
    if !(opened < made && made < parent_durable)
        || create.matches("open_parent(parent)?").count() != 1
        || create.matches("create_child(directory)?").count() != 1
        || create.matches("sync_parent(&parent_handle)\n    }").count() != 1
    {
        return Err("L parent namespace durability order changed".to_owned());
    }

    let open = sealed_region(source, "    fn open_directory(", "    fn create_directory(")?;
    let create_call = open
        .find("Self::create_directory(directory)")
        .ok_or_else(|| "L absent ledger no longer creates through durable namespace helper".to_owned())?;
    let child_open = open
        .find("let directory_handle =")
        .ok_or_else(|| "L child directory handle open missing".to_owned())?;
    let initialize_call = open
        .find("Self::initialize(directory, &directory_handle)?")
        .ok_or_else(|| "L child initialization missing".to_owned())?;
    let admission = open
        .find("Ok(Self {")
        .ok_or_else(|| "L store admission missing".to_owned())?;
    if !(create_call < child_open && child_open < initialize_call && initialize_call < admission) {
        return Err("L parent fsync no longer precedes child initialization and admission".to_owned());
    }

    let initialize = sealed_region(source, "    fn initialize(", "    fn inspect(")?;
    if !initialize.contains(
        "file.write_all(&head.encode())\n            .and_then(|()| file.sync_all())",
    ) {
        return Err("L initialized head lacks file fsync".to_owned());
    }
    let durable = initialize
        .find("directory_handle\n            .sync_all()")
        .ok_or_else(|| "L initialized head lacks directory fsync".to_owned())?;
    let admitted = initialize
        .find("Ok((epoch, 0, B256::ZERO))")
        .ok_or_else(|| "L initialized state return missing".to_owned())?;
    if durable >= admitted {
        return Err("L initialized head becomes admissible before durability".to_owned());
    }

    let inspect = sealed_region(source, "    fn inspect_with_capacity(", "    fn read_head(")?;
    let capacity_check = inspect
        .find(
            "if accumulated >= capacity {\n                return Err(invalid(SimulationLedgerInvalid::Sequence));\n            }",
        )
        .ok_or_else(|| "L startup scan capacity check missing or masked".to_owned())?;
    let push = inspect
        .find("sequences.push(sequence);")
        .ok_or_else(|| "L startup sequence collection missing".to_owned())?;
    let sort = inspect
        .find("sequences.sort_unstable();")
        .ok_or_else(|| "L startup sequence sort missing".to_owned())?;
    let read = inspect
        .find("let bytes = fs::read(directory.join(Self::record_name(sequence)))")
        .ok_or_else(|| "L startup record read missing".to_owned())?;
    if !(capacity_check < push && push < sort && sort < read)
        || inspect.matches("if accumulated >= capacity").count() != 1
    {
        return Err("L startup capacity is not checked before push, sort, and record read".to_owned());
    }
    Ok(())
}

#[test]
fn ledger_lifecycle_l0_control_l1_empty_old_layout_tamper_and_rollback_mutants_red() {
    let source = read(manifest_dir().join("src/arm/simulation_store.rs"));
    validate_ledger_lifecycle(&source).expect("L0 durable initialized head and strict reopen");
    eprintln!("L0: GREEN");

    for (label, mutant) in [
        (
            "existing empty",
            source.replacen(
                "if is_empty {\n                    return Err(SimulationStoreOpenError::InvalidExistingLedger {",
                "if false && is_empty {\n                    return Err(SimulationStoreOpenError::InvalidExistingLedger {",
                1,
            ),
        ),
        (
            "old layout",
            source.replacen(
                "SimulationLedgerHead::decode(&bytes)",
                "SimulationLedgerHead::decode(&bytes[..bytes.len().min(72)])",
                1,
            ),
        ),
        (
            "initial head durability",
            source.replacen(
                "file.write_all(&head.encode())\n            .and_then(|()| file.sync_all())",
                "file.write_all(&head.encode())\n            .and_then(|()| Ok(()))",
                1,
            ),
        ),
        (
            "head tamper",
            source.replacen(
                "let head = Self::read_head(directory)?;",
                "let head = Self::read_head(directory).unwrap_or(SimulationLedgerHead { ledger_epoch: SimulationLedgerEpoch([1; 32]), next_sequence: 0, latest_record_hash: B256::ZERO });",
                1,
            ),
        ),
        (
            "rollback",
            source.replacen(
                "if head.next_sequence != next_sequence || head.latest_record_hash != prior_hash",
                "if head.next_sequence > next_sequence || head.latest_record_hash != prior_hash",
                1,
            ),
        ),
        (
            "parent open removal",
            source.replacen(
                "let parent_handle = open_parent(parent)?;",
                "let parent_handle = open_parent(parent).unwrap();",
                1,
            ),
        ),
        (
            "parent open after mkdir",
            source.replacen(
                "let parent_handle = open_parent(parent)?;\n        create_child(directory)?;",
                "create_child(directory)?;\n        let parent_handle = open_parent(parent)?;",
                1,
            ),
        ),
        (
            "parent fsync failure bypass",
            source.replacen(
                "sync_parent(&parent_handle)",
                "sync_parent(&parent_handle).or(Ok(()))",
                1,
            ),
        ),
        (
            "parent fsync after initialization",
            source.replacen(
                "Self::create_directory(directory)\n                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;",
                "fs::DirBuilder::new().mode(0o700).create(directory)\n                    .map_err(|error| SimulationStoreOpenError::Io(error.kind()))?;",
                1,
            ),
        ),
        (
            "startup capacity masking",
            source.replacen(
                "if accumulated >= capacity {",
                "if false && accumulated >= capacity {",
                1,
            ),
        ),
        (
            "startup capacity removal",
            source.replacen(
                "if accumulated >= capacity {\n                return Err(invalid(SimulationLedgerInvalid::Sequence));\n            }",
                "",
                1,
            ),
        ),
    ] {
        assert_ne!(mutant, source, "L1 {label} mutant did not change input");
        assert!(validate_ledger_lifecycle(&mutant).is_err(), "L1 {label} mutant survived");
    }
    eprintln!("L1: RED");
}

fn validate_correlation_envelope(store: &str, entrypoint: &str, lib: &str) -> Result<(), String> {
    for required in [
        "pub struct SimulationCorrelationEnvelopeV1 {\n    ledger_epoch: SimulationLedgerEpoch,\n    sequence: u64,\n    correlation_key: SimulationCorrelationKey,\n}",
        "pub const fn ledger_epoch(&self) -> SimulationLedgerEpoch",
        "pub const fn sequence(&self) -> u64",
        "pub const fn correlation_key(&self) -> SimulationCorrelationKey",
        "correlation: SimulationCorrelationEnvelopeV1,",
        "\"correlation\": {\n                \"correlationKey\":",
        "\"ledgerEpoch\": Self::hex_epoch(correlation.ledger_epoch()),",
        "\"ledgerEpoch\": Self::hex_epoch(self.epoch),",
        "Self::validate_existing(&value, epoch, sequence, prior_hash)?;",
    ] {
        if !store.contains(required) {
            return Err(format!("X0 durable correlation envelope edge missing: {required}"));
        }
    }

    let validate =
        sealed_region(store, "    fn validate_existing(", "    fn exact_object<'a>(")?;
    for required in [
        "if !Self::hash_field(object, \"ledgerEpoch\", false)\n            || object.get(\"ledgerEpoch\").and_then(Value::as_str)\n                != Some(Self::hex_epoch(epoch).as_str())\n        {\n            return Err(invalid_epoch());\n        }",
        "if !Self::has_exact_keys(object, &top_keys)\n            || object.get(\"version\").and_then(Value::as_u64) != Some(VERSION)\n            || object.get(\"sequence\").and_then(Value::as_u64) != Some(sequence)\n        {\n            return Err(invalid());\n        }",
        "if !Self::hash_field(correlation, \"ledgerEpoch\", false)\n            || correlation.get(\"ledgerEpoch\") != object.get(\"ledgerEpoch\")\n        {\n            return Err(invalid_epoch());\n        }",
        "if correlation.get(\"sequence\").and_then(Value::as_u64) != Some(sequence)\n            || !Self::hash_field(correlation, \"correlationKey\", false)\n            || correlation.get(\"correlationKey\") != object.get(\"correlationKey\")\n        {\n            return Err(invalid());\n        }",
    ] {
        if !validate.contains(required) {
            return Err(format!("X0 exact envelope validation edge missing: {required}"));
        }
    }
    for masking in [
        "false &&",
        "true ||",
        "return Ok(())",
        ".unwrap(",
        ".unwrap_or",
        "Default::default",
        "::default()",
    ] {
        if validate.contains(masking) {
            return Err(format!("X0 envelope validation contains masking path: {masking}"));
        }
    }

    for required in [
        "Persisted {\n        /// Complete bounded join identity for the durable record.\n        correlation: SimulationCorrelationEnvelopeV1,",
        "correlation: *persisted.correlation(),",
    ] {
        if !entrypoint.contains(required) {
            return Err(format!("X1 terminal correlation envelope edge missing: {required}"));
        }
    }
    if !lib.contains(
        "SimulationCorrelationEnvelopeV1, SimulationCorrelationKey, SimulationEntrypointStatus,",
    ) {
        return Err("X1 public correlation envelope export missing".to_owned());
    }
    Ok(())
}

fn validate_correlation_recomputation(source: &str) -> Result<(), String> {
    let recompute = sealed_region(
        source,
        "    fn recompute_correlation_key(",
        "    fn hex_address(",
    )?;
    for required in [
        "campaign: B256",
        "victim: B256",
        "plan: B256",
        "signed: B256",
        "base-mev/simulation-correlation/v1",
    ] {
        if !recompute.contains(required) {
            return Err(format!("X1 correlation recomputation missing: {required}"));
        }
    }
    if recompute.contains("epoch") || recompute.contains("ledger") {
        return Err("X1 stable correlation key became epoch-dependent".to_owned());
    }
    Ok(())
}

fn validate_sticky_closure(entrypoint: &str) -> Result<(), String> {
    let closure = sealed_region(
        entrypoint,
        "pub enum SimulationLedgerClosure {",
        "impl TryFrom<SimulationPersistError> for SimulationLedgerClosure",
    )?;
    for reason in ["Full {", "PersistenceFailed {", "InvalidExistingLedger {"] {
        if closure.matches(reason).count() != 1 {
            return Err(format!("X2 sticky reason cardinality changed: {reason}"));
        }
    }
    if entrypoint.matches("tracing::error!(").count() != 3
        || entrypoint.matches("\"simulation ledger closed\"").count() != 3
    {
        return Err("X2 structured closure reason emission changed".to_owned());
    }

    let setter = sealed_region(entrypoint, "    fn set_ledger_closed(", "    fn close_worker(")?;
    if entrypoint.matches(".emit();").count() != 1
        || setter.matches(".emit();").count() != 1
        || setter.matches("SimulationEntrypointStatus::Ready =>").count() != 1
    {
        return Err("X2 set_ledger_closed is not the sole emit authority".to_owned());
    }
    for required in [
        "closure_reason = \"Full\"",
        "closure_reason = \"PersistenceFailed\"",
        "closure_reason = \"InvalidExistingLedger\"",
        "ledger_epoch = ?",
        "next_sequence,",
        "capacity,",
        "operation = ?operation,",
        "io_kind = ?io_kind,",
        "class = ?class,",
        "SimulationEntrypointStatus::LedgerClosed(reason) => reason,",
        "SimulationEntrypointStatus::Ready => {\n                *status = SimulationEntrypointStatus::LedgerClosed(proposed);\n                proposed.emit();\n                proposed\n            }",
    ] {
        if !entrypoint.contains(required) {
            return Err(format!("X2 sticky/bounded evidence edge missing: {required}"));
        }
    }
    Ok(())
}

#[test]
fn correlation_and_closure_x0_x1_x2_x3_controls_and_epoch_mutants_red() {
    let store = read(manifest_dir().join("src/arm/simulation_store.rs"));
    let entrypoint = read(manifest_dir().join("src/arm/simulation_entrypoint.rs"));
    let lib = read(manifest_dir().join("src/lib.rs"));

    validate_correlation_envelope(&store, &entrypoint, &lib).expect("X0 immutable durable envelope");
    eprintln!("X0: GREEN");
    validate_correlation_recomputation(&store).expect("X1 stable key excludes epoch");
    eprintln!("X1: GREEN");
    validate_sticky_closure(&entrypoint).expect("X2 exact first-wins structured closure");
    eprintln!("X2: GREEN");
    validate_head_contract(&store).expect("X3 required nonzero epoch authority");
    eprintln!("X3: GREEN");

    let mutant = store.replacen(
        "\"ledgerEpoch\": Self::hex_epoch(self.epoch),",
        "\"ledgerGeneration\": Self::hex_epoch(self.epoch),",
        1,
    );
    assert_ne!(mutant, store, "X0 epoch omission mutant did not change input");
    assert!(validate_correlation_envelope(&mutant, &entrypoint, &lib).is_err());

    let mutant = store.replacen(
        "ledger_epoch: SimulationLedgerEpoch,\n    sequence: u64,",
        "ledger_epoch: Option<SimulationLedgerEpoch>,\n    sequence: u64,",
        1,
    );
    assert_ne!(mutant, store, "X0 epoch optionality mutant did not change input");
    assert!(validate_correlation_envelope(&mutant, &entrypoint, &lib).is_err());

    let mutant = store.replacen(
        "if epoch.iter().all(|byte| *byte == 0) {",
        "if false && epoch.iter().all(|byte| *byte == 0) {",
        1,
    );
    assert_ne!(mutant, store, "X3 zero epoch mutant did not change input");
    assert!(validate_head_contract(&mutant).is_err());

    let mutant = store.replacen(
        "correlation.get(\"ledgerEpoch\") != object.get(\"ledgerEpoch\")",
        "correlation.get(\"ledgerEpoch\") == object.get(\"ledgerEpoch\")",
        1,
    );
    assert_ne!(mutant, store, "X0 epoch mismatch mutant did not change input");
    assert!(validate_correlation_envelope(&mutant, &entrypoint, &lib).is_err());

    let mutant = store.replacen(
        "signed: B256,\n    ) -> B256 {",
        "signed: B256,\n        epoch: SimulationLedgerEpoch,\n    ) -> B256 {",
        1,
    );
    assert_ne!(mutant, store, "X1 epoch-key mutant did not change input");
    assert!(validate_correlation_recomputation(&mutant).is_err());

    let mutant = entrypoint.replacen(
        "SimulationEntrypointStatus::LedgerClosed(reason) => reason,",
        "SimulationEntrypointStatus::LedgerClosed(_) => proposed,",
        1,
    );
    assert_ne!(mutant, entrypoint, "X2 last-reason-wins mutant did not change input");
    assert!(validate_sticky_closure(&mutant).is_err());
    let mutant = entrypoint.replacen(
        "proposed.emit();",
        "if false { proposed.emit(); }",
        1,
    );
    assert_ne!(mutant, entrypoint, "X2 masked sole-emit mutant did not change input");
    assert!(validate_sticky_closure(&mutant).is_err());

    let mutant = entrypoint.replacen(
        "proposed.emit();",
        "proposed.emit();\n                proposed.emit();",
        1,
    );
    assert_ne!(mutant, entrypoint, "X2 duplicate emit mutant did not change input");
    assert!(validate_sticky_closure(&mutant).is_err());

    for (label, mutant) in [
        (
            "false-and epoch equality",
            store.replacen(
                "if !Self::hash_field(object, \"ledgerEpoch\", false)",
                "if false && !Self::hash_field(object, \"ledgerEpoch\", false)",
                1,
            ),
        ),
        (
            "true-or correlation equality",
            store.replacen(
                "if !Self::hash_field(correlation, \"ledgerEpoch\", false)",
                "if true || !Self::hash_field(correlation, \"ledgerEpoch\", false)",
                1,
            ),
        ),
        (
            "early success",
            store.replacen(
                "    ) -> Result<(), SimulationStoreOpenError> {\n        let invalid =",
                "    ) -> Result<(), SimulationStoreOpenError> {\n        return Ok(());\n        let invalid =",
                1,
            ),
        ),
        (
            "unwrap default object",
            store.replacen(
                "let object = value.as_object().ok_or_else(invalid)?;",
                "let object = value.as_object().unwrap_or_default();",
                1,
            ),
        ),
        (
            "alternate epoch path",
            store.replacen(
                "correlation.get(\"ledgerEpoch\") != object.get(\"ledgerEpoch\")",
                "correlation.get(\"ledgerEpoch\") != value.get(\"ledgerEpoch\")",
                1,
            ),
        ),
    ] {
        assert_ne!(mutant, store, "X0 {label} masking mutant did not change input");
        assert!(
            validate_correlation_envelope(&mutant, &entrypoint, &lib).is_err(),
            "X0 {label} masking mutant survived"
        );
    }

    let mutant = store.replacen(
        "if head.next_sequence != next_sequence || head.latest_record_hash != prior_hash",
        "if head.next_sequence > next_sequence || head.latest_record_hash != prior_hash",
        1,
    );
    assert_ne!(mutant, store, "X3 rollback mutant did not change input");
    assert!(validate_ledger_lifecycle(&mutant).is_err());
    eprintln!("X0/X1/X2/X3 mutants: RED");
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

fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && matches!(
                &attr.meta,
                syn::Meta::List(list) if list.tokens.to_string() == "test"
            )
    })
}

fn item_has_cfg_test(item: &syn::Item) -> bool {
    let attrs = match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        syn::Item::Verbatim(_) => return false,
        _ => return false,
    };
    has_cfg_test(attrs)
}

fn impl_item_has_cfg_test(item: &syn::ImplItem) -> bool {
    let attrs: &[syn::Attribute] = match item {
        syn::ImplItem::Const(item) => &item.attrs,
        syn::ImplItem::Fn(item) => &item.attrs,
        syn::ImplItem::Type(item) => &item.attrs,
        syn::ImplItem::Macro(item) => &item.attrs,
        _ => &[],
    };
    has_cfg_test(attrs)
}

fn trait_item_has_cfg_test(item: &syn::TraitItem) -> bool {
    let attrs: &[syn::Attribute] = match item {
        syn::TraitItem::Const(item) => &item.attrs,
        syn::TraitItem::Fn(item) => &item.attrs,
        syn::TraitItem::Type(item) => &item.attrs,
        syn::TraitItem::Macro(item) => &item.attrs,
        _ => &[],
    };
    has_cfg_test(attrs)
}

fn path_ends_with(path: &syn::Path, suffix: &[&str]) -> bool {
    path.segments.len() >= suffix.len()
        && path
            .segments
            .iter()
            .rev()
            .zip(suffix.iter().rev())
            .all(|(segment, expected)| segment.ident == *expected)
}

fn known_macro_without_sealed_symbols(mac: &syn::Macro) -> bool {
    let name = mac.path.segments.last().map(|segment| segment.ident.to_string());
    let known = name.as_deref().is_some_and(|name| {
        matches!(
            name,
            "assert"
                | "assert_eq"
                | "assert_ne"
                | "bail"
                | "concat"
                | "cfg"
                | "debug"
                | "env"
                | "error"
                | "eyre"
                | "format"
                | "include_bytes"
                | "include_str"
                | "info"
                | "json"
                | "matches"
                | "macro_rules"
                | "panic"
                | "select"
                | "unreachable"
                | "vec"
                | "write"
        )
    });
    let tokens = mac.tokens.to_string();
    known
        && !tokens
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|token| {
                matches!(
                    token,
                    "ArmRuntime"
                        | "SimulationEntrypoint"
                        | "SimulationWorker"
                        | "SealedUnsignedCandidate"
                        | "T4eCandidateHandoff"
                        | "T4eHandoffError"
                        | "UnavailableSimulationHandoff"
                )
            })
}

const PROTECTED_PRODUCTION_TYPES: [&str; 5] = [
    "T4eCandidateHandoff",
    "ArmRuntime",
    "SimulationEntrypoint",
    "SimulationWorker",
    "SealedUnsignedCandidate",
];

struct AliasBinding {
    alias: String,
    target: Vec<String>,
}

#[derive(Default)]
struct ProductionAliasCollector {
    bindings: Vec<AliasBinding>,
    opaque_use_globs: usize,
}

impl ProductionAliasCollector {
    fn collect_use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.collect_use_tree(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(name) => {
                prefix.push(name.ident.to_string());
                self.bindings.push(AliasBinding {
                    alias: name.ident.to_string(),
                    target: prefix.clone(),
                });
                prefix.pop();
            }
            syn::UseTree::Rename(rename) => {
                prefix.push(rename.ident.to_string());
                self.bindings.push(AliasBinding {
                    alias: rename.rename.to_string(),
                    target: prefix.clone(),
                });
                prefix.pop();
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.collect_use_tree(item, prefix);
                }
            }
            syn::UseTree::Glob(_) => self.opaque_use_globs += 1,
        }
    }

    fn collect_type_binding(&mut self, alias: &syn::Ident, ty: &syn::Type) {
        if let syn::Type::Path(ty) = ty
            && ty.qself.is_none()
        {
            self.bindings.push(AliasBinding {
                alias: alias.to_string(),
                target: ty.path.segments.iter().map(|segment| segment.ident.to_string()).collect(),
            });
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for ProductionAliasCollector {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !item_has_cfg_test(item) {
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.collect_use_tree(&item.tree, &mut Vec::new());
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        self.collect_type_binding(&item.ident, &item.ty);
    }

    fn visit_item_trait_alias(&mut self, item: &'ast syn::ItemTraitAlias) {
        for bound in &item.bounds {
            if let syn::TypeParamBound::Trait(bound) = bound {
                self.bindings.push(AliasBinding {
                    alias: item.ident.to_string(),
                    target: bound.path.segments.iter().map(|segment| segment.ident.to_string()).collect(),
                });
            }
        }
    }
}

struct ProductionAliases {
    types: BTreeMap<String, BTreeSet<String>>,
    runtime_open: BTreeSet<String>,
    entrypoint_ready: BTreeSet<String>,
    worker_spawn: BTreeSet<String>,
    opaque_use_globs: usize,
}

impl ProductionAliases {
    fn resolve(collector: ProductionAliasCollector) -> Self {
        let mut types = BTreeMap::new();
        for symbol in PROTECTED_PRODUCTION_TYPES {
            types.insert(symbol.to_owned(), BTreeSet::from([symbol.to_owned()]));
        }
        let mut runtime_open = BTreeSet::new();
        let mut entrypoint_ready = BTreeSet::new();
        let mut worker_spawn = BTreeSet::new();

        loop {
            let mut changed = false;
            for binding in &collector.bindings {
                let matching_types: Vec<String> = types
                    .iter()
                    .filter(|(_, aliases)| {
                        binding.target.last().is_some_and(|target| aliases.contains(target))
                    })
                    .map(|(symbol, _)| symbol.clone())
                    .collect();
                for symbol in matching_types {
                    changed |= types
                        .get_mut(&symbol)
                        .expect("protected production type")
                        .insert(binding.alias.clone());
                }

                let target = binding.target.last().map(String::as_str);
                let owner = binding.target.iter().nth_back(1).map(String::as_str);
                let is_function_alias = |aliases: &BTreeSet<String>,
                                         owner_aliases: &BTreeSet<String>,
                                         member: &str| {
                    target.is_some_and(|target| aliases.contains(target))
                        || (target == Some(member)
                            && owner.is_some_and(|owner| owner_aliases.contains(owner)))
                };
                if is_function_alias(
                    &runtime_open,
                    &types["ArmRuntime"],
                    "open",
                ) {
                    changed |= runtime_open.insert(binding.alias.clone());
                }
                if is_function_alias(
                    &entrypoint_ready,
                    &types["SimulationEntrypoint"],
                    "ready",
                ) {
                    changed |= entrypoint_ready.insert(binding.alias.clone());
                }
                if is_function_alias(
                    &worker_spawn,
                    &types["SimulationWorker"],
                    "spawn",
                ) {
                    changed |= worker_spawn.insert(binding.alias.clone());
                }
            }
            if !changed {
                break;
            }
        }

        Self {
            types,
            runtime_open,
            entrypoint_ready,
            worker_spawn,
            opaque_use_globs: collector.opaque_use_globs,
        }
    }

    fn type_aliases(&self, symbol: &str) -> &BTreeSet<String> {
        &self.types[symbol]
    }
}

fn type_path_is_alias(ty: &syn::Type, aliases: &BTreeSet<String>) -> bool {
    matches!(
        ty,
        syn::Type::Path(ty)
            if ty.path.segments.last().is_some_and(|segment| aliases.contains(&segment.ident.to_string()))
    )
}

fn expression_references_associated(
    expression: &syn::ExprPath,
    owner_aliases: &BTreeSet<String>,
    member_aliases: &BTreeSet<String>,
    member: &str,
) -> bool {
    let segments = &expression.path.segments;
    let direct_alias = segments.len() == 1
        && segments.last().is_some_and(|segment| member_aliases.contains(&segment.ident.to_string()));
    let associated = segments.last().is_some_and(|segment| segment.ident == member)
        && (segments.iter().nth_back(1).is_some_and(|segment| {
            owner_aliases.contains(&segment.ident.to_string())
        }) || expression
            .qself
            .as_ref()
            .is_some_and(|qself| type_path_is_alias(&qself.ty, owner_aliases)));
    direct_alias || associated
}

#[derive(Default)]
struct HandoffBodyCardinality {
    rejected: usize,
    busy: usize,
    closed: usize,
    ok: usize,
    err: usize,
    rejected_err: usize,
}

impl<'ast> syn::visit::Visit<'ast> for HandoffBodyCardinality {
    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if path_ends_with(&expression.path, &["T4eHandoffError", "Rejected"]) {
            self.rejected += 1;
        }
        if path_ends_with(&expression.path, &["T4eHandoffError", "Busy"]) {
            self.busy += 1;
        }
        if path_ends_with(&expression.path, &["T4eHandoffError", "Closed"]) {
            self.closed += 1;
        }
        syn::visit::visit_expr_path(self, expression);
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if let syn::Expr::Path(function) = expression.func.as_ref() {
            if path_ends_with(&function.path, &["Ok"]) {
                self.ok += 1;
            }
            if path_ends_with(&function.path, &["Err"]) {
                self.err += 1;
                if expression.args.len() == 1
                    && matches!(
                        expression.args.first(),
                        Some(syn::Expr::Path(argument))
                            if path_ends_with(
                                &argument.path,
                                &["T4eHandoffError", "Rejected"],
                            )
                    )
                {
                    self.rejected_err += 1;
                }
            }
        }
        syn::visit::visit_expr_call(self, expression);
    }
}

struct CandidateFieldVisitor<'a> {
    candidate_aliases: &'a BTreeSet<String>,
    retains_candidate: bool,
}

impl<'ast> syn::visit::Visit<'ast> for CandidateFieldVisitor<'_> {
    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        if ty
            .path
            .segments
            .last()
            .is_some_and(|segment| self.candidate_aliases.contains(&segment.ident.to_string()))
        {
            self.retains_candidate = true;
        }
        syn::visit::visit_type_path(self, ty);
    }
}

struct ProductionHandoffVisitor {
    aliases: ProductionAliases,
    handoff_impls: usize,
    handoff_impl_target_mismatches: usize,
    try_handoff_methods: usize,
    handoff_body: HandoffBodyCardinality,
    candidate_fields: usize,
    worker_spawn_calls: usize,
    entrypoint_ready_calls: usize,
    entrypoint_ready_calls_in_worker_spawn: usize,
    runtime_open_calls: usize,
    opaque_production_items: usize,
    function_body_depth: usize,
    in_worker_impl: bool,
    in_worker_spawn: bool,
}

impl ProductionHandoffVisitor {
    fn new(aliases: ProductionAliases) -> Self {
        Self {
            aliases,
            handoff_impls: 0,
            handoff_impl_target_mismatches: 0,
            try_handoff_methods: 0,
            handoff_body: HandoffBodyCardinality::default(),
            candidate_fields: 0,
            worker_spawn_calls: 0,
            entrypoint_ready_calls: 0,
            entrypoint_ready_calls_in_worker_spawn: 0,
            runtime_open_calls: 0,
            opaque_production_items: 0,
            function_body_depth: 0,
            in_worker_impl: false,
            in_worker_spawn: false,
        }
    }

    fn record_candidate_type(&mut self, ty: &syn::Type) {
        let mut field_visitor = CandidateFieldVisitor {
            candidate_aliases: self.aliases.type_aliases("SealedUnsignedCandidate"),
            retains_candidate: false,
        };
        syn::visit::Visit::visit_type(&mut field_visitor, ty);
        if field_visitor.retains_candidate {
            self.candidate_fields += 1;
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for ProductionHandoffVisitor {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if item_has_cfg_test(item) {
            return;
        }
        match item {
            syn::Item::Macro(item) => {
                if !known_macro_without_sealed_symbols(&item.mac) {
                    self.opaque_production_items += 1;
                }
            }
            syn::Item::Verbatim(_) => self.opaque_production_items += 1,
            _ => syn::visit::visit_item(self, item),
        }
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        for field in &item.fields {
            if !has_cfg_test(&field.attrs) {
                self.record_candidate_type(&field.ty);
            }
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        for variant in &item.variants {
            if has_cfg_test(&variant.attrs) {
                continue;
            }
            for field in &variant.fields {
                if !has_cfg_test(&field.attrs) {
                    self.record_candidate_type(&field.ty);
                }
            }
        }
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        for field in &item.fields.named {
            if !has_cfg_test(&field.attrs) {
                self.record_candidate_type(&field.ty);
            }
        }
        syn::visit::visit_item_union(self, item);
    }

    fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
        self.record_candidate_type(&item.ty);
        syn::visit::visit_item_static(self, item);
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        self.record_candidate_type(&item.ty);
        syn::visit::visit_item_const(self, item);
    }
    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if matches!(item.ty.as_ref(), syn::Type::Path(ty) if ty.qself.is_some()) {
            self.opaque_production_items += 1;
            return;
        }
        syn::visit::visit_item_type(self, item);
    }


    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let self_is_unavailable = matches!(
            item.self_ty.as_ref(),
            syn::Type::Path(ty) if path_ends_with(&ty.path, &["UnavailableSimulationHandoff"])
        );
        let is_handoff_impl = item.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.segments.last().is_some_and(|segment| {
                self.aliases
                    .type_aliases("T4eCandidateHandoff")
                    .contains(&segment.ident.to_string())
            })
        });
        if is_handoff_impl {
            self.handoff_impls += 1;
            if !self_is_unavailable {
                self.handoff_impl_target_mismatches += 1;
            }
            for impl_item in &item.items {
                if let syn::ImplItem::Fn(method) = impl_item
                    && !has_cfg_test(&method.attrs)
                    && method.sig.ident == "try_handoff"
                {
                    self.try_handoff_methods += 1;
                    syn::visit::Visit::visit_block(&mut self.handoff_body, &method.block);
                }
            }
        }

        let old_in_worker_impl = self.in_worker_impl;
        self.in_worker_impl = type_path_is_alias(
            item.self_ty.as_ref(),
            self.aliases.type_aliases("SimulationWorker"),
        );
        syn::visit::visit_item_impl(self, item);
        self.in_worker_impl = old_in_worker_impl;
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if impl_item_has_cfg_test(item) {
            return;
        }
        match item {
            syn::ImplItem::Macro(item) => {
                if !known_macro_without_sealed_symbols(&item.mac) {
                    self.opaque_production_items += 1;
                }
            }
            syn::ImplItem::Verbatim(_) => self.opaque_production_items += 1,
            _ => syn::visit::visit_impl_item(self, item),
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        if trait_item_has_cfg_test(item) {
            return;
        }
        match item {
            syn::TraitItem::Macro(item) => {
                if !known_macro_without_sealed_symbols(&item.mac) {
                    self.opaque_production_items += 1;
                }
            }
            syn::TraitItem::Verbatim(_) => self.opaque_production_items += 1,
            _ => syn::visit::visit_trait_item(self, item),
        }
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.function_body_depth += 1;
        syn::visit::visit_item_fn(self, function);
        self.function_body_depth -= 1;
    }

    fn visit_impl_item_fn(&mut self, method: &'ast syn::ImplItemFn) {
        let old_in_worker_spawn = self.in_worker_spawn;
        self.in_worker_spawn = self.in_worker_impl && method.sig.ident == "spawn";
        self.function_body_depth += 1;
        syn::visit::visit_impl_item_fn(self, method);
        self.function_body_depth -= 1;
        self.in_worker_spawn = old_in_worker_spawn;
    }

    fn visit_trait_item_fn(&mut self, method: &'ast syn::TraitItemFn) {
        self.function_body_depth += 1;
        syn::visit::visit_trait_item_fn(self, method);
        self.function_body_depth -= 1;
    }

    fn visit_expr_macro(&mut self, expression: &'ast syn::ExprMacro) {
        if self.function_body_depth != 0 && !known_macro_without_sealed_symbols(&expression.mac) {
            self.opaque_production_items += 1;
            return;
        }
        syn::visit::visit_expr_macro(self, expression);
    }

    fn visit_stmt_macro(&mut self, statement: &'ast syn::StmtMacro) {
        if self.function_body_depth != 0 && !known_macro_without_sealed_symbols(&statement.mac) {
            self.opaque_production_items += 1;
            return;
        }
        syn::visit::visit_stmt_macro(self, statement);
    }


    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression_references_associated(
            expression,
            self.aliases.type_aliases("SimulationWorker"),
            &self.aliases.worker_spawn,
            "spawn",
        ) {
            self.worker_spawn_calls += 1;
        }
        if expression_references_associated(
            expression,
            self.aliases.type_aliases("SimulationEntrypoint"),
            &self.aliases.entrypoint_ready,
            "ready",
        ) {
            self.entrypoint_ready_calls += 1;
            if self.in_worker_spawn {
                self.entrypoint_ready_calls_in_worker_spawn += 1;
            }
        }
        if expression_references_associated(
            expression,
            self.aliases.type_aliases("ArmRuntime"),
            &self.aliases.runtime_open,
            "open",
        ) {
            self.runtime_open_calls += 1;
        }
        syn::visit::visit_expr_path(self, expression);
    }
}

fn analyze_production_handoff(
    source: &str,
    label: &str,
) -> Result<ProductionHandoffVisitor, String> {
    use syn::visit::Visit;

    let file =
        syn::parse_file(source).map_err(|error| format!("{label} did not parse: {error}"))?;
    let mut alias_collector = ProductionAliasCollector::default();
    alias_collector.visit_file(&file);
    let aliases = ProductionAliases::resolve(alias_collector);
    let mut visitor = ProductionHandoffVisitor::new(aliases);
    visitor.visit_file(&file);
    Ok(visitor)
}

fn insert_before_test_module(source: &str, addition: &str) -> String {
    source.replacen(
        "#[cfg(test)]\nmod tests {",
        &format!("{addition}\n\n#[cfg(test)]\nmod tests {{"),
        1,
    )
}

fn validate_deferred_production_handoff(entrypoint: &str, cli: &str) -> Result<(), String> {
    const FOLLOW_UP: &str = "Production T4e Simulation Installation + Settled-Loss Authority";

    for required in [
        "ProductionInstallationDeferred",
        "pub const fn deferred_production() -> Self",
        "Self::new(SimulationEntrypointUnavailable::ProductionInstallationDeferred)",
        "real settled-loss authority",
        "proofs/claim-store/custody",
        "shared bridge",
        "PR #55 committed-state dependency",
        "Err(T4eHandoffError::Rejected)",
        FOLLOW_UP,
    ] {
        if !entrypoint.contains(required) {
            return Err(format!("deferred rejecting entrypoint edge missing: {required}"));
        }
    }
    if entrypoint.contains("probe_production") || cli.contains("probe_production") {
        return Err(
            "deferred installation performed a production probe: probe_production".to_owned()
        );
    }
    for required in [
        "UnavailableSimulationHandoff::deferred_production()",
        "t4e_handoff: Some(arm_sim_handoff.into_handoff())",
        "status = ?self.config.arm_sim_status",
        "arm simulation production installation deferred; rejecting candidate handoff",
        FOLLOW_UP,
    ] {
        if !cli.contains(required) {
            return Err(format!("CLI deferred installation edge missing: {required}"));
        }
    }
    if cli.matches("t4e_handoff: Some(arm_sim_handoff.into_handoff())").count() != 1 {
        return Err("CLI production handoff is not the sole deferred rejecting sink".to_owned());
    }

    let entrypoint_analysis = analyze_production_handoff(entrypoint, "simulation entrypoint")?;
    let cli_analysis = analyze_production_handoff(cli, "CLI")?;
    let opaque_production_items =
        entrypoint_analysis.opaque_production_items + cli_analysis.opaque_production_items;
    let opaque_use_globs =
        entrypoint_analysis.aliases.opaque_use_globs + cli_analysis.aliases.opaque_use_globs;
    if opaque_production_items != 0 || opaque_use_globs != 0 {
        return Err(format!(
            "production contains syntax the deferral seal cannot resolve: opaque items/body macros/associated types={opaque_production_items}, use globs={opaque_use_globs}",
        ));
    }
    let handoff_impls = entrypoint_analysis.handoff_impls + cli_analysis.handoff_impls;
    let handoff_impl_target_mismatches = entrypoint_analysis.handoff_impl_target_mismatches
        + cli_analysis.handoff_impl_target_mismatches;
    let try_handoff_methods =
        entrypoint_analysis.try_handoff_methods + cli_analysis.try_handoff_methods;
    if handoff_impls != 1 || handoff_impl_target_mismatches != 0 || try_handoff_methods != 1 {
        return Err(format!(
            "production handoff implementation cardinality changed: impls={handoff_impls}, wrong_targets={handoff_impl_target_mismatches}, try_handoff_methods={try_handoff_methods}",
        ));
    }
    let rejected = entrypoint_analysis.handoff_body.rejected + cli_analysis.handoff_body.rejected;
    let busy = entrypoint_analysis.handoff_body.busy + cli_analysis.handoff_body.busy;
    let closed = entrypoint_analysis.handoff_body.closed + cli_analysis.handoff_body.closed;
    let ok = entrypoint_analysis.handoff_body.ok + cli_analysis.handoff_body.ok;
    let err = entrypoint_analysis.handoff_body.err + cli_analysis.handoff_body.err;
    let rejected_err =
        entrypoint_analysis.handoff_body.rejected_err + cli_analysis.handoff_body.rejected_err;
    if rejected != 1 || busy != 0 || closed != 0 || ok != 0 || err != 1 || rejected_err != 1 {
        return Err(format!(
            "deferred handoff is not Rejected-only: Rejected={rejected}, Busy={busy}, Closed={closed}, Ok={ok}, Err={err}, Err(Rejected)={rejected_err}",
        ));
    }
    if entrypoint_analysis.candidate_fields != 0 || cli_analysis.candidate_fields != 1 {
        return Err(format!(
            "candidate ownership cardinality changed: deferred entrypoint fields={}, sole T4d shadow slot fields={}",
            entrypoint_analysis.candidate_fields, cli_analysis.candidate_fields,
        ));
    }
    let worker_spawn_calls =
        entrypoint_analysis.worker_spawn_calls + cli_analysis.worker_spawn_calls;
    if worker_spawn_calls != 0 {
        return Err(format!(
            "production contains {worker_spawn_calls} SimulationWorker::spawn caller(s)"
        ));
    }
    let ready_calls =
        entrypoint_analysis.entrypoint_ready_calls + cli_analysis.entrypoint_ready_calls;
    if ready_calls != 1 || entrypoint_analysis.entrypoint_ready_calls_in_worker_spawn != 1 {
        return Err(format!(
            "SimulationEntrypoint::ready cardinality/context changed: total={ready_calls}, inside SimulationWorker::spawn={}",
            entrypoint_analysis.entrypoint_ready_calls_in_worker_spawn,
        ));
    }
    let runtime_open_calls =
        entrypoint_analysis.runtime_open_calls + cli_analysis.runtime_open_calls;
    if runtime_open_calls != 0 {
        return Err(format!(
            "deferred entrypoint or CLI contains {runtime_open_calls} ArmRuntime::open caller(s)"
        ));
    }
    Ok(())
}

#[test]
fn unavailable_u0_green_u1_deferral_forgotten_u2_silent_fallback_red() {
    let entrypoint = read(manifest_dir().join("src/arm/simulation_entrypoint.rs"));
    let cli = read(manifest_dir().join("../cli/src/mev_trader.rs"));
    validate_deferred_production_handoff(&entrypoint, &cli)
        .expect("U0 explicitly deferred rejecting production handoff");
    eprintln!("U0: GREEN");

    let mutant = entrypoint.replace("ProductionInstallationDeferred", "ProductionInstallerMissing");
    assert_ne!(mutant, entrypoint, "U1 patch did not change source");
    assert!(validate_deferred_production_handoff(&mutant, &cli).is_err());
    eprintln!("U1: RED");

    let mutant = entrypoint.replacen(
        "Err(T4eHandoffError::Rejected)",
        "Ok(()) /* silent candidate loss */",
        1,
    );
    assert_ne!(mutant, entrypoint, "U2 patch did not change source");
    assert!(validate_deferred_production_handoff(&mutant, &cli).is_err());
    eprintln!("U2: RED");

    for (name, result) in [
        ("Busy", "Err(T4eHandoffError::Busy)"),
        ("Closed", "Err(T4eHandoffError::Closed)"),
        ("Ok", "Ok(())"),
        ("Rejected", "Err(T4eHandoffError::Rejected)"),
    ] {
        let addition = format!(
            "struct AlternateHandoff{name};\n\
             impl T4eCandidateHandoff for AlternateHandoff{name} {{\n\
             \tfn try_handoff(&self, _candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {{\n\
             \t\t{result}\n\
             \t}}\n\
             }}"
        );
        let mutant = insert_before_test_module(&entrypoint, &addition);
        assert_ne!(mutant, entrypoint, "alternate {name} handoff patch did not change source");
        assert!(
            validate_deferred_production_handoff(&mutant, &cli).is_err(),
            "alternate {name} handoff remained GREEN"
        );
        eprintln!("U-{name}: RED");
    }

    let mutant = insert_before_test_module(
        &entrypoint,
        "fn aliased_runtime_probe() {\n\
         \tuse super::ArmRuntime as RuntimeProbe;\n\
         \tlet _ = RuntimeProbe::open();\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "aliased ArmRuntime open patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "aliased ArmRuntime open remained GREEN"
    );
    eprintln!("U-ALIASED-OPEN: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "fn hidden_simulation_installer() {\n\
         \tlet _ = SimulationEntrypoint::ready();\n\
         \tlet _ = SimulationWorker::spawn(runtime, armed, store);\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "hidden Ready/spawn caller patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "hidden Ready/spawn caller remained GREEN"
    );
    eprintln!("U-HIDDEN-INSTALLER: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "struct CandidateRetentionBypass {\n\
         \tcandidate: Option<SealedUnsignedCandidate>,\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "candidate retention patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "candidate retention remained GREEN"
    );
    eprintln!("U-CANDIDATE-RETENTION: RED");

    for (name, result) in [
        ("Busy", "Err(T4eHandoffError::Busy)"),
        ("Closed", "Err(T4eHandoffError::Closed)"),
        ("Rejected", "Err(T4eHandoffError::Rejected)"),
    ] {
        let addition = format!(
            "trait HandoffAlias{name} = T4eCandidateHandoff;\n\
             struct TraitAliasedAlternate{name};\n\
             impl HandoffAlias{name} for TraitAliasedAlternate{name} {{\n\
             \tfn try_handoff(&self, _candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {{\n\
             \t\t{result}\n\
             \t}}\n\
             }}"
        );
        let mutant = insert_before_test_module(&entrypoint, &addition);
        assert_ne!(mutant, entrypoint, "trait-alias {name} patch did not change source");
        assert!(
            validate_deferred_production_handoff(&mutant, &cli).is_err(),
            "trait-alias alternate {name} handoff remained GREEN"
        );
        eprintln!("U-TRAIT-ALIAS-{name}: RED");
    }

    let mutant = insert_before_test_module(
        &entrypoint,
        "type RuntimeAliasOne = ArmRuntime;\n\
         type RuntimeAliasTwo = RuntimeAliasOne;\n\
         fn type_aliased_runtime_probe() {\n\
         \tlet open = RuntimeAliasTwo::open;\n\
         \tlet _ = open;\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "ArmRuntime type-alias chain patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "ArmRuntime type-alias chain/function value remained GREEN"
    );
    eprintln!("U-RUNTIME-TYPE-ALIAS: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "use super::ArmRuntime as RuntimeUseOne;\n\
         use RuntimeUseOne as RuntimeUseTwo;\n\
         fn use_aliased_runtime_probe() {\n\
         \tlet _ = <RuntimeUseTwo>::open();\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "ArmRuntime use-alias/UFCS patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "ArmRuntime use-alias chain/UFCS remained GREEN"
    );
    eprintln!("U-RUNTIME-USE-ALIAS-UFCS: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "type EntrypointAliasOne = SimulationEntrypoint;\n\
         type EntrypointAliasTwo = EntrypointAliasOne;\n\
         type WorkerAliasOne = SimulationWorker;\n\
         type WorkerAliasTwo = WorkerAliasOne;\n\
         fn aliased_simulation_function_values() {\n\
         \tlet ready = EntrypointAliasTwo::ready;\n\
         \tlet spawn = WorkerAliasTwo::spawn;\n\
         \tlet _ = (ready, spawn);\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "entrypoint/worker alias patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "entrypoint/worker alias function values remained GREEN"
    );
    eprintln!("U-ENTRYPOINT-WORKER-ALIASES: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "type EntrypointUfcsAlias = SimulationEntrypoint;\n\
         type WorkerUfcsAlias = SimulationWorker;\n\
         fn aliased_simulation_ufcs() {\n\
         \tlet _ = <EntrypointUfcsAlias>::ready();\n\
         \tlet _ = <WorkerUfcsAlias>::spawn(runtime, armed, store);\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "entrypoint/worker UFCS patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "entrypoint/worker alias UFCS remained GREEN"
    );
    eprintln!("U-ENTRYPOINT-WORKER-UFCS: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "use SimulationEntrypoint::ready as make_ready;\n\
         use SimulationWorker::spawn as spawn_worker;\n\
         fn imported_simulation_function_values() {\n\
         \tlet _ = make_ready;\n\
         \tlet _ = spawn_worker;\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "associated function import patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "imported ready/spawn function values remained GREEN"
    );
    eprintln!("U-IMPORTED-FUNCTION-VALUES: RED");

    let mutant = insert_before_test_module(
        &entrypoint,
        "type CandidateAliasOne = SealedUnsignedCandidate;\n\
         type CandidateAliasTwo = CandidateAliasOne;\n\
         struct TypeAliasedCandidateRetention {\n\
         \tcandidate: Option<CandidateAliasTwo>,\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "candidate type-alias patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "candidate type-alias retention remained GREEN"
    );
    eprintln!("U-CANDIDATE-TYPE-ALIAS: RED");

    for (owner, member) in [
        ("SimulationWorker", "spawn"),
        ("SimulationEntrypoint", "ready"),
        ("ArmRuntime", "open"),
    ] {
        let addition = format!(
            "use {owner}::{member};\n\
             fn direct_associated_import_probe() {{\n\
             \tlet _ = {member};\n\
             }}"
        );
        let mutant = insert_before_test_module(&entrypoint, &addition);
        assert_ne!(
            mutant, entrypoint,
            "direct associated {owner}::{member} import patch did not change source"
        );
        assert!(
            validate_deferred_production_handoff(&mutant, &cli).is_err(),
            "direct associated {owner}::{member} import remained GREEN"
        );
        eprintln!("U-DIRECT-ASSOCIATED-{owner}-{member}: RED");
    }

    for (name, body) in [
        ("STMT", "install_production_simulation!();"),
        ("EXPR", "let _ = install_production_simulation!();"),
    ] {
        let addition = format!(
            "fn in_function_macro_{name}() {{\n\
             \t{body}\n\
             }}"
        );
        let mutant = insert_before_test_module(&entrypoint, &addition);
        assert_ne!(mutant, entrypoint, "in-function {name} macro patch did not change source");
        assert!(
            validate_deferred_production_handoff(&mutant, &cli).is_err(),
            "opaque in-function {name} macro remained GREEN"
        );
        eprintln!("U-IN-FUNCTION-MACRO-{name}: RED");
    }

    for (name, declaration) in [
        (
            "ENUM-VARIANT",
            "type RetainedCandidate = SealedUnsignedCandidate;\n\
             enum CandidateRetentionEnum {\n\
             \tRetained(RetainedCandidate),\n\
             }",
        ),
        (
            "UNION-FIELD",
            "type RetainedCandidate = SealedUnsignedCandidate;\n\
             union CandidateRetentionUnion {\n\
             \tretained: std::mem::ManuallyDrop<RetainedCandidate>,\n\
             }",
        ),
        (
            "TYPED-STATIC",
            "type RetainedCandidate = SealedUnsignedCandidate;\n\
             static RETAINED_CANDIDATE: Option<RetainedCandidate> = None;",
        ),
        (
            "TYPED-CONST",
            "type RetainedCandidate = SealedUnsignedCandidate;\n\
             const RETAINED_CANDIDATE: Option<RetainedCandidate> = None;",
        ),
    ] {
        let mutant = insert_before_test_module(&entrypoint, declaration);
        assert_ne!(mutant, entrypoint, "{name} retention patch did not change source");
        assert!(
            validate_deferred_production_handoff(&mutant, &cli).is_err(),
            "candidate retention through {name} alias remained GREEN"
        );
        eprintln!("U-CANDIDATE-RETENTION-{name}: RED");
    }

    let mutant = insert_before_test_module(
        &entrypoint,
        "trait RuntimeProjection {\n\
         \ttype Runtime;\n\
         }\n\
         impl RuntimeProjection for () {\n\
         \ttype Runtime = ArmRuntime;\n\
         }\n\
         type ProjectedRuntime = <() as RuntimeProjection>::Runtime;\n\
         fn projected_runtime_function_value() {\n\
         \tlet open = ProjectedRuntime::open;\n\
         \tlet _ = open;\n\
         }",
    );
    assert_ne!(mutant, entrypoint, "associated-type projection patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "associated-type projection/runtime function value remained GREEN"
    );
    eprintln!("U-ASSOCIATED-TYPE-PROJECTION: RED");
    let mutant = insert_before_test_module(
        &entrypoint,
        "install_production_simulation!();",
    );
    assert_ne!(mutant, entrypoint, "production macro item patch did not change source");
    assert!(
        validate_deferred_production_handoff(&mutant, &cli).is_err(),
        "opaque production macro item remained GREEN"
    );
    eprintln!("U-PRODUCTION-MACRO-ITEM: RED");
}
