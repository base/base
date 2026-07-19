//! Capability-absent-by-construction checks for the trader crate.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

const NORMAL_DEPENDENCIES: [&str; 17] = [
    "alloy-consensus",
    "alloy-eips",
    "alloy-primitives",
    "alloy-rpc-types-engine",
    "base-common-consensus",
    "base-execution-chainspec",
    "base-execution-evm",
    "rayon",
    "reth-evm",
    "reth-provider",
    "reth-revm",
    "revm",
    "revm-bytecode",
    "revm-database",
    "sha2",
    "thiserror",
    "tracing",
];
const SOURCE_FILES: [&str; 10] = [
    "frame.rs",
    "latency.rs",
    "lib.rs",
    "lifecycle.rs",
    "oracle.rs",
    "pairwise.rs",
    "port.rs",
    "registry.rs",
    "runtime.rs",
    "storage.rs",
];
const FORBIDDEN_DEPENDENCY_PREFIXES: [&str; 18] = [
    "alloy-json-rpc",
    "alloy-network",
    "alloy-provider",
    "alloy-pubsub",
    "alloy-rpc-client",
    "alloy-signer",
    "alloy-transport",
    "base-common-signer",
    "base-execution-txpool",
    "base-flashblocks",
    "base-tx-forwarding",
    "jsonrpsee",
    "reth-network",
    "reth-rpc",
    "reth-transaction-pool",
    "secp256k1",
    "tokio-tungstenite",
    "tungstenite",
];
const FORBIDDEN_IDENTIFIERS: [&str; 38] = [
    "AwsSigner",
    "Encodable2718",
    "GcpSigner",
    "LedgerSigner",
    "LocalSigner",
    "MnemonicBuilder",
    "PrivateKeySigner",
    "add_external_transaction",
    "add_transaction",
    "add_transactions",
    "encode_2718",
    "encode_enveloped",
    "encoded_2718",
    "eth_sendBundle",
    "eth_sendRawTransaction",
    "forward_transaction",
    "forward_transactions",
    "from_mnemonic",
    "insert_transaction",
    "insert_transactions",
    "into_signed",
    "network_encode",
    "private_key",
    "sendBundle",
    "sendRawTransaction",
    "send_batch",
    "send_bundle",
    "send_raw_transaction",
    "send_with_retries",
    "sign_dynamic_typed_data",
    "sign_hash",
    "sign_message",
    "sign_transaction",
    "signature_hash",
    "with_signature",
    "encode_enveloped",
    "network_encode",
    "encoded_2718",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().expect("workspace root")
}

fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked", "--offline"])
        .current_dir(workspace_root())
        .output()
        .expect("cargo metadata must run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata JSON")
}

fn dependency_features() -> BTreeMap<&'static str, (&'static [&'static str], bool)> {
    BTreeMap::from([
        ("alloy-consensus", (&["k256", "std"][..], false)),
        ("alloy-eips", (&["std"][..], false)),
        ("alloy-primitives", (&["std"][..], false)),
        ("alloy-rpc-types-engine", (&[][..], false)),
        ("base-common-consensus", (&["evm", "std"][..], false)),
        ("base-execution-chainspec", (&[][..], true)),
        ("base-execution-evm", (&[][..], true)),
        ("rayon", (&[][..], true)),
        ("reth-evm", (&[][..], true)),
        ("reth-provider", (&[][..], true)),
        ("reth-revm", (&[][..], true)),
        ("revm", (&["std"][..], false)),
        ("revm-bytecode", (&["std"][..], false)),
        ("revm-database", (&["std"][..], false)),
        ("sha2", (&["std"][..], false)),
        ("thiserror", (&["std"][..], false)),
        ("tracing", (&["std"][..], false)),
    ])
}

