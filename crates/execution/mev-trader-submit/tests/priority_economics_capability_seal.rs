#![cfg(feature = "tx-authority")]
#![doc = "Immutable source contracts for the T4b priority-economics capability detector."]

use std::{collections::BTreeSet, fs, path::PathBuf};

use syn::{Expr, ImplItem, Item, Lit, Meta, Type, UseTree, Visibility};

const VIEW_METHODS: [&str; 26] = [
    "access_digest",
    "beryl_env",
    "deployment_witness",
    "executor",
    "frame",
    "frame_digest",
    "freshness_witness",
    "header_coinbase",
    "header_identity_digest",
    "kickback_recipient",
    "nonce_witness",
    "order_digest",
    "overlay_digest",
    "parent_hash",
    "parent_header",
    "plan_digest",
    "resolved_adapters",
    "route_digest",
    "route_hops",
    "route_pools",
    "route_protocols",
    "route_tokens",
    "sender",
    "shape_digest",
    "state_digest",
    "unsigned_signing_hash",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn read(relative: &str) -> String {
    let path = root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn public_methods(source: &str, type_name: &str) -> BTreeSet<String> {
    let file = syn::parse_file(source).expect("production Rust parses");
    file.items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(item) => {
                let Type::Path(ty) = item.self_ty.as_ref() else {
                    return None;
                };
                (item.trait_.is_none()
                    && ty.path.segments.last().is_some_and(|segment| segment.ident == type_name))
                .then_some(item)
            }
            _ => None,
        })
        .flat_map(|item| &item.items)
        .filter_map(|item| match item {
            ImplItem::Fn(method) if matches!(method.vis, Visibility::Public(_)) => {
                Some(method.sig.ident.to_string())
            }
            _ => None,
        })
        .collect()
}

fn quoted_values(body: &str) -> BTreeSet<String> {
    body.split('"').skip(1).step_by(2).map(str::to_owned).collect()
}

fn is_t4b_cfg(attribute: &syn::Attribute) -> bool {
    let Meta::List(cfg) = &attribute.meta else {
        return false;
    };
    if !cfg.path.is_ident("cfg") {
        return false;
    }
    matches!(
        cfg.parse_args::<Meta>(),
        Ok(Meta::NameValue(value))
            if value.path.is_ident("feature")
                && matches!(
                    &value.value,
                    Expr::Lit(literal)
                        if matches!(&literal.lit, Lit::Str(feature) if feature.value() == "t4b-shadow")
                )
    )
}

fn cli_t4b_exports(source: &str, expected: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let file = syn::parse_file(source).map_err(|error| error.to_string())?;
    let mut candidates = file.items.iter().filter_map(|item| {
        let Item::Use(item) = item else {
            return None;
        };
        if !matches!(item.vis, Visibility::Public(_)) || !item.attrs.iter().any(is_t4b_cfg) {
            return None;
        }
        match &item.tree {
            UseTree::Path(root) if root.ident == "mev_trader" => Some(root.tree.as_ref()),
            _ => None,
        }
    });
    let tree = candidates.next().ok_or_else(|| "missing grouped T4b export".to_owned())?;
    if candidates.next().is_some() {
        return Err("ambiguous grouped T4b export".to_owned());
    }
    let UseTree::Group(group) = tree else {
        return Err("T4b export must be one direct group".to_owned());
    };
    let mut names = BTreeSet::new();
    for leaf in &group.items {
        let name = match leaf {
            UseTree::Name(name) => name.ident.to_string(),
            UseTree::Rename(rename) => {
                return Err(format!(
                    "T4b export alias is forbidden: {} as {}",
                    rename.ident, rename.rename
                ));
            }
            UseTree::Glob(_) => return Err("glob export is forbidden".to_owned()),
            UseTree::Group(_) | UseTree::Path(_) => {
                return Err("nested T4b export is ambiguous".to_owned());
            }
        };
        if !names.insert(name.clone()) {
            return Err(format!("duplicate T4b export {name}"));
        }
    }
    if &names != expected {
        return Err(format!("T4b exports differ: {names:?}"));
    }
    Ok(names)
}

