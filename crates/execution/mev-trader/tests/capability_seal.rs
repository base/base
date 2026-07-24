//! Capability-absent-by-construction checks for the trader crate.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

const NORMAL_DEPENDENCIES: [&str; 24] = [
    "alloy-consensus",
    "alloy-eips",
    "alloy-primitives",
    "alloy-rpc-types-engine",
    "base-common-consensus",
    "base-execution-chainspec",
    "base-execution-evm",
    "futures",
    "getrandom",
    "rayon",
    "redb",
    "reth-evm",
    "reth-provider",
    "reth-revm",
    "revm",
    "revm-bytecode",
    "revm-database",
    "serde",
    "serde_json",
    "sha2",
    "thiserror",
    "tokio",
    "tokio-tungstenite",
    "tracing",
];
const SOURCE_FILES: [&str; 15] = [
    "blink_ingress.rs",
    "edge_measurement.rs",
    "frame.rs",
    "latency.rs",
    "lib.rs",
    "lifecycle.rs",
    "measurement_tx.rs",
    "oracle.rs",
    "pairwise.rs",
    "port.rs",
    "preparation.rs",
    "registry.rs",
    "runtime.rs",
    "safety.rs",
    "storage.rs",
];
const KILLSTATE_ANCHOR_FILES: [&str; 3] = ["external_store.rs", "mod.rs", "types.rs"];
const FORBIDDEN_DEPENDENCY_PREFIXES: [&str; 25] = [
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
    "futures-util",
    "jsonrpsee",
    "libc",
    "native-tls",
    "openssl",
    "reth-network",
    "reth-rpc",
    "reth-transaction-pool",
    "rustls",
    "secp256k1",
    "tokio-native-tls",
    "tokio-rustls",
    "tungstenite",
    "url",
];
const FORBIDDEN_IDENTIFIERS: [&str; 35] = [
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
        // B1 safety adds owner-signature VERIFY-only recovery (EIP-191
        // recover_address_from_msg), which is gated behind alloy-primitives' k256
        // feature. This is recover-only; the signing-capable `secp256k1` crate
        // remains a forbidden dependency prefix.
        ("alloy-primitives", (&["k256", "std"][..], false)),
        ("alloy-rpc-types-engine", (&[][..], false)),
        ("base-common-consensus", (&["evm", "std"][..], false)),
        ("base-execution-chainspec", (&[][..], true)),
        ("base-execution-evm", (&[][..], true)),
        ("futures", (&[][..], true)),
        ("getrandom", (&[][..], true)),
        ("rayon", (&[][..], true)),
        // R9 victim at-most-once claim store: node-local redb, no network/keys.
        ("redb", (&[][..], true)),
        ("reth-evm", (&[][..], true)),
        ("reth-provider", (&[][..], true)),
        ("reth-revm", (&[][..], true)),
        ("revm", (&["std"][..], false)),
        ("revm-bytecode", (&["std"][..], false)),
        ("revm-database", (&["std"][..], false)),
        ("serde", (&["derive", "std"][..], false)),
        ("serde_json", (&["std"][..], false)),
        ("sha2", (&["std"][..], false)),
        ("thiserror", (&["std"][..], false)),
        ("tokio", (&["net", "rt", "sync", "time"][..], true)),
        ("tokio-tungstenite", (&["native-tls"][..], true)),
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
    assert!(development.is_empty());
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
        ("redb", "2.6.3", "registry+https://github.com/rust-lang/crates.io-index"),
        ("sha2", "0.10.9", "registry+https://github.com/rust-lang/crates.io-index"),
        ("thiserror", "2.0.18", "registry+https://github.com/rust-lang/crates.io-index"),
        ("tracing", "0.1.44", "registry+https://github.com/rust-lang/crates.io-index"),
        ("futures", "0.3.32", "registry+https://github.com/rust-lang/crates.io-index"),
        ("getrandom", "0.4.2", "registry+https://github.com/rust-lang/crates.io-index"),
        ("serde", "1.0.228", "registry+https://github.com/rust-lang/crates.io-index"),
        ("serde_json", "1.0.150", "registry+https://github.com/rust-lang/crates.io-index"),
        ("tokio", "1.52.3", "registry+https://github.com/rust-lang/crates.io-index"),
        ("tokio-tungstenite", "0.28.0", "registry+https://github.com/rust-lang/crates.io-index"),
        ("tungstenite", "0.28.0", "registry+https://github.com/rust-lang/crates.io-index"),
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
        let production = if file == "blink_ingress.rs" {
            let marker = "\n#[cfg(test)]\nmod tests {";
            assert_eq!(source.matches(marker).count(), 1, "sole final test module marker");
            let (production, _) = source.split_once(marker).expect("Blink production prefix");
            production
        } else {
            source.as_str()
        };
        let identifiers =
            identifiers(production).unwrap_or_else(|error| panic!("malformed {file}: {error}"));
        for forbidden in FORBIDDEN_IDENTIFIERS {
            assert!(!identifiers.contains(forbidden), "forbidden identifier {forbidden} in {file}");
        }

        if file == "blink_ingress.rs" {
            assert_eq!(production.matches("wss://baseauction.blinklabs.xyz/ws/v1/").count(), 1);
            assert_eq!(production.matches("blink_partialPendingTransactions").count(), 1);
            assert_eq!(
                production.matches("socket.send(Message::Text(BLINK_SUBSCRIBE.into()))").count(),
                1
            );
            assert_eq!(production.matches(".send(").count(), 1, "one audited sink send");
            assert_eq!(
                production
                    .matches(
                        "connect_async_tls_with_config(request, Some(websocket_config), false, None)",
                    )
                    .count(),
                1
            );
            assert_eq!(production.matches("String::with_capacity").count(), 1);
            assert_eq!(production.matches("format!(").count(), 0);
            for forbidden_constructor in [
                ".send(Message::Binary",
                ".send(Message::Ping",
                ".send(Message::Pong",
                ".send(Message::Close",
            ] {
                assert_eq!(production.matches(forbidden_constructor).count(), 0);
            }
            for forbidden_accessor in [
                "pub fn socket",
                "pub fn sink",
                "pub fn credential",
                "impl fmt::Display for BlinkCredential",
            ] {
                assert!(!production.contains(forbidden_accessor));
            }
        }
    }
}

