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
            "dep:libc",
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
        "libc",
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

fn public_use_exports(source: &str, name: &str) -> bool {
    fn tree_exports(tree: &syn::UseTree, name: &str) -> bool {
        match tree {
            syn::UseTree::Name(item) => item.ident == name,
            syn::UseTree::Rename(item) => item.rename == name,
            syn::UseTree::Path(path) => tree_exports(&path.tree, name),
            syn::UseTree::Group(group) => group.items.iter().any(|item| tree_exports(item, name)),
            syn::UseTree::Glob(_) => false,
        }
    }

    syn::parse_file(source).is_ok_and(|file| {
        file.items.iter().any(|item| {
            matches!(
                item,
                syn::Item::Use(item)
                    if matches!(item.vis, syn::Visibility::Public(_))
                        && tree_exports(&item.tree, name)
            )
        })
    })
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
    if !public_use_exports(lib, "SimulationCorrelationEnvelopeV1")
        || !public_use_exports(lib, "SimulationCorrelationKey")
    {
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
            "address"
                | "b256"
                | "sol"
                | "assert"
                | "assert_eq"
                | "assert_ne"
                | "bail"
                | "concat"
                | "cfg"
                | "debug"
                | "debug_assert"
                | "debug_assert_eq"
                | "env"
                | "error"
                | "eyre"
                | "format"
                | "include_bytes"
                | "include_str"
                | "info"
                | "json"
                | "matches"
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
                        | "ProductionSimulationHandoff"
                )
            })
}

const PROTECTED_PRODUCTION_TYPES: [&str; 6] = [
    "T4eCandidateHandoff",
    "ArmRuntime",
    "SimulationEntrypoint",
    "SimulationWorker",
    "SealedUnsignedCandidate",
    "ProductionSimulationHandoff",
];

struct AliasBinding {
    alias: String,
    target: Vec<String>,
    scope: Vec<String>,
}

#[derive(Default)]
struct ProductionAliasCollector {
    bindings: Vec<AliasBinding>,
    opaque_use_globs: usize,
    module_scope: Vec<String>,
    file_root_len: usize,
    canonical_source: bool,
    shadowing_definitions: BTreeSet<String>,
    protected_definitions: BTreeMap<String, BTreeSet<String>>,
}

fn qualified_name(scope: &[String], name: &str) -> String {
    scope
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(name))
        .collect::<Vec<_>>()
        .join("::")
}

fn path_scope_candidates(mut scope: Vec<String>, mut segments: Vec<String>) -> Vec<String> {
    if segments.first().is_some_and(|segment| segment == "crate") {
        segments.remove(0);
        return vec![segments.join("::")];
    }
    let mut explicit_parent = false;
    while segments.first().is_some_and(|segment| segment == "super") {
        explicit_parent = true;
        segments.remove(0);
        scope.pop();
    }
    if explicit_parent {
        return vec![scope
            .iter()
            .chain(&segments)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("::")];
    }
    if segments.first().is_some_and(|segment| segment == "self") {
        segments.remove(0);
        return vec![scope
            .iter()
            .chain(&segments)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("::")];
    }

    let mut candidates = Vec::new();
    loop {
        candidates.push(
            scope
                .iter()
                .chain(&segments)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("::"),
        );
        if scope.pop().is_none() {
            break;
        }
    }
    candidates
}

fn path_parts_resolve_identity(
    scope: &[String],
    segments: &[String],
    identities: &BTreeSet<String>,
    canonical_name: Option<&str>,
    shadowing_definitions: &BTreeSet<String>,
) -> bool {
    let candidates = path_scope_candidates(scope.to_vec(), segments.to_vec());
    for candidate in &candidates {
        if identities.contains(candidate) {
            return true;
        }
        if shadowing_definitions.contains(candidate) {
            return false;
        }
    }
    canonical_name.is_some_and(|canonical| {
        segments.last().is_some_and(|segment| segment == canonical)
    })
}

impl ProductionAliasCollector {
    fn collect_file(&mut self, file: &syn::File, module_root: &[String], canonical_source: bool) {
        use syn::visit::Visit;

        self.module_scope = module_root.to_vec();
        self.file_root_len = module_root.len();
        self.canonical_source = canonical_source;
        self.visit_file(file);
        self.module_scope.clear();
        self.file_root_len = 0;
        self.canonical_source = false;
    }

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
                    scope: self.module_scope.clone(),
                });
                self.shadowing_definitions
                    .insert(qualified_name(&self.module_scope, &name.ident.to_string()));
                prefix.pop();
            }
            syn::UseTree::Rename(rename) => {
                prefix.push(rename.ident.to_string());
                self.bindings.push(AliasBinding {
                    alias: rename.rename.to_string(),
                    target: prefix.clone(),
                    scope: self.module_scope.clone(),
                });
                self.shadowing_definitions.insert(qualified_name(
                    &self.module_scope,
                    &rename.rename.to_string(),
                ));
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
                scope: self.module_scope.clone(),
            });
        }
    }

    fn record_shadowing_definition(&mut self, item: &syn::Item) {
        let ident = match item {
            syn::Item::Enum(item) => Some(&item.ident),
            syn::Item::Struct(item) => Some(&item.ident),
            syn::Item::Trait(item) => Some(&item.ident),
            syn::Item::Mod(item) => Some(&item.ident),
            syn::Item::Union(item) => Some(&item.ident),
            syn::Item::TraitAlias(item) => Some(&item.ident),
            syn::Item::Type(item) => Some(&item.ident),
            _ => None,
        };
        if let Some(ident) = ident {
            let name = ident.to_string();
            if self.canonical_source
                && self.module_scope.len() == self.file_root_len
                && PROTECTED_PRODUCTION_TYPES.contains(&name.as_str())
            {
                self.protected_definitions
                    .entry(name)
                    .or_default()
                    .insert(qualified_name(&self.module_scope, &ident.to_string()));
            } else {
                self.shadowing_definitions
                    .insert(qualified_name(&self.module_scope, &ident.to_string()));
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for ProductionAliasCollector {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !item_has_cfg_test(item) {
            self.record_shadowing_definition(item);
            syn::visit::visit_item(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module_scope.push(item.ident.to_string());
        if let Some((_, items)) = &item.content {
            for item in items {
                self.visit_item(item);
            }
        }
        self.module_scope.pop();
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
                    scope: self.module_scope.clone(),
                });
            }
        }
    }
}