#[test]
fn clean_feature_graph_excludes_signer_arm_and_egress_capabilities() {
    let submit = read("crates/execution/mev-trader-submit/Cargo.toml");
    let cli = read("crates/execution/cli/Cargo.toml");
    let submit_t4b = submit
        .split_once("tx-authority = [")
        .expect("tx-authority feature")
        .1
        .split_once(']')
        .expect("closed feature")
        .0;
    let cli_t4b = cli
        .split_once("t4b-shadow = [")
        .expect("t4b-shadow feature")
        .1
        .split_once(']')
        .expect("closed feature")
        .0;
    for forbidden in ["phase-b", "arm", "egress", "k256", "rand_08", "reqwest", "transport"] {
        assert!(!submit_t4b.contains(forbidden), "submit T4b gained {forbidden}");
        assert!(!cli_t4b.contains(forbidden), "CLI T4b gained {forbidden}");
    }
}

#[test]
fn checked_bindings_view_and_nested_witness_api_are_exact() {
    let authority = read("crates/execution/mev-trader-submit/src/tx_authority.rs");
    assert_eq!(
        public_methods(&authority, "CheckedBindingsView"),
        VIEW_METHODS.into_iter().map(str::to_owned).collect()
    );
    for (name, expected) in [
        (
            "CheckedBerylEnvInputs",
            &[
                "base_fee_per_gas",
                "block_number",
                "chain_id",
                "excess_blob_gas",
                "gas_limit",
                "prev_randao",
                "timestamp",
            ][..],
        ),
        ("DeploymentWitness", &["executor", "route_adapters", "validated_parent"]),
        (
            "NonceWitness",
            &["committed_nonce", "parent_hash", "pending_overlay_nonce", "sender", "shape_nonce"],
        ),
        (
            "FreshnessWitness",
            &[
                "parent_hash",
                "snapshot_identity_digest",
                "snapshot_parent_hash",
                "valid_until_block",
            ],
        ),
    ] {
        assert_eq!(
            public_methods(&authority, name),
            expected.iter().copied().map(str::to_owned).collect(),
            "nested projection changed: {name}"
        );
    }
}