#[test]
fn a2_proof_binding_and_test_support_have_bounded_api_shape() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let frame = fs::read_to_string(root.join("src/frame.rs")).expect("frame source");
    let registry = fs::read_to_string(root.join("src/registry.rs")).expect("registry source");
    let pairwise = fs::read_to_string(root.join("src/pairwise.rs")).expect("pairwise source");
    let latency = fs::read_to_string(root.join("src/latency.rs")).expect("latency source");
    let crate_root = fs::read_to_string(root.join("src/lib.rs")).expect("crate root");
    let runtime = fs::read_to_string(root.join("src/runtime.rs")).expect("runtime source");
    let runtime_production = runtime
        .split_once("\n#[cfg(test)]\nmod tests {")
        .map_or(runtime.as_str(), |(source, _)| source);
    let integration =
        fs::read_to_string(root.join("tests/deterministic_e2e.rs")).expect("integration source");

    let frame_support_marker = "\n#[cfg(test)]\npub(crate) mod test_utils {";
    assert_eq!(frame.matches(frame_support_marker).count(), 1);
    let (frame_production, frame_support_and_tests) =
        frame.split_once(frame_support_marker).expect("frame test support");
    let (frame_support, _) = frame_support_and_tests
        .split_once("\n#[cfg(test)]\nmod tests {")
        .expect("final frame tests");

    assert!(frame_production.contains(
        "pub struct ProcessedFrame {\n    materialized_state: MaterializedState,\n    measurement_context: MeasurementContext,\n    dirty_pools: DirtyPoolSet,\n}"
    ));
    assert_eq!(frame_production.matches("impl ProcessedFrame {").count(), 1);
    assert_eq!(frame_production.matches("pub const fn materialized_state").count(), 1);
    assert_eq!(frame_production.matches("pub const fn measurement_context").count(), 1);
    assert_eq!(frame_production.matches("pub const fn dirty_pools").count(), 2);
    assert_eq!(frame_production.matches("Ok(Some(ProcessedFrame {").count(), 1);
    assert!(!frame_production.contains("impl Default for ProcessedFrame"));
    assert!(!frame_production.contains("&mut MaterializedState"));
    assert!(!frame_production.contains("&mut MeasurementContext"));
    let final_authority = frame_production
        .find("let current_authority = port.is_current_authoritative(snapshot);")
        .expect("final authority check");
    let proof_construction =
        frame_production.find("Ok(Some(ProcessedFrame {").expect("sole proof construction");
    assert!(final_authority < proof_construction);

    assert_eq!(
        frame_production
            .matches("!matches!(output.result, ExecutionResult::Success { .. })")
            .count(),
        1
    );
    assert_eq!(frame_production.matches("commit.commit(evm.db_mut(), output.state)?;").count(), 1);
    assert_eq!(frame_production.matches("evm.db_mut().commit(").count(), 0);
    let delta_validation = frame_production
        .find("DeltaGuard::validate_and_classify(&output.state, audit)")
        .expect("delta validation");
    let sole_commit = frame_production
        .find("commit.commit(evm.db_mut(), output.state)?;")
        .expect("sole guarded commit");
    let materialization = frame_production
        .find("StateMaterializer::materialize(")
        .expect("post-commit materialization");
    assert!(delta_validation < sole_commit);
    assert!(sole_commit < materialization);

    const RAW_VICTIM: &str = "f8628080830186a0940000000000000000000000000000000000000000808082422da0840cfc572845f5786e702984c2a582528cad4b49b2a10b9db1be7fca90058565a025e7109ceb98168d95b09b18bbf6b685130e0562f233877d492b94eee0c5b6d1";
    assert_eq!(frame.matches(RAW_VICTIM).count(), 1);
    assert_eq!(frame_support.matches("BaseTxEnvelope::decode_2718_exact").count(), 1);
    assert_eq!(frame_support.matches("keccak256(&raw_tx)").count(), 1);
    assert_eq!(frame_support.matches("transaction.recover_signer()").count(), 1);
    assert_eq!(
        frame_support.matches("Box::new(reth_provider::noop::NoopProvider::default())").count(),
        1
    );
    assert_eq!(frame_support.matches("FrameProcessor::process(").count(), 1);
    assert_eq!(frame_support.matches("Ok(Some(ProcessedFrame {").count(), 0);
    assert!(!frame_support.contains("impl ProcessedFrame"));
    assert!(!frame_support.contains("BackrunPlan"));

    let registry_support_marker = "\n#[cfg(test)]\npub(crate) mod test_utils {";
    assert_eq!(registry.matches(registry_support_marker).count(), 1);
    let (_, registry_support_and_tests) =
        registry.split_once(registry_support_marker).expect("registry test support");
    let (registry_support, _) = registry_support_and_tests
        .split_once("\n#[cfg(test)]\nmod tests {")
        .expect("final registry tests");
    assert_eq!(registry_support.matches("pub(crate) fn audited_sender_nonce").count(), 1);
    assert_eq!(registry_support.matches("AuditedWriteKey::AccountNonce").count(), 1);
    assert_eq!(registry_support.matches("const NONCE_EVIDENCE_DIGEST").count(), 1);
    assert!(registry_support.contains("B256::new([0x5a; 32])"));
    for forbidden in [
        "AccountBalance",
        "AuditedWriteKey::Storage",
        "ProcessedFrame",
        "MeasurementContext",
        "BackrunPlan",
    ] {
        assert!(!registry_support.contains(forbidden), "registry test support exposes {forbidden}");
    }

    assert_eq!(pairwise.matches("pub fn select_measurement(").count(), 1);
    assert!(pairwise.contains(
        "pub fn select_measurement(\n        processed: &ProcessedFrame,\n        candidates: &[PairwiseCandidate],"
    ));
    assert_eq!(pairwise.matches(") -> Result<Option<BackrunPlan>, PairwiseError> {").count(), 1);
    assert_eq!(pairwise.matches("let mut plan = BackrunPlan {").count(), 1);
    assert_eq!(pairwise.matches("pub struct MeasurementEncoder;").count(), 1);
    assert_eq!(pairwise.matches("impl MeasurementEncoder {").count(), 1);
    assert_eq!(pairwise.matches("pub fn encode(plan: &BackrunPlan)").count(), 1);
    assert!(!pairwise.contains("context: MeasurementContext"));
    assert!(!pairwise.contains("impl BackrunPlan {"));

    assert!(crate_root.contains(
        "DeltaError, DirtyPoolSet, FrameCommitGuard, FrameProcessor, MAX_FRAME_AGE_MILLIS,"
    ));
    assert!(crate_root.contains(
        "MAX_RAW_FRAME_BYTES, ProcessedFrame, SnapshotCoherence, ValidatedFrameDelta, VictimFrame,"
    ));
    assert!(!crate_root.contains("test_utils"));
    assert_eq!(runtime_production.matches("PairwiseEngine::discover(").count(), 1);
    assert_eq!(runtime_production.matches("PairwiseEngine::select_measurement(").count(), 1);
    assert_eq!(runtime_production.matches("PoolStatePreparer::prepare(").count(), 1);
    for forbidden in ["select_measurement", "MeasurementContext", "BackrunPlan", "ProcessedFrame"] {
        assert!(!integration.contains(forbidden), "integration bypass surface {forbidden}");
    }

    assert_eq!(latency.matches("#[ignore = ").count(), 1);
    assert_eq!(
        latency
            .matches("fn release_fixture_uses_ten_warmups_one_hundred_samples_and_drains()")
            .count(),
        1
    );
}