#[derive(Clone)]
struct ProductionAliases {
    types: BTreeMap<String, BTreeSet<String>>,
    runtime_open: BTreeSet<String>,
    entrypoint_ready: BTreeSet<String>,
    worker_spawn: BTreeSet<String>,
    opaque_use_globs: usize,
    shadowing_definitions: BTreeSet<String>,
}

impl ProductionAliases {
    fn resolve(collector: ProductionAliasCollector) -> Self {
        let mut types = BTreeMap::new();
        for symbol in PROTECTED_PRODUCTION_TYPES {
            types.insert(
                symbol.to_owned(),
                collector.protected_definitions.get(symbol).cloned().unwrap_or_default(),
            );
        }
        let mut runtime_open = BTreeSet::new();
        let mut entrypoint_ready = BTreeSet::new();
        let mut worker_spawn = BTreeSet::new();

        loop {
            let mut changed = false;
            for binding in &collector.bindings {
                let binding_identity = qualified_name(&binding.scope, &binding.alias);
                let matching_types = types
                    .iter()
                    .filter(|(symbol, identities)| {
                        path_parts_resolve_identity(
                            &binding.scope,
                            &binding.target,
                            identities,
                            Some(symbol.as_str()),
                            &collector.shadowing_definitions,
                        )
                    })
                    .map(|(symbol, _)| symbol.clone())
                    .collect::<Vec<_>>();
                for symbol in matching_types {
                    changed |= types
                        .get_mut(&symbol)
                        .expect("protected production type")
                        .insert(binding_identity.clone());
                }

                let target = binding.target.last().map(String::as_str);
                let owner_segments =
                    &binding.target[..binding.target.len().saturating_sub(1)];
                let is_function_alias = |aliases: &BTreeSet<String>,
                                         owner_aliases: &BTreeSet<String>,
                                         owner_symbol: &str,
                                         member: &str| {
                    path_parts_resolve_identity(
                        &binding.scope,
                        &binding.target,
                        aliases,
                        None,
                        &collector.shadowing_definitions,
                    ) || (target == Some(member)
                        && path_parts_resolve_identity(
                            &binding.scope,
                            owner_segments,
                            owner_aliases,
                            Some(owner_symbol),
                            &collector.shadowing_definitions,
                        ))
                };
                if is_function_alias(
                    &runtime_open,
                    &types["ArmRuntime"],
                    "ArmRuntime",
                    "open",
                ) {
                    changed |= runtime_open.insert(binding_identity.clone());
                }
                if is_function_alias(
                    &entrypoint_ready,
                    &types["SimulationEntrypoint"],
                    "SimulationEntrypoint",
                    "ready",
                ) {
                    changed |= entrypoint_ready.insert(binding_identity.clone());
                }
                if is_function_alias(
                    &worker_spawn,
                    &types["SimulationWorker"],
                    "SimulationWorker",
                    "spawn",
                ) {
                    changed |= worker_spawn.insert(binding_identity);
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
            shadowing_definitions: collector.shadowing_definitions,
        }
    }

    fn path_is_protected(&self, scope: &[String], path: &syn::Path, symbol: &str) -> bool {
        let segments =
            path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        path_parts_resolve_identity(
            scope,
            &segments,
            &self.types[symbol],
            Some(symbol),
            &self.shadowing_definitions,
        )
    }

    fn path_is_alias(&self, scope: &[String], path: &syn::Path, aliases: &BTreeSet<String>) -> bool {
        let segments =
            path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        path_parts_resolve_identity(
            scope,
            &segments,
            aliases,
            None,
            &self.shadowing_definitions,
        )
    }

    fn type_is_protected(&self, scope: &[String], ty: &syn::Type, symbol: &str) -> bool {
        matches!(ty, syn::Type::Path(ty) if ty.qself.is_none() && self.path_is_protected(scope, &ty.path, symbol))
    }
}


fn try_handoff_candidate(
    method: &syn::ImplItemFn,
    aliases: &ProductionAliases,
    module_scope: &[String],
) -> Option<String> {
    if method.sig.ident != "try_handoff" {
        return None;
    }
    let syn::ReturnType::Type(_, output) = &method.sig.output else {
        return None;
    };
    let syn::Type::Path(result) = output.as_ref() else {
        return None;
    };
    let Some(segment) = result.path.segments.last() else {
        return None;
    };
    if segment.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    let mut arguments = arguments.args.iter();
    if !matches!(
        arguments.next(),
        Some(syn::GenericArgument::Type(syn::Type::Tuple(unit))) if unit.elems.is_empty()
    ) || !matches!(
        arguments.next(),
        Some(syn::GenericArgument::Type(syn::Type::Path(error)))
            if path_ends_with(&error.path, &["T4eHandoffError"])
    ) || arguments.next().is_some()
    {
        return None;
    }

    method.sig.inputs.iter().find_map(|argument| {
        let syn::FnArg::Typed(argument) = argument else {
            return None;
        };
        if !aliases.type_is_protected(
            module_scope,
            &argument.ty,
            "SealedUnsignedCandidate",
        ) {
            return None;
        }
        match argument.pat.as_ref() {
            syn::Pat::Ident(binding) => Some(binding.ident.to_string()),
            _ => None,
        }
    })
}

fn has_exact_installed_handoff_body(
    method: &syn::ImplItemFn,
    aliases: &ProductionAliases,
    module_scope: &[String],
) -> bool {
    has_total_installed_handoff_body(method, aliases, module_scope)
}


struct CandidateFieldVisitor<'a> {
    aliases: &'a ProductionAliases,
    module_scope: &'a [String],
    retains_candidate: bool,
}

impl<'ast> syn::visit::Visit<'ast> for CandidateFieldVisitor<'_> {
    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        if ty.qself.is_some()
            || self
                .aliases
                .path_is_protected(self.module_scope, &ty.path, "SealedUnsignedCandidate")
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
    exact_installed_try_handoff_methods: usize,
    candidate_fields: usize,
    worker_spawn_calls: usize,
    entrypoint_ready_calls: usize,
    entrypoint_ready_calls_in_worker_spawn: usize,
    runtime_open_calls: usize,
    opaque_production_items: usize,
    function_body_depth: usize,
    module_scope: Vec<String>,
    in_worker_impl: bool,
    in_worker_spawn: bool,
    current_impl_protected: BTreeSet<String>,
}