#[test]
fn metadata_has_exact_target_and_dependency_shape() {
    let metadata = metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let package = packages
        .iter()
        .find(|package| package["name"] == "base-mev-trader")
        .expect("base-mev-trader package");

    let targets = package["targets"].as_array().expect("targets");
    assert_eq!(targets.iter().filter(|target| target["kind"][0] == "lib").count(), 1);
    for target in targets {
        let kind = target["kind"].as_array().expect("target kind");
        assert!(kind == &[Value::String("lib".into())] || kind == &[Value::String("test".into())]);
        if kind == &[Value::String("lib".into())] {
            assert_eq!(
                target["crate_types"].as_array().expect("crate types"),
                &[Value::String("lib".into())]
            );
        }
    }

    let dependencies = package["dependencies"].as_array().expect("dependencies");
    let normal: BTreeSet<_> = dependencies
        .iter()
        .filter(|dependency| dependency["kind"].is_null())
        .map(|dependency| dependency["name"].as_str().expect("dependency name"))
        .collect();
    assert_eq!(normal, BTreeSet::from(NORMAL_DEPENDENCIES));
    let development: BTreeSet<_> = dependencies
        .iter()
        .filter(|dependency| dependency["kind"] == "dev")
        .map(|dependency| dependency["name"].as_str().expect("dependency name"))
        .collect();
    assert_eq!(development, BTreeSet::from(["serde_json"]));
    assert!(!dependencies.iter().any(|dependency| dependency["kind"] == "build"));

    let expected = dependency_features();
    for dependency in dependencies.iter().filter(|dependency| dependency["kind"].is_null()) {
        let name = dependency["name"].as_str().expect("dependency name");
        let (features, default_features) = expected[name];
        let actual_features: BTreeSet<_> = dependency["features"]
            .as_array()
            .expect("features")
            .iter()
            .map(|feature| feature.as_str().expect("feature"))
            .collect();
        assert_eq!(actual_features, features.iter().copied().collect(), "features for {name}");
        assert_eq!(dependency["uses_default_features"], default_features, "defaults for {name}");
        assert!(dependency["rename"].is_null(), "renamed dependency {name}");
        assert!(dependency["target"].is_null(), "target-specific dependency {name}");
        assert_eq!(dependency["optional"], false, "optional dependency {name}");
        assert!(
            !FORBIDDEN_DEPENDENCY_PREFIXES
                .iter()
                .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}-"))),
            "forbidden direct dependency {name}"
        );
    }
}

#[test]
fn lockfile_contains_exact_resolved_pins() {
    let lock = fs::read_to_string(workspace_root().join("Cargo.lock")).expect("Cargo.lock");
    let packages = lock.split("[[package]]").skip(1).collect::<Vec<_>>();
    let expected = [
        ("revm", "40.0.3", "registry+https://github.com/rust-lang/crates.io-index"),
        ("revm-database", "15.0.2", "registry+https://github.com/rust-lang/crates.io-index"),
        ("revm-bytecode", "11.0.1", "registry+https://github.com/rust-lang/crates.io-index"),
        ("rayon", "1.12.0", "registry+https://github.com/rust-lang/crates.io-index"),
        ("sha2", "0.10.9", "registry+https://github.com/rust-lang/crates.io-index"),
        ("thiserror", "2.0.18", "registry+https://github.com/rust-lang/crates.io-index"),
        ("tracing", "0.1.44", "registry+https://github.com/rust-lang/crates.io-index"),
        ("serde_json", "1.0.150", "registry+https://github.com/rust-lang/crates.io-index"),
    ];
    for (name, version, source) in expected {
        assert!(
            packages.iter().any(|package| {
                package.contains(&format!("\nname = \"{name}\"\n"))
                    && package.contains(&format!("\nversion = \"{version}\"\n"))
                    && package.contains(&format!("\nsource = \"{source}\"\n"))
            }),
            "missing resolved pin {name} {version}"
        );
    }
    let reth_source = "git+https://github.com/paradigmxyz/reth?tag=v2.3.0#9384bc53d8c0c77e59cac83fdaaf3b372c6d2216";
    for name in ["reth-evm", "reth-provider", "reth-revm"] {
        assert!(
            packages.iter().any(|package| {
                package.contains(&format!("\nname = \"{name}\"\n"))
                    && package.contains("\nversion = \"2.3.0\"\n")
                    && package.contains(&format!("\nsource = \"{reth_source}\"\n"))
            }),
            "missing Reth pin {name}"
        );
    }
}

fn raw_string_end(bytes: &[u8], start: usize) -> Result<Option<usize>, String> {
    let mut cursor = start;
    if bytes.get(cursor) == Some(&b'b') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return Ok(None);
    }
    cursor += 1;
    let hash_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return Ok(None);
    }
    let hashes = cursor - hash_start;
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes.get(cursor + 1..cursor + 1 + hashes)
                == Some(&bytes[hash_start..hash_start + hashes])
        {
            return Ok(Some(cursor + 1 + hashes));
        }
        cursor += 1;
    }
    Err("unterminated raw string".into())
}