#[test]
fn scanner_ignores_non_code_and_rejects_malformed_source() {
    let unique_forbidden_identifiers: BTreeSet<_> = FORBIDDEN_IDENTIFIERS.iter().copied().collect();
    assert_eq!(unique_forbidden_identifiers.len(), FORBIDDEN_IDENTIFIERS.len());
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

/// Production source with any trailing `#[cfg(test)]` items removed.
fn production_prefix(source: &str) -> &str {
    source.split_once("\n#[cfg(test)]").map_or(source, |(prefix, _)| prefix)
}

#[test]
fn r9_claim_store_is_sealed_and_bootstrap_is_provisioning_gated() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/victim_claim");
    let module_files = ["mod.rs", "store.rs", "types.rs"];

    // The provisioning surface (`bootstrap`) must be gated behind the
    // `r9-provisioning` feature (plus cfg(test)), so the default live-node
    // build never compiles it.
    let modrs = fs::read_to_string(src.join("mod.rs")).expect("victim_claim mod.rs");
    assert!(
        modrs.contains("#[cfg(any(test, feature = \"r9-provisioning\"))]\n    pub fn bootstrap("),
        "VictimClaimStore::bootstrap must be gated behind the r9-provisioning feature"
    );

    // No production (non-test) callsite of `bootstrap` may exist anywhere in the
    // crate: the only permitted `bootstrap(` token is the gated definition.
    let mut production_files: Vec<PathBuf> = SOURCE_FILES
        .iter()
        .map(|name| Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name))
        .collect();
    production_files.extend(module_files.iter().map(|name| src.join(name)));
    for path in &production_files {
        let source = fs::read_to_string(path).unwrap_or_else(|_| panic!("source {path:?}"));
        let production = production_prefix(&source);
        let calls = production.matches("bootstrap(").count();
        let definitions = production.matches("pub fn bootstrap(").count();
        assert_eq!(calls, definitions, "unexpected production bootstrap() callsite in {path:?}");
    }

    // The victim_claim production source carries none of the forbidden
    // capability identifiers (the seal's SOURCE_FILES scan is top-level only).
    for name in module_files {
        let source = fs::read_to_string(src.join(name)).unwrap_or_else(|_| panic!("source {name}"));
        let production = production_prefix(&source);
        let identifiers =
            identifiers(production).unwrap_or_else(|error| panic!("malformed {name}: {error}"));
        for forbidden in FORBIDDEN_IDENTIFIERS {
            assert!(
                !identifiers.contains(forbidden),
                "forbidden identifier {forbidden} in victim_claim/{name}"
            );
        }
    }
}