impl ProductionHandoffVisitor {
    fn new(aliases: ProductionAliases, module_root: Vec<String>) -> Self {
        Self {
            aliases,
            handoff_impls: 0,
            handoff_impl_target_mismatches: 0,
            try_handoff_methods: 0,
            exact_installed_try_handoff_methods: 0,
            candidate_fields: 0,
            worker_spawn_calls: 0,
            entrypoint_ready_calls: 0,
            entrypoint_ready_calls_in_worker_spawn: 0,
            runtime_open_calls: 0,
            opaque_production_items: 0,
            function_body_depth: 0,
            module_scope: module_root,
            in_worker_impl: false,
            in_worker_spawn: false,
            current_impl_protected: BTreeSet::new(),
        }
    }

    fn record_candidate_type(&mut self, ty: &syn::Type) {
        let mut field_visitor = CandidateFieldVisitor {
            aliases: &self.aliases,
            module_scope: &self.module_scope,
            retains_candidate: false,
        };
        syn::visit::Visit::visit_type(&mut field_visitor, ty);
        if field_visitor.retains_candidate {
            self.candidate_fields += 1;
        }
    }

    fn path_is_protected(&self, path: &syn::Path, symbol: &str) -> bool {
        self.aliases.path_is_protected(&self.module_scope, path, symbol)
    }

    fn type_is_protected(&self, ty: &syn::Type, symbol: &str) -> bool {
        self.aliases.type_is_protected(&self.module_scope, ty, symbol)
    }

    fn expression_references_protected_associated(
        &self,
        expression: &syn::ExprPath,
        symbol: &str,
        member_aliases: &BTreeSet<String>,
        member: &str,
    ) -> bool {
        let segments = &expression.path.segments;
        let self_owner = segments.len() == 2
            && segments.first().is_some_and(|segment| segment.ident == "Self")
            && self.current_impl_protected.contains(symbol);
        let direct_alias = self
            .aliases
            .path_is_alias(&self.module_scope, &expression.path, member_aliases);
        let associated = segments.last().is_some_and(|segment| segment.ident == member)
            && if let Some(qself) = &expression.qself {
                self.type_is_protected(&qself.ty, symbol)
            } else if segments.len() >= 2 {
                let mut owner = expression.path.clone();
                owner.segments.pop();
                self.path_is_protected(&owner, symbol)
            } else {
                false
            };
        direct_alias || associated || self_owner
    }
}

impl<'ast> syn::visit::Visit<'ast> for ProductionHandoffVisitor {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if item_has_cfg_test(item) {
            return;
        }
        match item {
            syn::Item::Macro(item) => {
                if item.ident.is_some() || !known_macro_without_sealed_symbols(&item.mac) {
                    self.opaque_production_items += 1;
                }
            }
            syn::Item::Verbatim(_) => {
                self.opaque_production_items += 1;
            }
            _ => syn::visit::visit_item(self, item),
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if item.attrs.iter().any(|attr| attr.path().is_ident("path")) {
            self.opaque_production_items += 1;
            return;
        }
        self.module_scope.push(item.ident.to_string());
        if let Some((_, items)) = &item.content {
            for item in items {
                self.visit_item(item);
            }
        }
        self.module_scope.pop();
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
        let is_handoff_impl = item.trait_.as_ref().is_some_and(|(_, path, _)| {
            self.path_is_protected(path, "T4eCandidateHandoff")
        });
        let self_is_installed =
            self.type_is_protected(item.self_ty.as_ref(), "ProductionSimulationHandoff");
        let try_handoff_methods = is_handoff_impl
            .then(|| {
                item.items
                    .iter()
                    .filter(|impl_item| {
                        matches!(
                            impl_item,
                            syn::ImplItem::Fn(method)
                                if method.sig.ident == "try_handoff" && !has_cfg_test(&method.attrs)
                        )
                    })
                    .count()
            })
            .unwrap_or_default();
        let exact_installed_try_handoff_methods = is_handoff_impl
            .then(|| {
                item.items
                    .iter()
                    .filter(|impl_item| {
                        matches!(
                            impl_item,
                            syn::ImplItem::Fn(method)
                                if !has_cfg_test(&method.attrs)
                                    && has_exact_installed_handoff_body(
                                        method,
                                        &self.aliases,
                                        &self.module_scope,
                                    )
                        )
                    })
                    .count()
            })
            .unwrap_or_default();
        self.try_handoff_methods += try_handoff_methods;
        self.exact_installed_try_handoff_methods += exact_installed_try_handoff_methods;
        if is_handoff_impl {
            self.handoff_impls += 1;
            if !self_is_installed {
                self.handoff_impl_target_mismatches += 1;
            }
        }

        let old_in_worker_impl = self.in_worker_impl;
        let old_current_impl_protected = std::mem::take(&mut self.current_impl_protected);
        for symbol in ["ArmRuntime", "SimulationEntrypoint", "SimulationWorker"] {
            if self.type_is_protected(item.self_ty.as_ref(), symbol) {
                self.current_impl_protected.insert(symbol.to_owned());
            }
        }
        self.in_worker_impl = self.current_impl_protected.contains("SimulationWorker");
        syn::visit::visit_item_impl(self, item);
        self.in_worker_impl = old_in_worker_impl;
        self.current_impl_protected = old_current_impl_protected;
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
        if self.expression_references_protected_associated(
            expression,
            "SimulationWorker",
            &self.aliases.worker_spawn,
            "spawn",
        ) {
            self.worker_spawn_calls += 1;
        }
        if self.expression_references_protected_associated(
            expression,
            "SimulationEntrypoint",
            &self.aliases.entrypoint_ready,
            "ready",
        ) {
            self.entrypoint_ready_calls += 1;
            if self.in_worker_spawn {
                self.entrypoint_ready_calls_in_worker_spawn += 1;
            }
        }
        if self.expression_references_protected_associated(
            expression,
            "ArmRuntime",
            &self.aliases.runtime_open,
            "open",
        ) {
            self.runtime_open_calls += 1;
        }
        syn::visit::visit_expr_path(self, expression);
    }
}

struct ParsedProductionSource {
    file: syn::File,
    module_root: Vec<String>,
    canonical_source: bool,
}

fn parse_production_source(
    source: &str,
    label: &str,
    module_root: Vec<String>,
    canonical_source: bool,
) -> Result<ParsedProductionSource, String> {
    let file = syn::parse_file(source).map_err(|error| format!("{label} did not parse: {error}"))?;
    Ok(ParsedProductionSource {
        file,
        module_root,
        canonical_source,
    })
}