fn identifiers(source: &str) -> Result<BTreeSet<String>, String> {
    let bytes = source.as_bytes();
    let mut identifiers = BTreeSet::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if let Some(end) = raw_string_end(bytes, cursor)? {
            cursor = end;
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"//") {
            cursor += 2;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"/*") {
            cursor += 2;
            let mut depth = 1usize;
            while cursor < bytes.len() && depth != 0 {
                if bytes.get(cursor..cursor + 2) == Some(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes.get(cursor..cursor + 2) == Some(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            if depth != 0 {
                return Err("unterminated block comment".into());
            }
            continue;
        }
        if bytes[cursor] == b'"' {
            cursor += 1;
            let mut closed = false;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor = cursor.saturating_add(2),
                    b'"' => {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            if !closed {
                return Err("unterminated string".into());
            }
            continue;
        }
        if bytes[cursor] == b'\'' {
            let mut end = cursor + 1;
            let mut closed = false;
            while end < bytes.len() && bytes[end] != b'\n' {
                match bytes[end] {
                    b'\\' => end = end.saturating_add(2),
                    b'\'' => {
                        end += 1;
                        closed = true;
                        break;
                    }
                    _ => end += 1,
                }
            }
            if closed {
                cursor = end;
                continue;
            }
            cursor += 1;
            continue;
        }
        if bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_' {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
            {
                cursor += 1;
            }
            identifiers.insert(source[start..cursor].to_owned());
            continue;
        }
        cursor += 1;
    }
    Ok(identifiers)
}

#[test]
fn production_source_has_exact_files_and_no_forbidden_identifiers() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let actual: BTreeSet<_> = fs::read_dir(&source_root)
        .expect("source directory")
        .map(|entry| entry.expect("source entry"))
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "rs"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(actual, SOURCE_FILES.iter().map(|name| (*name).to_owned()).collect());

    for file in SOURCE_FILES {
        let source = fs::read_to_string(source_root.join(file)).expect("source file");
        let identifiers =
            identifiers(&source).unwrap_or_else(|error| panic!("malformed {file}: {error}"));
        for forbidden in FORBIDDEN_IDENTIFIERS {
            assert!(!identifiers.contains(forbidden), "forbidden identifier {forbidden} in {file}");
        }
    }
}

#[test]
fn scanner_ignores_non_code_and_rejects_malformed_source() {
    let source = r###"
        // send_raw_transaction
        /* nested /* PrivateKeySigner */ comment */
        const A: &str = "sign_hash";
        const B: &str = r##"insert_transaction"##;
        fn permitted_identifier() {}
    "###;
    let found = identifiers(source).expect("valid source");
    assert!(found.contains("permitted_identifier"));
    assert!(!found.contains("send_raw_transaction"));
    assert!(!found.contains("PrivateKeySigner"));
    assert!(!found.contains("sign_hash"));
    assert!(!found.contains("insert_transaction"));
    assert!(identifiers("/* unterminated").is_err());
    assert!(identifiers("\"unterminated").is_err());
    assert!(identifiers("r###\"unterminated").is_err());
}

#[test]
fn runtime_wiring_is_idle_and_preserves_forwarding_semantics() {
    let root = workspace_root();
    let adapter = fs::read_to_string(root.join("crates/execution/cli/src/mev_trader.rs"))
        .expect("CLI adapter");
    let standard = fs::read_to_string(root.join("crates/execution/cli/src/standard_node.rs"))
        .expect("standard node");
    let runtime = fs::read_to_string(root.join("crates/execution/mev-trader/src/runtime.rs"))
        .expect("trader runtime");

    let adapter_identifiers = identifiers(&adapter).expect("well-formed CLI adapter");
    for forbidden in FORBIDDEN_IDENTIFIERS {
        assert!(
            !adapter_identifiers.contains(forbidden),
            "forbidden identifier {forbidden} in CLI trader adapter"
        );
    }
    for forbidden in
        ["FlashblocksSubscriber", "Sender", "VictimFrame", "websocket", "credential", "transport"]
    {
        assert!(
            !adapter_identifiers.contains(forbidden),
            "live producer surface {forbidden} in CLI trader adapter"
        );
    }

    assert_eq!(adapter.matches("tokio::spawn").count(), 2);
    assert_eq!(adapter.matches("subscribe_to_flashblocks").count(), 1);
    assert!(runtime.contains("A0_MEASUREMENT_IDLE_NO_LIVE_INGRESS"));
    assert!(runtime.contains("FixturePoolRegistry::new(descriptors, digest)"));

    let bundle = "runner.install_ext::<BundleExtension>(());";
    let forwarding = "runner.install_ext::<TxForwardingExtension>((&args).into());";
    let installer = "MevTraderPhaseAInstaller::maybe_install(";
    let protected_comment =
        "// Issue #45: clone the shared FlashblocksState BEFORE the config is moved";
    assert_eq!(standard.matches(forwarding).count(), 1);
    assert_eq!(standard.matches(installer).count(), 1);
    assert!(standard.find(bundle) < standard.find(forwarding));
    assert!(standard.find(forwarding) < standard.find(installer));
    assert!(standard.find(installer) < standard.find(protected_comment));
}
