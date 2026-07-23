#![cfg(feature = "tx-authority")]
#![doc = "Offline AST, source, and feature-graph seal for the T4b unsigned authority tier."]

use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

use syn::{
    ExprCall, ExprMethodCall, ExprStruct, File, Ident, Item, ItemFn, ItemImpl, Macro, Path,
    Visibility, visit::Visit,
};

fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn feature_body<'a>(manifest: &'a str, name: &str) -> &'a str {
    let marker = format!("{name} = [");
    let rest = manifest.split_once(&marker).unwrap_or_else(|| panic!("missing {name}")).1;
    rest.split_once(']').expect("closed feature list").0
}

fn cargo_tree(root: &PathBuf, features: Option<&str>) -> String {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "-p",
        "base-reth-node",
        "-e",
        "normal,build,features",
        "--prefix",
        "none",
        "--offline",
    ]);
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    command_output(command, root)
}

fn submit_feature_provenance(root: &PathBuf) -> String {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "-p",
        "base-reth-node",
        "--features",
        "t4b-shadow",
        "-i",
        "mev-trader-submit",
        "-e",
        "normal,build,features",
        "--prefix",
        "none",
        "--offline",
    ]);
    command_output(command, root)
}

fn package_closure(root: &PathBuf, package: &str, features: Option<&str>) -> String {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "-p",
        package,
        "--no-default-features",
        "-e",
        "normal,build,features",
        "--prefix",
        "none",
        "--offline",
    ]);
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    command_output(command, root)
}

fn command_output(mut command: Command, root: &PathBuf) -> String {
    let output = command.current_dir(root).output().expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("cargo tree UTF-8")
}

#[derive(Default)]
struct AstSeal {
    violations: BTreeSet<String>,
    observer_impls: usize,
    node_view_impls: usize,
    pending_view_impls: usize,
    base_mainnet_calls: usize,
    observe_candidate_calls: usize,
    spawn_calls: usize,
    subscription_calls: usize,
    pending_adapter_constructions: usize,
    node_view_constructions: usize,
    authority_constructions: usize,
    observer_factories: usize,
    observer_install_calls: usize,
}

impl AstSeal {
    fn inspect_path(&mut self, path: &Path) {
        let segments =
            path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        let joined = segments.join("::");
        for forbidden in ["reqwest", "txpool", "signer", "RawEgress", "ProdBackend", "OpenOptions"]
        {
            if segments.iter().any(|segment| segment == forbidden) {
                self.violations.insert(forbidden.to_owned());
            }
        }
        if joined.starts_with("std::net") || joined.starts_with("std::fs") {
            self.violations.insert(joined);
        }
    }

    fn count_call(&mut self, name: &str) {
        match name {
            "base_mainnet" => self.base_mainnet_calls += 1,
            "observe_candidate" | "try_observe" => self.observe_candidate_calls += 1,
            "spawn" | "spawn_blocking" => self.spawn_calls += 1,
            "subscribe_to_flashblocks" => self.subscription_calls += 1,
            "with_t4b_observer" | "start_with_t4b_observer" => {
                self.observer_install_calls += 1;
            }
            _ => {}
        }
    }
}

impl<'ast> Visit<'ast> for AstSeal {
    fn visit_ident(&mut self, ident: &'ast Ident) {
        let name = ident.to_string();
        for forbidden in [
            "Signature",
            "RawEgress",
            "ProdBackend",
            "OpenOptions",
            "load_and_sign",
            "send_gated",
            "into_signed",
            "txpool",
            "signer",
            "reqwest",
        ] {
            if name == forbidden {
                self.violations.insert(forbidden.to_owned());
            }
        }
    }