fn module_root_for_source(path: &Path, source_root: &Path) -> Result<Vec<String>, String> {
    let relative = path
        .strip_prefix(source_root)
        .map_err(|_| format!("source {} is outside {}", path.display(), source_root.display()))?;
    let mut components = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let stem = relative
        .file_stem()
        .ok_or_else(|| format!("source {} has no file stem", path.display()))?
        .to_string_lossy();
    if stem != "lib" && stem != "main" && stem != "mod" {
        components.push(stem.into_owned());
    }
    Ok(components)
}

fn analyze_production_handoff(
    source: &ParsedProductionSource,
    aliases: &ProductionAliases,
) -> ProductionHandoffVisitor {
    use syn::visit::Visit;

    let mut visitor =
        ProductionHandoffVisitor::new(aliases.clone(), source.module_root.clone());
    visitor.visit_file(&source.file);
    visitor
}

fn validate_install_error_taxonomy(source: &str) -> Result<(), String> {
    let file = syn::parse_file(source)
        .map_err(|error| format!("production installation source did not parse: {error}"))?;
    let matches = file
        .items
        .iter()
        .filter_map(|item| {
            let syn::Item::Enum(item) = item else {
                return None;
            };
            (item.ident == "ProductionSimulationInstallError" && !has_cfg_test(&item.attrs))
                .then_some(item)
        })
        .collect::<Vec<_>>();
    let [install_error] = matches.as_slice() else {
        return Err(format!(
            "ProductionSimulationInstallError cardinality changed: {}",
            matches.len()
        ));
    };
    if !install_error.generics.params.is_empty() {
        return Err("ProductionSimulationInstallError gained generics".to_owned());
    }

    let actual = install_error
        .variants
        .iter()
        .map(|variant| {
            let payload = match &variant.fields {
                syn::Fields::Unit => String::new(),
                syn::Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                    let syn::Type::Path(payload) = &fields.unnamed[0].ty else {
                        return Err(format!("{} payload is not a type path", variant.ident));
                    };
                    if payload.qself.is_some() {
                        return Err(format!("{} payload uses qualified self", variant.ident));
                    }
                    let Some(payload) = payload.path.segments.last() else {
                        return Err(format!("{} payload path is empty", variant.ident));
                    };
                    format!("({})", payload.ident)
                }
                syn::Fields::Named(_) | syn::Fields::Unnamed(_) => {
                    return Err(format!("{} payload shape changed", variant.ident));
                }
            };
            Ok(format!("{}{payload}", variant.ident))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let expected = [
        "InstallationInProgress",
        "ActivationInvariant",
        "ArmRuntimeUnavailable(ProductionArmRuntimeOpenFailure)",
        "CommittedStateUnavailable(ProductionProviderFailure)",
        "DrawdownAuthorityUnavailable(SettledLossUnavailableReason)",
        "CampaignBundleUnavailable(ProductionCampaignBundleFailure)",
        "ClaimStoreUnavailable(ProductionClaimFailure)",
        "DeploymentIdentityUnavailable(ProductionDeploymentFailure)",
        "CustodyUnavailable(ProductionCustodyFailure)",
        "FailSinkUnavailable(ProductionArmFailure)",
        "ArmingUnavailable(ProductionArmFailure)",
        "PersistenceUnavailable(ProductionStoreOpenFailure)",
        "CapacityUnavailable(SimulationLedgerClosure)",
        "BridgeUnavailable(ProductionBridgeFailure)",
        "WorkerSpawnUnavailable",
        "WorkerStartupUnavailable(WorkerStartupFailure)",
    ];
    if actual != expected {
        return Err(format!(
            "ProductionSimulationInstallError taxonomy changed: {actual:?}"
        ));
    }
    Ok(())
}

#[test]
fn installed_error_taxonomy_control_and_mutants() {
    const CONTROL: &str = r#"
enum ProductionSimulationInstallError {
    InstallationInProgress,
    ActivationInvariant,
    ArmRuntimeUnavailable(ProductionArmRuntimeOpenFailure),
    CommittedStateUnavailable(ProductionProviderFailure),
    DrawdownAuthorityUnavailable(SettledLossUnavailableReason),
    CampaignBundleUnavailable(ProductionCampaignBundleFailure),
    ClaimStoreUnavailable(ProductionClaimFailure),
    DeploymentIdentityUnavailable(ProductionDeploymentFailure),
    CustodyUnavailable(ProductionCustodyFailure),
    FailSinkUnavailable(ProductionArmFailure),
    ArmingUnavailable(ProductionArmFailure),
    PersistenceUnavailable(ProductionStoreOpenFailure),
    CapacityUnavailable(SimulationLedgerClosure),
    BridgeUnavailable(ProductionBridgeFailure),
    WorkerSpawnUnavailable,
    WorkerStartupUnavailable(WorkerStartupFailure),
}
"#;
    validate_install_error_taxonomy(CONTROL).expect("installed error taxonomy control");
    eprintln!("U-INSTALL-ERROR-TAXONOMY: GREEN");

    for (name, mutant) in [
        (
            "MISSING-REASON",
            CONTROL.replace("    BridgeUnavailable(ProductionBridgeFailure),\n", ""),
        ),
        (
            "AGGREGATE-DEFERRAL",
            CONTROL.replace(
                "    InstallationInProgress,\n",
                "    InstallationInProgress,\n    ProductionInstallationDeferred,\n",
            ),
        ),
        (
            "COLLAPSED-PAYLOAD",
            CONTROL.replace(
                "CommittedStateUnavailable(ProductionProviderFailure)",
                "CommittedStateUnavailable(ProductionArmFailure)",
            ),
        ),
    ] {
        assert_ne!(mutant, CONTROL, "{name} patch did not change source");
        assert!(
            validate_install_error_taxonomy(&mutant).is_err(),
            "{name} mutant remained GREEN"
        );
        eprintln!("U-INSTALL-ERROR-{name}: RED");
    }
}

#[derive(Default)]
struct InstallConstructorBodyVisitor {
    state_uses: usize,
    fixed_state_constructions: usize,
}

impl<'ast> syn::visit::Visit<'ast> for InstallConstructorBodyVisitor {
    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression.path.segments.last().is_some_and(|segment| segment.ident == "state") {
            self.state_uses += 1;
        }
        let segments =
            expression.path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        if segments.first().is_some_and(|segment| segment == "ProductionHandoffState")
            || segments.as_slice() == ["Default", "default"]
        {
            self.fixed_state_constructions += 1;
        }
        syn::visit::visit_expr_path(self, expression);
    }
}