#[test]
fn p0a_killstate_anchor_has_exact_nested_surface_and_transition_owner() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src/killstate_anchor");
    let actual: BTreeSet<_> = fs::read_dir(&src)
        .expect("killstate_anchor directory")
        .map(|entry| entry.expect("anchor source entry"))
        .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "rs"))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(actual, KILLSTATE_ANCHOR_FILES.iter().map(|name| (*name).to_owned()).collect());
    assert!(!actual.contains("local.rs"));

    for name in KILLSTATE_ANCHOR_FILES {
        let source = fs::read_to_string(src.join(name)).expect("anchor source");
        let found =
            identifiers(&source).unwrap_or_else(|error| panic!("malformed {name}: {error}"));
        for forbidden in FORBIDDEN_IDENTIFIERS {
            assert!(
                !found.contains(forbidden),
                "forbidden identifier {forbidden} in killstate_anchor/{name}"
            );
        }
    }

    let module = fs::read_to_string(src.join("mod.rs")).expect("anchor module");
    let external = fs::read_to_string(src.join("external_store.rs")).expect("external store");
    let safety = fs::read_to_string(root.join("src/safety.rs")).expect("safety source");
    let crate_root = fs::read_to_string(root.join("src/lib.rs")).expect("crate root");

    assert_eq!(module.matches("impl KillStateStore for AnchoredKillStateStore").count(), 1);
    assert_eq!(safety.matches("impl KillStateStore for FileKillStateStore").count(), 1);
    assert!(safety.contains("#[cfg(test)]\nimpl KillStateStore for FileKillStateStore"));
    assert!(
        safety.contains(
            "#[cfg(test)]\n#[derive(Debug, Clone)]\npub(crate) struct FileKillStateStore"
        )
    );
    assert!(!crate_root.contains("FileKillStateStore"));
    assert!(!module.contains("pub fn new("));
    assert!(!module.contains("pub fn open_for_test("));
    assert!(!module.contains("pub fn from_opened("));
    assert_eq!(crate_root.matches("open_anchored_killstate").count(), 1);

    for lifecycle in ["bootstrap", "activate"] {
        assert!(
            external.contains(&format!(
                "#[cfg(any(test, feature = \"p0-provisioning\"))]\nimpl AnchorProvisioner"
            )),
            "{lifecycle} must be confined to the provisioning implementation"
        );
        assert_eq!(
            external.matches(&format!("pub fn {lifecycle}(")).count(),
            1,
            "one feature-gated provisioning definition for {lifecycle}"
        );
        assert!(
            crate_root.contains(
                "#[cfg(any(test, feature = \"p0-provisioning\"))]\npub use killstate_anchor::{AnchorProvisioner, BootstrapEvidence, SeedAuthorization};"
            ),
            "provisioning re-export must be absent from default builds"
        );
        let test_module = module.find("\n#[cfg(test)]\nmod tests").expect("final test module");
        for (callsite, _) in module.match_indices(&format!("AnchorProvisioner::{lifecycle}(")) {
            assert!(
                callsite > test_module,
                "production lifecycle callsite in the transition owner"
            );
        }
    }

    let open_start = external.find("fn open_existing_at(").expect("open_existing implementation");
    let open_tail = &external[open_start..];
    let open_end = open_tail
        .find("\n}\n\n#[cfg(any(test, feature = \"p0-provisioning\"))]")
        .expect("open_existing end");
    let open_existing = &open_tail[..open_end];
    for forbidden in
        ["create(", "create_new", "truncate(true)", "bootstrap", "activate", "adopt", "reseed"]
    {
        assert!(
            !open_existing.contains(forbidden),
            "production open_existing contains forbidden lifecycle token {forbidden}"
        );
    }
    assert!(open_existing.contains("open_database_file(&paths.db_path, false)"));
    assert!(open_existing.contains("db_path: paths.db_path"));
    assert!(open_existing.contains("leaf_identity: opened.leaf_identity"));
    assert!(external.contains("options.read(true).write(true).create(false).truncate(false);"));
    assert!(external.contains("let db = redb::Database::open(path)"));
    assert!(!external.contains("redb::Database::create(path)"));
    assert!(external.contains("let process_uid = process_uid()?;"));
    assert!(external.contains("if actual_uid != expected_uid"));
    assert!(external.contains("if actual_mode & 0o777 != expected_mode"));

    let read_start = external.find("pub(super) fn read_hwm(&self)").expect("read_hwm");
    let read_tail = &external[read_start..];
    let read_end = read_tail.find("    /// Durably advances").expect("read_hwm end");
    let read_hwm = &read_tail[..read_end];
    assert_eq!(read_hwm.matches("self.verify_live_leaf()?;").count(), 2);
    assert!(
        read_hwm.find("self.verify_live_leaf()?;").expect("pre-read validation")
            < read_hwm.find("read_row(&self.db)?").expect("redb row read")
    );
    assert!(
        read_hwm.rfind("self.verify_live_leaf()?;").expect("post-read validation")
            > read_hwm.find("read_row(&self.db)?").expect("redb row read")
    );

    let observe_start = external.find("pub(super) fn observe(&self").expect("observe");
    let observe_tail = &external[observe_start..];
    let observe_end = observe_tail.find("\n    fn verify_live_leaf").expect("observe end");
    let observe = &observe_tail[..observe_end];
    assert_eq!(observe.matches("self.verify_live_leaf()?;").count(), 2);
    assert!(
        observe.find("self.verify_live_leaf()?;").expect("pre-observe validation")
            < observe.find("self.db.begin_write()").expect("write transaction")
    );
    assert!(
        observe.rfind("self.verify_live_leaf()?;").expect("post-observe validation")
            > observe.find("write.commit()").expect("Immediate commit")
    );

    let verifier_start = external.find("fn verify_live_leaf(&self)").expect("live leaf verifier");
    let verifier_tail = &external[verifier_start..];
    let verifier_end = verifier_tail.find("\n}\n\n/// Narrow").expect("live leaf verifier end");
    let verifier = &verifier_tail[..verifier_end];
    assert!(verifier.contains("std::fs::symlink_metadata(&self.db_path)"));
    assert!(verifier.contains("metadata.len() == 0"));
    assert!(verifier.contains("current != self.leaf_identity"));
    assert!(verifier.contains("validate_unix_owner_mode("));
    for forbidden in [
        "open_database_file",
        "redb::Database::open",
        "OpenOptions",
        "create",
        "repair",
        "adopt",
        "reseed",
    ] {
        for (name, body) in
            [("read_hwm", read_hwm), ("observe", observe), ("verify_live_leaf", verifier)]
        {
            assert!(!body.contains(forbidden), "{name} contains bypass token {forbidden}");
        }
    }
}