#[test]
fn submit_cli_and_execution_ast_positives_are_present_once() {
    let authority = read("crates/execution/mev-trader-submit/src/tx_authority.rs");
    let cli = read("crates/execution/cli/src/mev_trader.rs");
    let cli_lib = read("crates/execution/cli/src/lib.rs");
    for required in [
        "pub trait CandidateExecutionAdapter: Sized",
        "pub fn execute_once<A>",
        "request.into_parts()",
        "parts.into_tx_and_bindings()",
        "prepare_pre_economics(view)",
    ] {
        assert!(authority.contains(required) || cli.contains(required), "missing {required}");
    }
    assert_eq!(cli.matches("evm.transact(").count(), 1);
    for method in [
        "fn basic(",
        "fn code_by_hash(",
        "fn storage(",
        "fn storage_by_account_id(",
        "fn block_hash(",
        "fn commit(",
    ] {
        assert!(cli.contains(method), "AuditedDatabase lost {method}");
    }
    assert!(cli.contains("AuditPhase::Candidate"));
    assert!(cli.contains("CandidateBlockHashForbidden"));
    let expected: BTreeSet<_> = [
        "AuditPhase",
        "AuditedAccessKindV1",
        "AuditedAccessV1",
        "AuditedDatabase",
        "AuditedDatabaseError",
        "CandidateAccessAllowlistV1",
        "CandidateAccessedStateV1",
        "CandidateExecutionCardinalityV1",
        "CandidateStateCollectionError",
        "T4bCaptureDispositionV1",
        "T4bOverlayError",
        "T4bParentOverlayAdapter",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(cli_t4b_exports(&cli_lib, &expected), Ok(expected.clone()));

    let grouped_start = cli_lib
        .find("#[cfg(feature = \"t4b-shadow\")]\npub use mev_trader::{")
        .expect("grouped T4b export start");
    let grouped_body =
        grouped_start + "#[cfg(feature = \"t4b-shadow\")]\npub use mev_trader::{".len();
    let grouped_end =
        grouped_start + cli_lib[grouped_start..].find("};").expect("grouped T4b export boundary");

    let lowercase = cli_lib.replacen("    AuditPhase,", "    AuditPhase as audit_phase,", 1);
    let alias = cli_lib.replacen("    AuditPhase,", "    AuditPhase as EscapedAlias,", 1);
    let approved_name_alias =
        cli_lib.replacen("    AuditPhase,", "    AuditedAccessKindV1 as AuditPhase,", 1);
    let mut glob = cli_lib.clone();
    glob.replace_range(grouped_body..grouped_end, "\n    *\n");
    for (name, mutant) in [
        ("lowercase leaf", lowercase),
        ("glob leaf", glob),
        ("unapproved alias", alias),
        ("approved-name alias", approved_name_alias),
    ] {
        syn::parse_file(&mutant).unwrap_or_else(|error| panic!("{name} mutation parses: {error}"));
        assert!(cli_t4b_exports(&mutant, &expected).is_err(), "{name} escaped CLI seal");
    }
}

#[test]
fn normal_t4b_surface_has_no_raw_serde_signer_or_egress_escape() {
    let authority = read("crates/execution/mev-trader-submit/src/tx_authority.rs");
    let submit_lib = read("crates/execution/mev-trader-submit/src/lib.rs");
    let cli = read("crates/execution/cli/src/mev_trader.rs");
    assert!(!authority.contains("pub fn raw_tx("));
    assert!(!authority.contains("pub const fn raw_tx("));
    assert!(!authority.contains("serde::Serialize"));
    assert!(!submit_lib.contains("RawOwner"));
    let t4b = cli.split_once("mod t4b_shadow {").expect("T4b module").1;
    let t4b = t4b.split_once("mod t4d_shadow {").expect("following module").0;
    for forbidden in [
        "send_gated",
        "RawEgress",
        "RawBackend",
        "ProdBackend",
        "SigningKey",
        "HotWalletKey",
        "reqwest::",
        "T4bMutantEgressProbe",
    ] {
        assert!(!t4b.contains(forbidden), "normal T4b contains {forbidden}");
    }
}

#[test]
fn public_raw_accessor_mutant_is_compile_valid_and_detector_visible() {
    let runner = read("scripts/t4b-capability-mutants.py");
    assert!(runner.contains("pub const fn raw_tx(&self) -> &TxEip1559 { &self.unsigned_tx }"));
    assert!(runner.contains("\"public-raw-accessor\": \"raw-public-accessor\""));
    assert!(runner.contains("--features\", \"tx-authority\", \"--offline"));
}

#[test]
fn raw_owner_serde_mutant_is_compile_valid_and_detector_visible() {
    let runner = read("scripts/t4b-capability-mutants.py");
    assert!(runner.contains("#[derive(Debug, serde::Serialize)]"));
    assert!(runner.contains("serde = { workspace = true, features = [\\\"derive\\\"] }"));
    assert!(runner.contains("\"raw-owner-serde\": \"raw-owner-serde\""));
}

#[test]
fn raw_owner_root_reexport_mutant_is_compile_valid_and_detector_visible() {
    let runner = read("scripts/t4b-capability-mutants.py");
    assert!(runner.contains("ProtocolAdapterMapping, RawOwner, SnapshotFreshnessToken"));
    assert!(runner.contains("\"raw-owner-root-reexport\": \"raw-owner-root-reexport\""));
}

#[test]
fn tracked_egress_mutant_has_one_patch_derived_ten_file_authority() {
    let runner = read("scripts/t4b-capability-mutants.py");
    let manifest = read("crates/execution/cli/testdata/t4b-mutant4/patches/MANIFEST");
    fn parse_manifest(source: &str) -> Option<Vec<(&str, &str, &str)>> {
        if source.as_bytes().len() != 538
            || !source.ends_with('\n')
            || source.contains('\r')
            || source.lines().count() != 6
        {
            return None;
        }
        source
            .strip_suffix('\n')?
            .split('\n')
            .map(|row| {
                let mut fields = row.split('\0');
                let parsed = (fields.next()?, fields.next()?, fields.next()?);
                fields.next().is_none().then_some(parsed)
            })
            .collect()
    }

    let expected_rows = vec![
        (
            "01-cargo-dependency.patch",
            "9d2ea12da4c67dc98f3cf7819b218f4e64b447bd5090b06c00ce80facec03db3",
            "517",
        ),
        (
            "02-cargo-feature.patch",
            "6bd9b3d080bef3a44e0774c648c35664baeedf2d46103380ec8ed8c176159950",
            "473",
        ),
        (
            "03-import.patch",
            "af8a31532456a8a35914ff66bbdaffe5cd40890ea2b912525705039afadb2fdb",
            "355",
        ),
        (
            "04-probe.patch",
            "83d2f30de960a36b2a1f6953046615921ce078f265fd8c5846d5287791f1e0f2",
            "608",
        ),
        (
            "05-observer-call.patch",
            "3b1af1cd6a579648755e6d2ba1fd725207eaec12d8901ac4e457a11e6fee763f",
            "499",
        ),
        (
            "06-root-export.patch",
            "438244cf22f50bc6246081e41771e723c5b9f3229d498e0665c6123a4fea713d",
            "606",
        ),
    ];
    assert_eq!(parse_manifest(&manifest), Some(expected_rows.clone()));
    let hash_offset = manifest.find(expected_rows[0].1).expect("first pinned SHA");
    let mut same_length_hex_mutant = manifest.clone().into_bytes();
    same_length_hex_mutant[hash_offset] =
        if same_length_hex_mutant[hash_offset] == b'a' { b'b' } else { b'a' };
    let same_length_hex_mutant =
        String::from_utf8(same_length_hex_mutant).expect("ASCII manifest mutation");
    assert_eq!(same_length_hex_mutant.len(), manifest.len());
    assert_ne!(parse_manifest(&same_length_hex_mutant), Some(expected_rows.clone()));

    let patch_names = expected_rows.iter().map(|row| row.0);
    for name in patch_names {
        let patch = read(&format!("crates/execution/cli/testdata/t4b-mutant4/patches/{name}"));
        let additions = patch
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .map(|line| format!("{}\n", &line[1..]))
            .collect::<String>();
        assert!(!additions.is_empty(), "{name} has no additions");
        assert!(
            !runner.contains(additions.as_str()),
            "{name} additions were duplicated by hand in the runner"
        );
    }

    let authority_paths = [
        "crates/execution/cli/testdata/t4b-mutant4/patches/MANIFEST",
        "crates/execution/cli/testdata/t4b-mutant4/patches/01-cargo-dependency.patch",
        "crates/execution/cli/testdata/t4b-mutant4/patches/02-cargo-feature.patch",
        "crates/execution/cli/testdata/t4b-mutant4/patches/03-import.patch",
        "crates/execution/cli/testdata/t4b-mutant4/patches/04-probe.patch",
        "crates/execution/cli/testdata/t4b-mutant4/patches/05-observer-call.patch",
        "crates/execution/cli/testdata/t4b-mutant4/patches/06-root-export.patch",
        "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/Cargo.toml",
        "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/mev_trader.rs",
        "crates/execution/cli/testdata/t4b-mutant4/fixture/crates/execution/cli/src/lib.rs",
    ];
    for path in authority_paths {
        let quoted = format!("\"{path}\"");
        assert!(runner.contains(quoted.as_str()), "unsealed authority: {path}");
    }
    assert!(runner.contains("len(AUTHORITY_FILES) != 10"));
    assert!(runner.contains("for relative in AUTHORITY_FILES:"));
    assert!(runner.contains("\"ls-files\", \"--error-unmatch\""));
    assert!(runner.contains("\"diff\", \"--cached\", \"--quiet\""));
    assert!(runner.contains("parsed.append(parse_patch(name, data))"));
    assert!(runner.contains("apply_fixture_hunk(replay[hunk.path], hunk)"));
    assert_eq!(runner.matches("insert_patch_additions(root, hunk)").count(), 1);
    assert!(!runner.contains("git\", \"apply"));
    let expected = quoted_values(
        runner.split_once("TEN = [").expect("ten deltas").1.split_once(']').expect("closed").0,
    );
    assert_eq!(expected.len(), 10);
}

#[test]
fn phase_b_signer_edge_mutant_is_compile_valid_and_detector_visible() {
    let runner = read("scripts/t4b-capability-mutants.py");
    assert!(runner.contains("alloy-consensus/k256"));
    assert!(runner.contains("dep:k256"));
    assert!(runner.contains("dep:rand_08"));
    assert!(runner.contains(r#"cfg(any(feature = "phase-b", feature = "tx-authority"))"#));
    assert!(runner.contains("\"phase-b-signer-edge\": \"phase-b-signer-edge\""));
}