fn validate_installed_handoff_constructor(source: &str) -> Result<(), String> {
    use syn::visit::Visit;

    let file = syn::parse_file(source)
        .map_err(|error| format!("production handoff source did not parse: {error}"))?;
    let methods = file
        .items
        .iter()
        .filter_map(|item| {
            let syn::Item::Impl(item) = item else {
                return None;
            };
            let syn::Type::Path(owner) = item.self_ty.as_ref() else {
                return None;
            };
            (item.trait_.is_none()
                && owner.qself.is_none()
                && path_ends_with(&owner.path, &["ProductionSimulationHandoff"]))
            .then_some(item)
        })
        .flat_map(|item| &item.items)
        .filter_map(|item| {
            let syn::ImplItem::Fn(method) = item else {
                return None;
            };
            (method.sig.ident == "install" && !has_cfg_test(&method.attrs)).then_some(method)
        })
        .collect::<Vec<_>>();
    let [method] = methods.as_slice() else {
        return Err(format!(
            "ProductionSimulationHandoff::install cardinality changed: {}",
            methods.len()
        ));
    };
    let [syn::FnArg::Typed(input)] = method.sig.inputs.iter().collect::<Vec<_>>().as_slice() else {
        return Err("install must consume exactly one state input and no receiver".to_owned());
    };
    if !matches!(method.vis, syn::Visibility::Public(_))
        || !matches!(
            input.pat.as_ref(),
            syn::Pat::Ident(binding)
                if binding.ident == "state"
                    && binding.by_ref.is_none()
                    && binding.mutability.is_none()
                    && binding.subpat.is_none()
        )
        || !matches!(
            input.ty.as_ref(),
            syn::Type::Path(state)
                if state.qself.is_none()
                    && path_ends_with(&state.path, &["ProductionHandoffState"])
        )
        || !matches!(
            &method.sig.output,
            syn::ReturnType::Type(_, output)
                if matches!(
                    output.as_ref(),
                    syn::Type::Path(output)
                        if output.qself.is_none() && path_ends_with(&output.path, &["Self"])
                )
        )
    {
        return Err("ProductionSimulationHandoff::install signature changed".to_owned());
    }

    let mut body = InstallConstructorBodyVisitor::default();
    body.visit_block(&method.block);
    if body.state_uses != 1 || body.fixed_state_constructions != 0 {
        return Err(format!(
            "install must move its supplied state exactly once without a fixed fallback: state_uses={}, fixed_states={}",
            body.state_uses, body.fixed_state_constructions
        ));
    }
    Ok(())
}

#[test]
fn installed_handoff_constructor_control_and_mutants() {
    const CONTROL: &str = r#"
impl ProductionSimulationHandoff {
    pub fn install(state: ProductionHandoffState) -> Self {
        Self {
            shared: Arc::new(ProductionHandoffShared {
                state: Mutex::new(state),
            }),
        }
    }
}
"#;
    validate_installed_handoff_constructor(CONTROL).expect("installed handoff constructor control");
    eprintln!("U-INSTALL-CONSTRUCTOR: GREEN");

    for (name, mutant) in [
        (
            "DEFAULT-UNAVAILABLE",
            CONTROL.replace(
                "Mutex::new(state)",
                "Mutex::new(ProductionHandoffState::Unavailable(error))",
            ),
        ),
        (
            "DEFERRED-CONSTRUCTOR",
            CONTROL.replace("pub fn install(", "pub fn deferred_production("),
        ),
        (
            "SECOND-CONSTRUCTOR",
            format!("{CONTROL}\n{CONTROL}"),
        ),
    ] {
        assert_ne!(mutant, CONTROL, "{name} patch did not change source");
        assert!(
            validate_installed_handoff_constructor(&mutant).is_err(),
            "{name} mutant remained GREEN"
        );
        eprintln!("U-INSTALL-CONSTRUCTOR-{name}: RED");
    }
}

#[derive(Default)]
struct InstalledHandoffBodyVisitor {
    candidate_uses: usize,
    rejected: usize,
    busy: usize,
    closed: usize,
    successful_enqueue: usize,
    disconnected: usize,
    full: usize,
    admission_invariant: usize,
    top_level_wildcard_arms: usize,
    try_send_calls: usize,
}

impl<'ast> syn::visit::Visit<'ast> for InstalledHandoffBodyVisitor {
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if matches!(arm.pat, syn::Pat::Wild(_)) {
            self.top_level_wildcard_arms += 1;
        }
        syn::visit::visit_arm(self, arm);
    }

    fn visit_pat_tuple_struct(&mut self, pattern: &'ast syn::PatTupleStruct) {
        if path_ends_with(&pattern.path, &["TrySendError", "Disconnected"]) {
            self.disconnected += 1;
        }
        if path_ends_with(&pattern.path, &["TrySendError", "Full"]) {
            self.full += 1;
        }
        syn::visit::visit_pat_tuple_struct(self, pattern);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression.path.segments.last().is_some_and(|segment| segment.ident == "candidate") {
            self.candidate_uses += 1;
        }
        for (suffix, count) in [
            (&["T4eHandoffError", "Rejected"][..], &mut self.rejected),
            (&["T4eHandoffError", "Busy"][..], &mut self.busy),
            (&["T4eHandoffError", "Closed"][..], &mut self.closed),
            (&["TrySendError", "Disconnected"][..], &mut self.disconnected),
            (&["TrySendError", "Full"][..], &mut self.full),
            (
                &["ProductionHandoffClosed", "AdmissionInvariant"][..],
                &mut self.admission_invariant,
            ),
        ] {
            if path_ends_with(&expression.path, suffix) {
                *count += 1;
            }
        }
        syn::visit::visit_expr_path(self, expression);
    }

    fn visit_expr_call(&mut self, expression: &'ast syn::ExprCall) {
        if matches!(
            expression.func.as_ref(),
            syn::Expr::Path(function)
                if path_ends_with(&function.path, &["Ok"])
                    && expression.args.len() == 1
                    && matches!(
                        expression.args.first(),
                        Some(syn::Expr::Tuple(tuple)) if tuple.elems.is_empty()
                    )
        ) {
            self.successful_enqueue += 1;
        }
        syn::visit::visit_expr_call(self, expression);
    }

    fn visit_expr_method_call(&mut self, expression: &'ast syn::ExprMethodCall) {
        if expression.method == "try_send" {
            self.try_send_calls += 1;
        }
        syn::visit::visit_expr_method_call(self, expression);
    }
}