    fn visit_path(&mut self, path: &'ast Path) {
        self.inspect_path(path);
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref()
            && let Some(segment) = path.path.segments.last()
        {
            self.count_call(&segment.ident.to_string());
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        self.count_call(&call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if let Some((_, path, _)) = &item.trait_ {
            match path.segments.last().map(|segment| segment.ident.to_string()).as_deref() {
                Some("CandidateTxShapeObserver") => self.observer_impls += 1,
                Some("TxAuthorityNodeView") => self.node_view_impls += 1,
                Some("PendingSnapshotView") => self.pending_view_impls += 1,
                _ => {}
            }
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_expr_struct(&mut self, item: &'ast ExprStruct) {
        match item.path.segments.last().map(|segment| segment.ident.to_string()).as_deref() {
            Some("PendingSnapshotViewAdapter") => self.pending_adapter_constructions += 1,
            Some("T4bNodeView") => self.node_view_constructions += 1,
            Some("T4bShadowAuthority") => self.authority_constructions += 1,
            _ => {}
        }
        syn::visit::visit_expr_struct(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if item.sig.ident == "observer" {
            self.observer_factories += 1;
        }
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_macro(&mut self, item: &'ast Macro) {
        let tokens = item.tokens.to_string();
        for forbidden in [
            "Signature",
            "load_and_sign",
            "send_gated",
            "into_signed",
            "reqwest",
            "txpool",
            "std :: net",
            "std :: fs",
        ] {
            if tokens.contains(forbidden) {
                self.violations.insert(format!("macro:{forbidden}"));
            }
        }
        syn::visit::visit_macro(self, item);
    }
}

fn parse_production(source: &str) -> File {
    let production = source.split("#[cfg(test)]").next().expect("production source");
    syn::parse_file(production).expect("production source parses as Rust AST")
}

fn tx_authority_modules(source: &str) -> BTreeSet<String> {
    let mut selected = BTreeSet::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "#[cfg(feature = \"tx-authority\")]" {
            continue;
        }
        let declaration = lines.find(|next| !next.trim().is_empty()).expect("module after cfg");
        if let Some(name) = declaration
            .trim()
            .strip_prefix("mod ")
            .or_else(|| declaration.trim().strip_prefix("pub mod "))
            .and_then(|rest| rest.strip_suffix(';'))
        {
            selected.insert(name.to_owned());
        }
    }
    selected
}

fn assert_private_fields(file: &File, names: &[&str]) {
    for name in names {
        let item = file
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == *name => Some(item),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing struct {name}"));
        assert!(
            item.fields.iter().all(|field| matches!(field.vis, Visibility::Inherited)),
            "{name} exposes authority fields"
        );
    }
}

fn package_set(tree: &str) -> BTreeSet<String> {
    tree.lines().filter_map(|line| line.split_once(" v").map(|(name, _)| name.to_owned())).collect()
}

fn feature_set(tree: &str) -> BTreeSet<String> {
    tree.lines().filter(|line| line.contains(" feature \"")).map(str::to_owned).collect()
}

#[test]
fn t4b_default_and_selected_feature_closures_preserve_zero_broadcast_capability() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../../..");
    let submit_manifest = read(crate_dir.join("Cargo.toml"));
    let submit_lib = read(crate_dir.join("src/lib.rs"));
    let assembler_source = read(crate_dir.join("src/assembler.rs"));
    let calldata_source = read(crate_dir.join("src/calldata.rs"));
    let fee_source = read(crate_dir.join("src/fee.rs"));
    let authority_source = read(crate_dir.join("src/tx_authority.rs"));
    let cli_manifest = read(root.join("crates/execution/cli/Cargo.toml"));
    let cli_source = read(root.join("crates/execution/cli/src/mev_trader.rs"));
    let trader_manifest = read(root.join("crates/execution/mev-trader/Cargo.toml"));
    let trader_runtime = read(root.join("crates/execution/mev-trader/src/runtime.rs"));
    let trader_port = read(root.join("crates/execution/mev-trader/src/port.rs"));
    let node_manifest = read(root.join("bin/node/Cargo.toml"));

    for source in [&submit_lib, &assembler_source, &calldata_source, &fee_source, &authority_source]
    {
        syn::parse_file(source).expect("submit source parses as Rust AST");
    }
    assert_eq!(
        tx_authority_modules(&submit_lib),
        BTreeSet::from(["calldata".to_owned(), "fee".to_owned(), "tx_authority".to_owned()])
    );
    let authority_ast = parse_production(&authority_source);
    assert_private_fields(
        &authority_ast,
        &["ValidatedAbiHop", "ValidatedAtomicCall", "ValidatedUnsignedAtomicTx"],
    );

    assert!(!submit_manifest.lines().any(|line| line.trim_start().starts_with("default =")));
    let submit_tier = feature_body(&submit_manifest, "tx-authority");
    for forbidden in ["phase-b", "arm", "k256", "rand", "reqwest", "zeroize"] {
        assert!(!submit_tier.contains(forbidden), "tx-authority enables {forbidden}");
    }
    assert!(submit_tier.contains("base-mev-trader/t4b-shadow"));
    assert!(assembler_source.starts_with("//!"));
    assert!(assembler_source.contains("#![cfg(feature = \"phase-b\")]"));
    assert!(submit_lib.contains("#[cfg(feature = \"phase-b\")]\npub mod assembler;"));
    assert!(submit_lib.contains("#[cfg(feature = \"phase-b\")]\npub mod signer;"));
    assert!(submit_lib.contains("#[cfg(feature = \"arm\")]\nmod arm;"));

    let cli_tier = feature_body(&cli_manifest, "t4b-shadow");
    assert!(cli_tier.contains("mev-trader-submit/tx-authority"));
    for forbidden in ["phase-b", "arm", "reqwest", "signer"] {
        assert!(!cli_tier.contains(forbidden), "CLI T4b enables {forbidden}");
    }
    assert!(trader_manifest.contains("t4b-shadow = [\"t4a-shadow\"]"));
    assert!(node_manifest.contains("t4b-shadow = [ \"base-execution-cli/t4b-shadow\" ]"));
    assert!(cli_manifest.contains("mev-trader-submit = { workspace = true, optional = true }"));

    let mut selected_seal = AstSeal::default();
    for source in [&calldata_source, &fee_source, &authority_source] {
        selected_seal.visit_file(&parse_production(source));
    }
    assert!(
        selected_seal.violations.is_empty(),
        "forbidden selected AST: {:?}",
        selected_seal.violations
    );
    assert_eq!(selected_seal.spawn_calls, 0);
    assert_eq!(selected_seal.subscription_calls, 0);
    assert_eq!(calldata_source.matches("executeBlinkOfaAtomicCall").count(), 1);
    assert_eq!(assembler_source.matches("AtomicCalldataEncoder::encode_legacy").count(), 1);
    assert_eq!(
        authority_source
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .matches("AtomicCalldataEncoder::encode_validated")
            .count(),
        1
    );

    let default_tree = cargo_tree(&root, None);
    assert!(!default_tree.contains("mev-trader-submit v"));
    let selected_tree = cargo_tree(&root, Some("t4b-shadow"));
    assert!(selected_tree.contains("mev-trader-submit v"));
    let provenance = submit_feature_provenance(&root);
    let enabled_submit_features = provenance
        .lines()
        .filter_map(|line| {
            line.strip_prefix("mev-trader-submit feature \"")?.split_once('"').map(|(name, _)| name)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        enabled_submit_features,
        BTreeSet::from(["base-mev-trader", "default", "tx-authority"])
    );
    for forbidden in ["phase-b", "arm", "arm-live-egress", "arm-provisioning"] {
        assert!(!provenance.contains(&format!("mev-trader-submit feature \"{forbidden}\"")));
    }
    let trader_closure = package_closure(&root, "base-mev-trader", Some("t4b-shadow"));
    let submit_closure = package_closure(&root, "mev-trader-submit", Some("tx-authority"));
    let trader_baseline = package_set(&trader_closure);
    let submit_selected = package_set(&submit_closure);
    let selected_delta: BTreeSet<String> =
        submit_selected.difference(&trader_baseline).cloned().collect();
    assert_eq!(
        selected_delta,
        BTreeSet::from(["mev-trader-submit".to_owned()]),
        "T4b submit closure added an unreviewed package beyond the inherited trader baseline"
    );
    let feature_delta: BTreeSet<String> =
        feature_set(&submit_closure).difference(&feature_set(&trader_closure)).cloned().collect();
    assert_eq!(
        feature_delta,
        BTreeSet::from(["base-mev-trader feature \"default\"".to_owned()]),
        "T4b submit closure added an unreviewed dependency feature"
    );

    let cli_ast = parse_production(&cli_source);
    let mut cli_seal = AstSeal::default();
    cli_seal.visit_file(&cli_ast);
    assert_eq!(cli_seal.violations, BTreeSet::from(["OpenOptions".to_owned()]));
    assert_eq!(cli_seal.node_view_impls, 1);
    assert_eq!(cli_seal.observer_impls, 1);
    assert_eq!(cli_seal.pending_view_impls, 1);
    assert_eq!(cli_seal.base_mainnet_calls, 1);
    assert_eq!(cli_seal.observe_candidate_calls, 0);
    assert_eq!(cli_seal.subscription_calls, 1);
    assert_eq!(cli_seal.spawn_calls, 4);
    assert_eq!(cli_seal.pending_adapter_constructions, 1);
    assert_eq!(cli_seal.node_view_constructions, 1);
    assert_eq!(cli_seal.authority_constructions, 1);
    assert_eq!(cli_seal.observer_factories, 1);
    assert_eq!(cli_seal.observer_install_calls, 2);

    let t4b_module = cli_ast
        .items
        .iter()
        .find_map(|item| match item {
            Item::Mod(module) if module.ident == "t4b_shadow" => Some(module),
            _ => None,
        })
        .expect("inline T4b module");
    assert!(t4b_module.content.is_some(), "T4b module must stay inline and sealed");
    let mut t4b_cli_seal = AstSeal::default();
    t4b_cli_seal.visit_item_mod(t4b_module);
    assert!(t4b_cli_seal.violations.is_empty());
    assert_eq!(t4b_cli_seal.spawn_calls, 0);
    assert_eq!(t4b_cli_seal.subscription_calls, 0);
    assert_eq!(t4b_cli_seal.node_view_impls, 1);
    assert_eq!(t4b_cli_seal.observer_impls, 1);
    assert_eq!(t4b_cli_seal.base_mainnet_calls, 1);
    assert_eq!(t4b_cli_seal.node_view_constructions, 1);
    assert_eq!(t4b_cli_seal.authority_constructions, 1);
    assert_eq!(t4b_cli_seal.observer_factories, 1);

    let mut port_seal = AstSeal::default();
    port_seal.visit_file(&parse_production(&trader_port));
    assert!(port_seal.violations.is_empty());
    assert_eq!(port_seal.spawn_calls, 0);
    assert_eq!(port_seal.subscription_calls, 0);

    let mut runtime_seal = AstSeal::default();
    runtime_seal.visit_file(&parse_production(&trader_runtime));
    assert!(runtime_seal.violations.is_empty());
    assert_eq!(runtime_seal.observe_candidate_calls, 1);
    assert_eq!(runtime_seal.spawn_calls, 0);
    assert_eq!(runtime_seal.subscription_calls, 0);
    assert!(!trader_runtime.contains("mev_trader_submit"));
}