fn anchor_negative_fixture_is_rejected(source: &str) -> bool {
    let found = identifiers(source).expect("fixture source");
    FORBIDDEN_IDENTIFIERS.iter().any(|forbidden| found.contains(*forbidden))
        || source.contains("pub fn new(")
        || source.contains("redb::Database::create(path)")
        || source.contains("pub use killstate_anchor::AnchorProvisioner;")
}

#[test]
fn p0a_killstate_anchor_negative_fixtures_fail_the_seal() {
    for fixture in [
        "fn bypass() { send_raw_transaction(); }",
        "impl AnchoredKillStateStore { pub fn new(path: PathBuf) {} }",
        "fn open() { redb::Database::create(path); }",
        "pub use killstate_anchor::AnchorProvisioner;",
    ] {
        assert!(anchor_negative_fixture_is_rejected(fixture), "fixture was not rejected");
    }
    assert!(!anchor_negative_fixture_is_rejected(
        "pub fn open_anchored_killstate() -> Result<AnchoredKillStateStore, StartupError> {}"
    ));
}
#[test]
fn runtime_wiring_is_receive_only_and_preserves_forwarding_semantics() {
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
    for forbidden in ["WebSocketStream", "Message", "Sender", "raw_tx", "send_raw_transaction"] {
        assert!(
            !adapter_identifiers.contains(forbidden),
            "forbidden transport or egress surface {forbidden} in CLI trader adapter"
        );
    }

    assert_eq!(adapter.matches("tokio::spawn").count(), 5);
    assert_eq!(adapter.matches("spawn_critical_task").count(), 0);
    assert_eq!(adapter.matches("spawn_with_graceful_shutdown_signal").count(), 1);
    assert_eq!(adapter.matches("subscribe_to_flashblocks").count(), 1);
    assert_eq!(adapter.matches("MEV_TRADER_BLINK_CREDENTIAL_FILE").count(), 1);
    let phase_gate = adapter.find("if !Self::enabled(env)").expect("exact phase gate");
    let flashblocks_gate =
        adapter.find("flashblocks_config.as_ref()?").expect("flashblocks-present gate");
    let credential_gate = adapter
        .find("std::env::var_os(\"MEV_TRADER_BLINK_CREDENTIAL_FILE\")")
        .expect("credential consult");
    assert!(phase_gate < flashblocks_gate);
    assert!(flashblocks_gate < credential_gate);
    assert!(runtime.contains("LatestSlot<QueuedBlinkVictim>"));
    assert!(runtime.contains("FixturePoolRegistry::new(descriptors, digest)"));
    assert_eq!(runtime.matches("run_consumer").count(), 1);
    assert_eq!(runtime.matches("run_control").count(), 1);

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