fn has_total_installed_handoff_body(
    method: &syn::ImplItemFn,
    aliases: &ProductionAliases,
    module_scope: &[String],
) -> bool {
    use syn::visit::Visit;

    if try_handoff_candidate(method, aliases, module_scope).as_deref() != Some("candidate")
        || method.sig.inputs.len() != 2
        || !matches!(
            method.sig.inputs.first(),
            Some(syn::FnArg::Receiver(receiver))
                if receiver.reference.is_some() && receiver.mutability.is_none()
        )
    {
        return false;
    }

    let mut body = InstalledHandoffBodyVisitor::default();
    body.visit_block(&method.block);
    body.candidate_uses == 1
        && body.rejected == 1
        && body.busy == 1
        && body.closed == 7
        && body.successful_enqueue == 1
        && body.disconnected == 1
        && body.full == 1
        && body.admission_invariant == 2
        && body.top_level_wildcard_arms == 0
        && body.try_send_calls == 1
}

#[test]
fn installed_handoff_mapping_control_and_mutants() {
    const CONTROL: &str = r#"
impl T4eCandidateHandoff for ProductionSimulationHandoff {
    fn try_handoff(&self, candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {
        let Ok(mut state) = self.shared.state.lock() else {
            return Err(T4eHandoffError::Closed);
        };
        let ProductionHandoffState::Open { admission, sender, entrypoint } = &*state else {
            return match &*state {
                ProductionHandoffState::Installing(_) | ProductionHandoffState::Unavailable(_) => {
                    Err(T4eHandoffError::Rejected)
                }
                ProductionHandoffState::Closed { .. } => Err(T4eHandoffError::Closed),
                ProductionHandoffState::Open { .. } => unreachable!("matched open state"),
            };
        };
        if entrypoint.status() != SimulationEntrypointStatus::Ready {
            return Err(T4eHandoffError::Closed);
        }
        match admission.compare_exchange(FREE, OCCUPIED, AcqRel, Acquire) {
            Ok(_) => {}
            Err(OCCUPIED) => return Err(T4eHandoffError::Busy),
            Err(CLOSED) => return Err(T4eHandoffError::Closed),
            Err(_) => {
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::AdmissionInvariant,
                };
                return Err(T4eHandoffError::Closed);
            }
        }
        let reservation = ProductionReservation { admission: Arc::clone(admission) };
        match sender.try_send(AdmittedCandidate { candidate, reservation }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::Disconnected,
                };
                Err(T4eHandoffError::Closed)
            }
            Err(TrySendError::Full(_)) => {
                *state = ProductionHandoffState::Closed {
                    entrypoint: Some(Arc::clone(entrypoint)),
                    reason: ProductionHandoffClosed::AdmissionInvariant,
                };
                Err(T4eHandoffError::Closed)
            }
        }
    }
}
"#;
    let file = syn::parse_file(CONTROL).expect("mapping control parses");
    let syn::Item::Impl(item) = &file.items[0] else {
        panic!("mapping control impl");
    };
    let syn::ImplItem::Fn(method) = &item.items[0] else {
        panic!("mapping control method");
    };
    let mut collector = ProductionAliasCollector::default();
    collector.collect_file(&file, &["arm".to_owned(), "production_handoff".to_owned()], true);
    let aliases = ProductionAliases::resolve(collector);
    assert!(has_total_installed_handoff_body(
        method,
        &aliases,
        &["arm".to_owned(), "production_handoff".to_owned()]
    ));
    eprintln!("U-INSTALL-MAPPING: GREEN");

    for (name, mutant) in [
        (
            "BUSY-AS-REJECTED",
            CONTROL.replacen("T4eHandoffError::Busy", "T4eHandoffError::Rejected", 1),
        ),
        (
            "SUCCESS-AS-REJECTED",
            CONTROL.replacen("Ok(()) => Ok(())", "Ok(()) => Err(T4eHandoffError::Rejected)", 1),
        ),
        (
            "CANDIDATE-BOX-LEAK",
            CONTROL.replacen(
                "        let Ok(mut state)",
                "        Box::leak(Box::new(&candidate));\n        let Ok(mut state)",
                1,
            ),
        ),
        (
            "WILDCARD-STATE",
            CONTROL.replacen(
                "ProductionHandoffState::Closed { .. } => Err(T4eHandoffError::Closed)",
                "_ => Err(T4eHandoffError::Closed)",
                1,
            ),
        ),
    ] {
        assert_ne!(mutant, CONTROL, "{name} patch did not change source");
        let file = syn::parse_file(&mutant).unwrap_or_else(|error| panic!("{name} parses: {error}"));
        let syn::Item::Impl(item) = &file.items[0] else {
            panic!("{name} impl");
        };
        let syn::ImplItem::Fn(method) = &item.items[0] else {
            panic!("{name} method");
        };
        let mut collector = ProductionAliasCollector::default();
        collector.collect_file(&file, &["arm".to_owned(), "production_handoff".to_owned()], true);
        let aliases = ProductionAliases::resolve(collector);
        assert!(
            !has_total_installed_handoff_body(
                method,
                &aliases,
                &["arm".to_owned(), "production_handoff".to_owned()]
            ),
            "{name} mutant remained GREEN"
        );
        eprintln!("U-INSTALL-MAPPING-{name}: RED");
    }
}

#[test]
fn installed_production_handoff_source_satisfies_closed_contract() {
    let source = read(manifest_dir().join("src/arm/production_handoff.rs"));
    validate_install_error_taxonomy(&source).expect("installed production error taxonomy");
    validate_installed_handoff_constructor(&source).expect("installed production constructor");

    let file = syn::parse_file(&source).expect("production handoff parses");
    let scope = ["arm".to_owned(), "production_handoff".to_owned()];
    let mut collector = ProductionAliasCollector::default();
    collector.collect_file(&file, &scope, true);
    let aliases = ProductionAliases::resolve(collector);
    let methods = file
        .items
        .iter()
        .filter_map(|item| {
            let syn::Item::Impl(item) = item else {
                return None;
            };
            let (_, implementation, _) = item.trait_.as_ref()?;
            path_ends_with(implementation, &["T4eCandidateHandoff"]).then_some(item)
        })
        .flat_map(|item| &item.items)
        .filter_map(|item| {
            let syn::ImplItem::Fn(method) = item else {
                return None;
            };
            (method.sig.ident == "try_handoff" && !has_cfg_test(&method.attrs)).then_some(method)
        })
        .collect::<Vec<_>>();
    let [method] = methods.as_slice() else {
        panic!("production try_handoff cardinality changed: {}", methods.len());
    };
    assert!(
        has_total_installed_handoff_body(method, &aliases, &scope),
        "production try_handoff mapping or candidate retention changed"
    );
    eprintln!("U-INSTALLED-PRODUCTION-HANDOFF: GREEN");
}

fn insert_before_test_module(source: &str, addition: &str) -> String {
    source.replacen(
        "#[cfg(test)]\nmod tests {",
        &format!("{addition}\n\n#[cfg(test)]\nmod tests {{"),
        1,
    )
}
/// Enumerates the sealed crate source inventory, intentionally stronger than the
/// active module graph: any orphan `.rs` file under `src/` is unreviewed production
/// source and must not be able to hide an additional handoff implementation.
fn collect_submit_rust_sources(
    directory: &Path,
    sources: &mut Vec<(PathBuf, String)>,
) -> Result<(), String> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| format!("read submit source directory {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read submit source entry: {error}"))?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_submit_rust_sources(&path, sources)?;
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path != manifest_dir().join("src/arm/simulation_entrypoint.rs")
        {
            let source = std::fs::read_to_string(&path)
                .map_err(|error| format!("read submit source {}: {error}", path.display()))?;
            sources.push((path, source));
        }
    }
    Ok(())
}

fn submit_crate_sources() -> Result<Vec<(PathBuf, String)>, String> {
    let mut sources = Vec::new();
    collect_submit_rust_sources(&manifest_dir().join("src"), &mut sources)?;
    Ok(sources)
}

fn validate_installed_production_handoff(
    handoff: &str,
    cli: &str,
    source_override: Option<(&Path, &str)>,
) -> Result<(), String> {
    validate_install_error_taxonomy(handoff)?;
    validate_installed_handoff_constructor(handoff)?;
    for forbidden in [
        "ProductionInstallationDeferred",
        "deferred_production",
        "arm_sim_status:",
        "production installation deferred",
    ] {
        if handoff.contains(forbidden) || cli.contains(forbidden) {
            return Err(format!("installed production retained deferral edge: {forbidden}"));
        }
    }
    for required in [
        "ProductionBundleInputs",
        "ProductionInstallBundle::load",
        "spawn_production_simulation",
        "Duration::from_secs(5)",
        "ProductionInstallDisposition::Ready",
        "InstalledSubmissionBridge::base_mainnet",
        "Arc::clone(&bridge)",
    ] {
        if !cli.contains(required) {
            return Err(format!("CLI installed conjunction missing: {required}"));
        }
    }

    let submit_source_root = manifest_dir().join("src");
    let cli_source_root = manifest_dir().join("../cli/src");
    let cli_path = cli_source_root.join("mev_trader.rs");
    let mut parsed_sources = vec![parse_production_source(
        cli,
        "CLI",
        module_root_for_source(&cli_path, &cli_source_root)?,
        false,
    )?];
    let mut override_applied = source_override.is_none();
    for (path, source) in submit_crate_sources()? {
        let analyzed_source = if let Some((override_path, replacement)) = source_override
            && path.ends_with(override_path)
        {
            override_applied = true;
            replacement
        } else {
            source.as_str()
        };
        let module_root = module_root_for_source(&path, &submit_source_root)?;
        let canonical_source = matches!(
            module_root.as_slice(),
            [arm, simulation_entrypoint]
                if arm == "arm" && simulation_entrypoint == "simulation_entrypoint"
        ) || matches!(
            module_root.as_slice(),
            [arm, production_handoff]
                if arm == "arm" && production_handoff == "production_handoff"
        ) || matches!(
            module_root.as_slice(),
            [tx_authority, bridge]
                if tx_authority == "tx_authority" && bridge == "bridge"
        ) || matches!(
            module_root.as_slice(),
            [arm, witness] if arm == "arm" && witness == "witness"
        );
        parsed_sources.push(parse_production_source(
            analyzed_source,
            &format!("submit source {}", path.display()),
            module_root,
            canonical_source,
        )?);
    }
    if !override_applied {
        return Err("submit source override did not match a crate source file".to_owned());
    }

    let mut collector = ProductionAliasCollector::default();
    for source in &parsed_sources {
        collector.collect_file(
            &source.file,
            &source.module_root,
            source.canonical_source,
        );
    }
    let aliases = ProductionAliases::resolve(collector);
    let analyses =
        parsed_sources.iter().map(|source| analyze_production_handoff(source, &aliases)).collect::<Vec<_>>();

    let opaque_production_items =
        analyses.iter().map(|analysis| analysis.opaque_production_items).sum::<usize>();
    if opaque_production_items != 0 || aliases.opaque_use_globs != 0 {
        return Err(format!(
            "production contains syntax the installed seal cannot resolve: opaque crate-wide items={opaque_production_items}, use globs={}",
            aliases.opaque_use_globs
        ));
    }

    let handoff_impls = analyses.iter().map(|analysis| analysis.handoff_impls).sum::<usize>();
    let target_mismatches =
        analyses.iter().map(|analysis| analysis.handoff_impl_target_mismatches).sum::<usize>();
    let try_handoff_methods =
        analyses.iter().map(|analysis| analysis.try_handoff_methods).sum::<usize>();
    let exact_installed = analyses
        .iter()
        .map(|analysis| analysis.exact_installed_try_handoff_methods)
        .sum::<usize>();
    if handoff_impls != 1
        || target_mismatches != 0
        || try_handoff_methods != 1
        || exact_installed != 1
    {
        return Err(format!(
            "crate-wide installed handoff cardinality changed: impls={handoff_impls}, wrong_targets={target_mismatches}, methods={try_handoff_methods}, exact_installed={exact_installed}"
        ));
    }

    let candidate_fields = analyses.iter().map(|analysis| analysis.candidate_fields).sum::<usize>();
    if candidate_fields != 2 {
        return Err(format!(
            "candidate ownership must be exactly the T4d slot and worker payload: fields={candidate_fields}"
        ));
    }
    let worker_spawn_calls =
        analyses.iter().map(|analysis| analysis.worker_spawn_calls).sum::<usize>();
    if worker_spawn_calls != 1 {
        return Err(format!(
            "SimulationWorker::spawn production call cardinality changed: {worker_spawn_calls}"
        ));
    }
    let ready_calls =
        analyses.iter().map(|analysis| analysis.entrypoint_ready_calls).sum::<usize>();
    let ready_in_spawn = analyses
        .iter()
        .map(|analysis| analysis.entrypoint_ready_calls_in_worker_spawn)
        .sum::<usize>();
    if ready_calls != 1 || ready_in_spawn != 1 {
        return Err(format!(
            "SimulationEntrypoint::ready cardinality/context changed: total={ready_calls}, inside_spawn={ready_in_spawn}"
        ));
    }
    let runtime_open_calls =
        analyses.iter().map(|analysis| analysis.runtime_open_calls).sum::<usize>();
    if runtime_open_calls != 1 {
        return Err(format!("ArmRuntime::open cardinality changed: {runtime_open_calls}"));
    }
    Ok(())
}

#[test]
fn installed_u0_green_and_recursive_whole_crate_mutants_red() {
    let handoff_path = Path::new("src/arm/production_handoff.rs");
    let handoff = read(manifest_dir().join(handoff_path));
    let cli = read(manifest_dir().join("../cli/src/mev_trader.rs"));
    let transport_path = Path::new("src/arm/transport.rs");
    let transport = read(manifest_dir().join(transport_path));

    validate_installed_production_handoff(&handoff, &cli, None)
        .expect("U0 exact installed production handoff");
    eprintln!("U0-INSTALLED: GREEN");

    let box_leak = handoff.replacen(
        "        let Ok(mut state) = self.shared.state.lock() else {",
        "        Box::leak(Box::new(&candidate));\n        let Ok(mut state) = self.shared.state.lock() else {",
        1,
    );
    assert_ne!(box_leak, handoff, "Box::leak patch did not change source");
    assert!(
        validate_installed_production_handoff(
            &box_leak,
            &cli,
            Some((handoff_path, &box_leak))
        )
        .is_err(),
        "#56 candidate Box::leak mutant remained GREEN"
    );
    eprintln!("U-INSTALLED-BOX-LEAK: RED");

    let second_impl = insert_before_test_module(
        &transport,
        "struct SecondProductionHandoff;\n\
         impl T4eCandidateHandoff for SecondProductionHandoff {\n\
         \tfn try_handoff(&self, candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {\n\
         \t\tdrop(candidate);\n\
         \t\tErr(T4eHandoffError::Rejected)\n\
         \t}\n\
         }",
    );
    assert_ne!(second_impl, transport, "second impl patch did not change source");
    assert!(
        validate_installed_production_handoff(
            &handoff,
            &cli,
            Some((transport_path, &second_impl))
        )
        .is_err(),
        "second crate-wide implementation remained GREEN"
    );
    eprintln!("U-INSTALLED-SECOND-IMPL: RED");

    let aliased_impl = insert_before_test_module(
        &transport,
        "use crate::T4eCandidateHandoff as HiddenHandoff;\n\
         struct AliasedProductionHandoff;\n\
         impl HiddenHandoff for AliasedProductionHandoff {\n\
         \tfn try_handoff(&self, candidate: SealedUnsignedCandidate) -> Result<(), T4eHandoffError> {\n\
         \t\tdrop(candidate);\n\
         \t\tErr(T4eHandoffError::Rejected)\n\
         \t}\n\
         }",
    );
    assert_ne!(aliased_impl, transport, "aliased impl patch did not change source");
    assert!(
        validate_installed_production_handoff(
            &handoff,
            &cli,
            Some((transport_path, &aliased_impl))
        )
        .is_err(),
        "aliased second implementation remained GREEN"
    );
    eprintln!("U-INSTALLED-ALIASED-IMPL: RED");

    let qself_target = handoff.replacen(
        "impl T4eCandidateHandoff for ProductionSimulationHandoff {",
        "impl T4eCandidateHandoff for <() as HandoffCarrier>::ProductionSimulationHandoff {",
        1,
    );
    let qself_target = qself_target.replacen(
        "impl T4eCandidateHandoff for <() as HandoffCarrier>::ProductionSimulationHandoff {",
        "trait HandoffCarrier { type ProductionSimulationHandoff; }\n\
         struct ProjectedHandoff;\n\
         impl HandoffCarrier for () { type ProductionSimulationHandoff = ProjectedHandoff; }\n\
         impl T4eCandidateHandoff for <() as HandoffCarrier>::ProductionSimulationHandoff {",
        1,
    );
    assert_ne!(qself_target, handoff, "qself target patch did not change source");
    assert!(
        validate_installed_production_handoff(
            &qself_target,
            &cli,
            Some((handoff_path, &qself_target))
        )
        .is_err(),
        "qself replacement target remained GREEN"
    );
    eprintln!("U-INSTALLED-QSELF-TARGET: RED");

    let extra_owner = insert_before_test_module(
        &transport,
        "struct RetainedCandidateOwner { candidate: SealedUnsignedCandidate }",
    );
    assert_ne!(extra_owner, transport, "extra owner patch did not change source");
    assert!(
        validate_installed_production_handoff(
            &handoff,
            &cli,
            Some((transport_path, &extra_owner))
        )
        .is_err(),
        "third candidate owner remained GREEN"
    );
    eprintln!("U-INSTALLED-CANDIDATE-OWNER: RED");

    for (name, addition) in [
        ("MACRO", "install_production_simulation!();"),
        ("PATH", "#[path = \"hidden.rs\"] mod hidden;"),
        ("GLOB", "use crate::arm::*;"),
    ] {
        let mutant = insert_before_test_module(&transport, addition);
        assert_ne!(mutant, transport, "{name} patch did not change source");
        assert!(
            validate_installed_production_handoff(
                &handoff,
                &cli,
                Some((transport_path, &mutant))
            )
            .is_err(),
            "{name} resolution escape remained GREEN"
        );
        eprintln!("U-INSTALLED-{name}: RED");
    }
}
