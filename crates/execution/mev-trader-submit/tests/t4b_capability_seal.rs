#![cfg(feature = "tx-authority")]
#![doc = "Offline AST, source, and feature-graph seal for the T4b unsigned authority tier."]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path as FsPath, PathBuf},
    process::Command,
};

use base_mev_trader::ExactProtocol;
use mev_trader_submit::ProtocolAdapterMapping;
use syn::{
    Attribute, Expr, ExprCall, ExprField, ExprMethodCall, ExprPath, ExprStruct, File, Ident,
    ImplItem, ImplItemFn, Item, ItemFn, ItemImpl, ItemStruct, Lit, Macro, Member, Meta, Pat, Path,
    Stmt, Token, Type, TypePath, UseTree, Visibility, parse::Parser, punctuated::Punctuated,
    visit::Visit,
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

fn submit_feature_provenance(root: &PathBuf, selected_feature: &str) -> String {
    let mut command = Command::new(env!("CARGO"));
    command.args([
        "tree",
        "-p",
        "base-reth-node",
        "--features",
        selected_feature,
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
    assemble_sealed_calls: usize,
    encode_validated_calls: usize,
    observe_candidate_calls: usize,
    spawn_calls: usize,
    thread_calls: usize,
    item_macros: usize,
    production_modules: usize,
    subscription_calls: usize,
    slot_constructions: usize,
    pending_adapter_constructions: usize,
    node_view_constructions: usize,
    authority_constructions: usize,
    observer_factories: usize,
    observer_install_calls: usize,
    t4d_observer_calls: usize,
    strict_unsigned_handoff: bool,
    allow_victim_envelope: bool,
}

impl AstSeal {
    fn unsigned_handoff() -> Self {
        Self { strict_unsigned_handoff: true, ..Self::default() }
    }

    fn unsigned_authority() -> Self {
        Self { strict_unsigned_handoff: true, allow_victim_envelope: true, ..Self::default() }
    }

    fn inspect_path(&mut self, path: &Path) {
        let segments =
            path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        let joined = segments.join("::");
        for forbidden in [
            "reqwest",
            "txpool",
            "signer",
            "RawEgress",
            "RawBackend",
            "ProdBackend",
            "OpenOptions",
            "AuthorizedCandidate",
        ] {
            if segments.iter().any(|segment| segment == forbidden) {
                self.violations.insert(forbidden.to_owned());
            }
        }
        if self.strict_unsigned_handoff {
            for forbidden in [
                "PrimitiveSignature",
                "Signed",
                "SigningKey",
                "TxEnvelope",
                "AuthorizedSignedSubmission",
                "HotWalletKey",
                "sign_unsigned",
                "raw_signed",
                "raw_tx",
            ] {
                if forbidden == "TxEnvelope" && self.allow_victim_envelope {
                    continue;
                }
                if segments.iter().any(|segment| segment == forbidden) {
                    self.violations.insert(forbidden.to_owned());
                }
            }
        }
        if joined.starts_with("std::net") || joined.starts_with("std::fs") {
            self.violations.insert(joined);
        }
    }

    fn count_call(&mut self, name: &str) {
        match name {
            "base_mainnet" => self.base_mainnet_calls += 1,
            "assemble_sealed" => self.assemble_sealed_calls += 1,
            "encode_validated" => self.encode_validated_calls += 1,
            "observe_candidate" | "try_observe" => self.observe_candidate_calls += 1,
            "send" => {
                self.violations.insert("send".to_owned());
            }
            "spawn" | "spawn_blocking" => self.spawn_calls += 1,
            "subscribe_to_flashblocks" => self.subscription_calls += 1,
            "with_t4b_observer" | "start_with_t4b_observer" | "start_with_t4d_observer" => {
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
            "RawBackend",
            "ProdBackend",
            "OpenOptions",
            "AuthorizedCandidate",
            "load_and_sign",
            "send_gated",
            "into_signed",
            "sign",
            "send",
            "backend",
            "txpool",
            "signer",
            "reqwest",
        ] {
            if name == forbidden {
                self.violations.insert(forbidden.to_owned());
            }
        }
        if self.strict_unsigned_handoff
            && matches!(
                name.as_str(),
                "PrimitiveSignature"
                    | "Signed"
                    | "SigningKey"
                    | "TxEnvelope"
                    | "AuthorizedSignedSubmission"
                    | "HotWalletKey"
                    | "sign_unsigned"
                    | "raw_signed"
                    | "raw_tx"
            )
            && !(name == "TxEnvelope" && self.allow_victim_envelope)
        {
            self.violations.insert(name);
        }
    }

    fn visit_path(&mut self, path: &'ast Path) {
        self.inspect_path(path);
        syn::visit::visit_path(self, path);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            let segments = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            if let Some(name) = segments.last() {
                self.count_call(name);
            }
            if segments.ends_with(&["ShadowLatestSlot".to_owned(), "new".to_owned()]) {
                self.slot_constructions += 1;
            }
            if segments.ends_with(&["t4d_shadow".to_owned(), "observer".to_owned()]) {
                self.t4d_observer_calls += 1;
            }
            if segments.ends_with(&["thread".to_owned(), "spawn".to_owned()]) {
                self.thread_calls += 1;
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        if !has_cfg_test(&item.attrs) {
            self.item_macros += 1;
        }
        syn::visit::visit_item_macro(self, item);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        self.count_call(&call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_field(&mut self, field: &'ast ExprField) {
        if matches!(&field.member, Member::Named(name) if name == "send") {
            self.visit_expr(&field.base);
            return;
        }
        syn::visit::visit_expr_field(self, field);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if has_cfg_test(&item.attrs) {
            return;
        }
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
        if has_cfg_test(&item.attrs) {
            return;
        }
        if item.sig.ident == "observer" {
            self.observer_factories += 1;
        }
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_impl_item_fn(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            self.production_modules += 1;
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_macro(&mut self, item: &'ast Macro) {
        if item.path.segments.last().is_some_and(|segment| segment.ident == "include") {
            self.violations.insert("include macro redirect".to_owned());
        }
        let tokens = item.tokens.to_string();
        for forbidden in [
            "Signature",
            "AuthorizedCandidate",
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
        if self.strict_unsigned_handoff {
            for forbidden in [
                "PrimitiveSignature",
                "Signed",
                "SigningKey",
                "TxEnvelope",
                "AuthorizedSignedSubmission",
                "HotWalletKey",
                "sign_unsigned",
                "raw_signed",
                "raw_tx",
            ] {
                if forbidden == "TxEnvelope" && self.allow_victim_envelope {
                    continue;
                }
                if tokens.contains(forbidden) {
                    self.violations.insert(format!("macro:{forbidden}"));
                }
            }
        }
        syn::visit::visit_macro(self, item);
    }
}

#[derive(Default)]
struct VictimEnvelopeSeal {
    imports: usize,
    decode_calls: usize,
    eip1559_patterns: usize,
    violations: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for VictimEnvelopeSeal {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        for leaf in use_leaves(item) {
            if leaf.source.last().map(String::as_str) == Some("TxEnvelope") {
                self.imports += 1;
                if leaf.renamed || leaf.glob {
                    self.violations.insert("aliased victim envelope import".to_owned());
                }
            }
        }
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(path) = call.func.as_ref() {
            let segments = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            if segments.ends_with(&["TxEnvelope".to_owned(), "decode_2718".to_owned()]) {
                self.decode_calls += 1;
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_pat_tuple_struct(&mut self, pattern: &'ast syn::PatTupleStruct) {
        let segments = pattern
            .path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if segments.ends_with(&["TxEnvelope".to_owned(), "Eip1559".to_owned()]) {
            self.eip1559_patterns += 1;
        }
        syn::visit::visit_pat_tuple_struct(self, pattern);
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_fn(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }
}

fn parse_production(source: &str) -> File {
    syn::parse_file(source).expect("complete source parses as Rust AST")
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
#[derive(Debug)]
struct UseLeaf {
    source: Vec<String>,
    public_name: String,
    renamed: bool,
    glob: bool,
}

fn flatten_use_tree(tree: &UseTree, prefix: &mut Vec<String>, leaves: &mut Vec<UseLeaf>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            flatten_use_tree(&path.tree, prefix, leaves);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let mut source = prefix.clone();
            source.push(name.ident.to_string());
            leaves.push(UseLeaf {
                source,
                public_name: name.ident.to_string(),
                renamed: false,
                glob: false,
            });
        }
        UseTree::Rename(rename) => {
            let mut source = prefix.clone();
            source.push(rename.ident.to_string());
            leaves.push(UseLeaf {
                source,
                public_name: rename.rename.to_string(),
                renamed: true,
                glob: false,
            });
        }
        UseTree::Glob(_) => leaves.push(UseLeaf {
            source: prefix.clone(),
            public_name: "*".to_owned(),
            renamed: false,
            glob: true,
        }),
        UseTree::Group(group) => {
            for item in &group.items {
                flatten_use_tree(item, prefix, leaves);
            }
        }
    }
}

fn use_leaves(item: &syn::ItemUse) -> Vec<UseLeaf> {
    let mut leaves = Vec::new();
    flatten_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
    leaves
}

fn nested_meta(list: &syn::MetaList) -> Punctuated<Meta, Token![,]> {
    Punctuated::<Meta, Token![,]>::parse_terminated
        .parse2(list.tokens.clone())
        .expect("nested meta parses")
}

fn meta_has_feature(meta: &Meta, feature: &str) -> bool {
    matches!(
        meta,
        Meta::NameValue(value)
            if value.path.is_ident("feature")
                && matches!(
                    &value.value,
                    Expr::Lit(literal)
                        if matches!(&literal.lit, Lit::Str(value) if value.value() == feature)
                )
    )
}

fn has_cfg_feature(attrs: &[Attribute], feature: &str) -> bool {
    attrs.iter().any(|attr| {
        let Meta::List(cfg) = &attr.meta else {
            return false;
        };
        if !cfg.path.is_ident("cfg") {
            return false;
        }
        let nested = nested_meta(cfg);
        nested.len() == 1 && nested.first().is_some_and(|meta| meta_has_feature(meta, feature))
    })
}
fn meta_mentions_feature(meta: &Meta, feature: &str) -> bool {
    meta_has_feature(meta, feature)
        || matches!(
            meta,
            Meta::List(list)
                if nested_meta(list).iter().any(|nested| meta_mentions_feature(nested, feature))
        )
}

fn cfg_mentions_feature(attrs: &[Attribute], feature: &str) -> bool {
    attrs.iter().any(|attr| {
        matches!(
            &attr.meta,
            Meta::List(cfg)
                if cfg.path.is_ident("cfg")
                    && nested_meta(cfg).iter().any(|meta| meta_mentions_feature(meta, feature))
        )
    })
}

fn public_item_attrs(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Enum(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::ExternCrate(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Fn(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Mod(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Macro(item) if item.attrs.iter().any(|attr| attr.path().is_ident("macro_export")) => {
            Some(&item.attrs)
        }
        Item::Static(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Struct(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Trait(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::TraitAlias(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Type(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Union(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        Item::Use(item) if matches!(item.vis, Visibility::Public(_)) => Some(&item.attrs),
        _ => None,
    }
}

fn meta_requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => {
            nested_meta(list).iter().any(meta_requires_test)
        }
        Meta::List(list) if list.path.is_ident("any") => {
            let nested = nested_meta(list);
            !nested.is_empty() && nested.iter().all(meta_requires_test)
        }
        Meta::List(_) | Meta::NameValue(_) => false,
    }
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let Meta::List(cfg) = &attr.meta else {
            return false;
        };
        cfg.path.is_ident("cfg") && nested_meta(cfg).iter().any(meta_requires_test)
    })
}

fn item_struct<'a>(file: &'a File, name: &str) -> &'a ItemStruct {
    file.items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == name => Some(item),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing struct {name}"))
}

fn self_type_name(item: &ItemImpl) -> Option<String> {
    let Type::Path(path) = item.self_ty.as_ref() else {
        return None;
    };
    path.path.segments.last().map(|segment| segment.ident.to_string())
}

struct AuthoritySurfaceInventory<'a> {
    submit_lib: &'a File,
    authority: &'a File,
}

impl<'a> AuthoritySurfaceInventory<'a> {
    const fn new(submit_lib: &'a File, authority: &'a File) -> Self {
        Self { submit_lib, authority }
    }

    fn root_exports(&self, feature: &str) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut exports = BTreeSet::new();
        let mut violations = BTreeSet::new();
        for item in &self.submit_lib.items {
            let Some(attrs) = public_item_attrs(item) else {
                continue;
            };
            if feature != "t4d-bridge" && !matches!(item, Item::Use(_)) {
                continue;
            }
            if !cfg_mentions_feature(attrs, feature) {
                continue;
            }
            let Item::Use(item) = item else {
                violations.insert(format!("non-use public root item: {item:?}"));
                continue;
            };
            if !has_cfg_feature(&item.attrs, feature) {
                violations.insert(format!("non-exact feature gate: {item:?}"));
                continue;
            }
            for leaf in use_leaves(item) {
                let authority_export = leaf.source.first().map(String::as_str)
                    == Some("tx_authority")
                    && leaf.source.len() == 2;
                let economics_export =
                    leaf.source.as_slice() == ["economics", "PriorityEconomicsAuthority"];
                if (!authority_export && !economics_export) || leaf.renamed || leaf.glob {
                    violations.insert(format!("{:?} as {}", leaf.source, leaf.public_name));
                } else {
                    exports.insert(leaf.public_name);
                }
            }
        }
        (exports, violations)
    }

    fn assert_private_module(&self, name: &str) {
        let modules = self
            .submit_lib
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Mod(item) if item.ident == name => Some(item),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(modules.len(), 1, "expected one tx-authority module `{name}`");
        assert_eq!(modules[0].attrs.len(), 1, "module `{name}` has ambiguous attributes");
        assert!(
            has_cfg_feature(&modules[0].attrs, "tx-authority"),
            "module `{name}` escaped the tx-authority feature gate"
        );
        assert!(
            matches!(modules[0].vis, Visibility::Inherited),
            "tx-authority module `{name}` must remain private"
        );
        assert!(
            modules[0].content.is_none() && modules[0].semi.is_some(),
            "tx-authority module `{name}` must remain an out-of-line declaration"
        );
    }

    fn public_impl_items(&self, type_name: &str) -> BTreeSet<String> {
        let mut surface = BTreeSet::new();
        for item in &self.authority.items {
            let Item::Impl(item) = item else {
                continue;
            };
            if item.trait_.is_some() || self_type_name(item).as_deref() != Some(type_name) {
                continue;
            }
            for member in &item.items {
                match member {
                    ImplItem::Fn(method) if matches!(method.vis, Visibility::Public(_)) => {
                        surface.insert(format!("fn:{}", method.sig.ident));
                    }
                    ImplItem::Const(value) if matches!(value.vis, Visibility::Public(_)) => {
                        surface.insert(format!("const:{}", value.ident));
                    }
                    ImplItem::Type(value) if matches!(value.vis, Visibility::Public(_)) => {
                        surface.insert(format!("type:{}", value.ident));
                    }
                    _ => {}
                }
            }
        }
        surface
    }

    fn implemented_traits(&self, type_name: &str) -> BTreeSet<String> {
        self.authority
            .items
            .iter()
            .filter_map(|item| {
                let Item::Impl(item) = item else {
                    return None;
                };
                if self_type_name(item).as_deref() != Some(type_name) {
                    return None;
                }
                item.trait_
                    .as_ref()
                    .and_then(|(_, path, _)| path.segments.last())
                    .map(|segment| segment.ident.to_string())
            })
            .collect()
    }

    fn derived_traits(&self, type_name: &str) -> BTreeSet<String> {
        let mut derived = BTreeSet::new();
        for attr in &item_struct(self.authority, type_name).attrs {
            if !attr.path().is_ident("derive") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if let Some(segment) = meta.path.segments.last() {
                    derived.insert(segment.ident.to_string());
                }
                Ok(())
            })
            .expect("derive attribute parses");
        }
        derived
    }

    fn inherent_method(&self, type_name: &str, method_name: &str) -> &ImplItemFn {
        self.authority
            .items
            .iter()
            .find_map(|item| {
                let Item::Impl(item) = item else {
                    return None;
                };
                if item.trait_.is_some() || self_type_name(item).as_deref() != Some(type_name) {
                    return None;
                }
                item.items.iter().find_map(|member| match member {
                    ImplItem::Fn(method) if method.sig.ident == method_name => Some(method),
                    _ => None,
                })
            })
            .unwrap_or_else(|| panic!("missing {type_name}::{method_name}"))
    }
}

fn method_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| format!("fn:{name}")).collect()
}

fn member_is(member: &Member, expected: &str) -> bool {
    matches!(member, Member::Named(name) if name == expected)
}

fn path_is(expr: &Expr, expected: &str) -> bool {
    matches!(
        expr,
        Expr::Path(ExprPath { path, .. })
            if path.segments.len() == 1 && path.segments[0].ident == expected
    )
}

fn literal_index(expr: &Expr) -> Option<usize> {
    let Expr::Lit(literal) = expr else {
        return None;
    };
    let Lit::Int(value) = &literal.lit else {
        return None;
    };
    value.base10_parse().ok()
}

fn route_protocol_index(expr: &Expr) -> Option<usize> {
    let Expr::Field(protocol) = expr else {
        return None;
    };
    if !member_is(&protocol.member, "protocol") {
        return None;
    }
    let Expr::Index(index) = protocol.base.as_ref() else {
        return None;
    };
    let Expr::Field(route) = index.expr.as_ref() else {
        return None;
    };
    if !member_is(&route.member, "route") || !path_is(&route.base, "plan") {
        return None;
    }
    literal_index(&index.index)
}

fn mapping_projection(expr: &Expr, projection: &str) -> bool {
    let Expr::MethodCall(map) = expr else {
        return false;
    };
    if map.method != "map" || !path_is(&map.receiver, "hop_protocols") || map.args.len() != 1 {
        return false;
    }
    let Some(Expr::Closure(closure)) = map.args.first() else {
        return false;
    };
    let Some(Pat::Ident(parameter)) = closure.inputs.first() else {
        return false;
    };
    let parameter = parameter.ident.to_string();
    let Expr::Field(projected) = closure.body.as_ref() else {
        return false;
    };
    if !member_is(&projected.member, projection) {
        return false;
    }
    let Expr::MethodCall(resolve) = projected.base.as_ref() else {
        return false;
    };
    if resolve.method != "resolve"
        || resolve.args.len() != 1
        || !resolve.args.first().is_some_and(|argument| path_is(argument, &parameter))
    {
        return false;
    }
    let Expr::Field(adapters) = resolve.receiver.as_ref() else {
        return false;
    };
    member_is(&adapters.member, "adapters") && path_is(&adapters.base, "execution")
}

fn local_initializer<'a>(method: &'a ImplItemFn, name: &str) -> &'a Expr {
    method
        .block
        .stmts
        .iter()
        .find_map(|statement| {
            let Stmt::Local(local) = statement else {
                return None;
            };
            let Pat::Ident(pattern) = &local.pat else {
                return None;
            };
            if pattern.ident != name {
                return None;
            }
            local.init.as_ref().map(|init| init.expr.as_ref())
        })
        .unwrap_or_else(|| panic!("missing local `{name}`"))
}

fn assert_array_field(item: &ItemStruct, field_name: &str, element_name: &str, length: usize) {
    let field = item
        .fields
        .iter()
        .find(|field| field.ident.as_ref().is_some_and(|ident| ident == field_name))
        .unwrap_or_else(|| panic!("missing field {field_name}"));
    let Type::Array(array) = &field.ty else {
        panic!("{field_name} is not an array");
    };
    let Type::Path(element) = array.elem.as_ref() else {
        panic!("{field_name} element is not a path");
    };
    assert_eq!(
        element.path.segments.last().map(|segment| segment.ident.to_string()).as_deref(),
        Some(element_name),
        "{field_name} element type changed"
    );
    assert_eq!(literal_index(&array.len), Some(length), "{field_name} cardinality changed");
}

fn field_binds_ident(field: &syn::FieldValue, expected: &str) -> bool {
    member_is(&field.member, expected) && path_is(&field.expr, expected)
}

#[derive(Default)]
struct WitnessConstructionSeal {
    observation_fields: Vec<Vec<syn::FieldValue>>,
    output_fields: Vec<Vec<syn::FieldValue>>,
    installed_identity_constructions: usize,
}

impl<'ast> Visit<'ast> for WitnessConstructionSeal {
    fn visit_expr_struct(&mut self, item: &'ast ExprStruct) {
        match item.path.segments.last().map(|segment| segment.ident.to_string()).as_deref() {
            Some("UnsignedTxShapeObservation") => {
                self.observation_fields.push(item.fields.iter().cloned().collect());
            }
            Some("ValidatedUnsignedAtomicTx") => {
                self.output_fields.push(item.fields.iter().cloned().collect());
            }
            Some("InstalledExecutionIdentity") => self.installed_identity_constructions += 1,
            _ => {}
        }
        syn::visit::visit_expr_struct(self, item);
    }
}

#[derive(Default)]
struct RawTxCapabilitySeal {
    token_definitions: usize,
    token_private_fields: usize,
    token_constructions: usize,
    accessor_definitions: usize,
    valid_accessor_signatures: usize,
    accessor_calls: usize,
    macro_escapes: usize,
}

impl RawTxCapabilitySeal {
    fn is_capability_type(ty: &Type) -> bool {
        let Type::Reference(reference) = ty else {
            return false;
        };
        let Type::Path(path) = reference.elem.as_ref() else {
            return false;
        };
        path.path.segments.last().is_some_and(|segment| segment.ident == "BridgeConversionSeal")
    }
}

impl<'ast> Visit<'ast> for RawTxCapabilitySeal {
    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        if item.ident == "BridgeConversionSeal" {
            self.token_definitions += 1;
            self.token_private_fields += item
                .fields
                .iter()
                .filter(|field| matches!(field.vis, Visibility::Inherited))
                .count();
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_expr_struct(&mut self, item: &'ast ExprStruct) {
        if item.path.segments.last().is_some_and(|segment| segment.ident == "BridgeConversionSeal")
        {
            self.token_constructions += 1;
        }
        syn::visit::visit_expr_struct(self, item);
    }

    fn visit_expr_method_call(&mut self, item: &'ast ExprMethodCall) {
        if item.method == "unsigned_tx_with_bridge_access" {
            self.accessor_calls += 1;
        }
        syn::visit::visit_expr_method_call(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        if item.sig.ident == "unsigned_tx_with_bridge_access" {
            self.accessor_definitions += 1;
            let mut inputs = item.sig.inputs.iter();
            let receiver_is_shared = inputs.next().is_some_and(|input| {
                matches!(
                    input,
                    syn::FnArg::Receiver(receiver)
                        if receiver.reference.is_some() && receiver.mutability.is_none()
                )
            });
            let capability_is_shared = inputs.next().is_some_and(|input| {
                matches!(
                    input,
                    syn::FnArg::Typed(argument) if Self::is_capability_type(&argument.ty)
                )
            });
            if matches!(item.vis, Visibility::Restricted(_))
                && item.sig.constness.is_some()
                && receiver_is_shared
                && capability_is_shared
                && inputs.next().is_none()
            {
                self.valid_accessor_signatures += 1;
            }
        }
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_fn(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_macro(&mut self, item: &'ast Macro) {
        let tokens = item.tokens.to_string();
        if tokens.contains("BridgeConversionSeal")
            || tokens.contains("unsigned_tx_with_bridge_access")
        {
            self.macro_escapes += 1;
        }
        syn::visit::visit_macro(self, item);
    }
}

fn rust_files(root: &FsPath) -> Vec<PathBuf> {
    fn collect(path: &FsPath, files: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                collect(&entry, files);
            } else if entry.extension().is_some_and(|extension| extension == "rs") {
                files.push(entry);
            }
        }
    }

    let mut files = Vec::new();
    collect(root, &mut files);
    files
}

fn workspace_submit_linkers(root: &PathBuf) -> Vec<(String, PathBuf)> {
    let mut command = Command::new(env!("CARGO"));
    command.args(["metadata", "--no-deps", "--format-version", "1", "--offline"]);
    let output = command.current_dir(root).output().expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata JSON");
    let mut linkers = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .filter_map(|package| {
            let links_submit =
                package["dependencies"].as_array().expect("dependencies").iter().any(
                    |dependency| {
                        if dependency["name"] != "mev-trader-submit" {
                            return false;
                        }
                        assert!(
                            dependency["rename"].is_null(),
                            "{} aliases the submit crate in Cargo metadata",
                            package["name"]
                        );
                        true
                    },
                );
            if !links_submit {
                return None;
            }
            let name = package["name"].as_str().expect("package name").to_owned();
            let manifest = PathBuf::from(package["manifest_path"].as_str().expect("manifest path"));
            Some((name, manifest.parent().expect("manifest parent").join("src")))
        })
        .collect::<Vec<_>>();
    linkers.sort();
    linkers
}

#[derive(Default)]
struct ImportedAuthority {
    aliases: BTreeSet<String>,
    crate_aliases: BTreeSet<String>,
    imports: usize,
    public_reexports: usize,
    violations: BTreeSet<String>,
}

fn authority_feature_for(name: &str) -> Option<&'static str> {
    match name {
        "ValidatedUnsignedAtomicTx" => Some("t4b-shadow"),
        "InstalledSubmissionBridge" | "SealedUnsignedCandidate" => Some("t4d-shadow"),
        _ => None,
    }
}

struct ImportedAuthorityCollector {
    external: bool,
    inventory: ImportedAuthority,
}

impl ImportedAuthorityCollector {
    fn inspect_use(&mut self, item: &syn::ItemUse) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        for leaf in use_leaves(item) {
            let first = leaf.source.first().map(String::as_str);
            let last = leaf.source.last().map(String::as_str);
            let direct_crate_alias = leaf.source.len() == 1;
            let grouped_self_alias = leaf.source.len() == 2 && last == Some("self");
            if self.external
                && first == Some("mev_trader_submit")
                && (direct_crate_alias || grouped_self_alias)
            {
                self.inventory.crate_aliases.insert(leaf.public_name.clone());
                self.inventory
                    .violations
                    .insert(format!("non-canonical crate alias {}", leaf.public_name));
                continue;
            }
            let Some(required_feature) = last.and_then(authority_feature_for) else {
                continue;
            };
            let from_external = first == Some("mev_trader_submit");
            let from_authority = leaf.source.iter().any(|segment| segment == "tx_authority")
                || matches!(first, Some("crate" | "self" | "super"))
                    && !leaf.source.iter().any(|segment| segment == "assembler");
            if from_external || (!self.external && from_authority) {
                self.inventory.imports += 1;
                self.inventory.aliases.insert(leaf.public_name.clone());
                if matches!(item.vis, Visibility::Public(_)) {
                    self.inventory.public_reexports += 1;
                }
                if leaf.glob || leaf.renamed {
                    self.inventory.violations.insert(format!(
                        "non-canonical authority import {:?} as {}",
                        leaf.source, leaf.public_name
                    ));
                }
                if self.external && !has_cfg_feature(&item.attrs, required_feature) {
                    self.inventory
                        .violations
                        .insert(format!("external authority import escaped {required_feature}"));
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for ImportedAuthorityCollector {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.inspect_use(item);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if self.external && item.ident == "mev_trader_submit" {
            let alias = item
                .rename
                .as_ref()
                .map_or_else(|| item.ident.to_string(), |(_, alias)| alias.to_string());
            self.inventory.crate_aliases.insert(alias.clone());
            self.inventory.violations.insert(format!("non-canonical extern crate alias {alias}"));
        }
    }

    fn visit_macro(&mut self, item: &'ast Macro) {
        let tokens = item.tokens.to_string();
        if item.path.segments.last().is_some_and(|segment| segment.ident == "include")
            || [
                "ValidatedUnsignedAtomicTx",
                "InstalledSubmissionBridge",
                "SealedUnsignedCandidate",
                "mev_trader_submit",
            ]
            .iter()
            .any(|authority| tokens.contains(authority))
        {
            self.inventory.violations.insert("macro authority redirect".to_owned());
        }
        syn::visit::visit_macro(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        if item.attrs.iter().any(|attr| attr.path().is_ident("path")) {
            self.inventory.violations.insert("path module authority redirect".to_owned());
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }
}

fn imported_authority(file: &File, external: bool) -> ImportedAuthority {
    let mut collector =
        ImportedAuthorityCollector { external, inventory: ImportedAuthority::default() };
    collector.visit_file(file);
    collector.inventory
}

struct AuthorityUseVisitor<'a> {
    aliases: &'a BTreeSet<String>,
    crate_aliases: &'a BTreeSet<String>,
    uses: usize,
    slot_uses: usize,
}

impl<'a> AuthorityUseVisitor<'a> {
    const fn new(aliases: &'a BTreeSet<String>, crate_aliases: &'a BTreeSet<String>) -> Self {
        Self { aliases, crate_aliases, uses: 0, slot_uses: 0 }
    }

    fn path_is_authority(&self, path: &Path) -> bool {
        let segments =
            path.segments.iter().map(|segment| segment.ident.to_string()).collect::<Vec<_>>();
        let Some(last) = segments.last() else {
            return false;
        };
        if segments.len() == 1 && self.aliases.contains(last) {
            return true;
        }
        if authority_feature_for(last).is_none() {
            return false;
        }
        segments
            .first()
            .is_some_and(|first| first == "mev_trader_submit" || self.crate_aliases.contains(first))
            || segments.iter().any(|segment| segment == "tx_authority")
    }

    fn type_is_authority(&self, ty: &Type) -> bool {
        matches!(ty, Type::Path(path) if self.path_is_authority(&path.path))
    }
}

impl<'ast> Visit<'ast> for AuthorityUseVisitor<'_> {
    fn visit_type_path(&mut self, item: &'ast TypePath) {
        if self.path_is_authority(&item.path) {
            self.uses += 1;
        }
        if item.path.segments.last().is_some_and(|segment| segment.ident == "ShadowLatestSlot")
            && let Some(segment) = item.path.segments.last()
            && let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments
            && arguments.args.iter().any(|argument| {
                matches!(argument, syn::GenericArgument::Type(ty) if self.type_is_authority(ty))
            })
        {
            self.slot_uses += 1;
        }
        syn::visit::visit_type_path(self, item);
    }

    fn visit_expr_path(&mut self, item: &'ast ExprPath) {
        if self.path_is_authority(&item.path) {
            self.uses += 1;
        }
        syn::visit::visit_expr_path(self, item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_fn(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_impl(self, item);
        }
    }
}

fn authoritative_uses(file: &File, external: bool) -> (ImportedAuthority, usize, usize) {
    let imports = imported_authority(file, external);
    let (uses, slot_uses) = {
        let mut visitor = AuthorityUseVisitor::new(&imports.aliases, &imports.crate_aliases);
        visitor.visit_file(file);
        (visitor.uses, visitor.slot_uses)
    };
    (imports, uses, slot_uses)
}

#[test]
fn t4c_tx_authority_public_surface_is_exact_and_nonforgeable() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let submit_lib =
        syn::parse_file(&read(crate_dir.join("src/lib.rs"))).expect("submit lib parses");
    let authority = parse_production(&read(crate_dir.join("src/tx_authority.rs")));
    let inventory = AuthoritySurfaceInventory::new(&submit_lib, &authority);

    let (exports, export_violations) = inventory.root_exports("tx-authority");
    assert!(export_violations.is_empty(), "invalid tx-authority exports: {export_violations:?}");
    assert_eq!(
        exports,
        BTreeSet::from([
            "DeployedContractIdentity".to_owned(),
            "InstalledExecutionIdentity".to_owned(),
            "ProtocolAdapterMapping".to_owned(),
            "PriorityEconomicsAuthority".to_owned(),
            "SnapshotFreshnessToken".to_owned(),
            "TxAuthorityAssembler".to_owned(),
            "TxAuthorityError".to_owned(),
            "TxAuthorityNodeError".to_owned(),
            "TxAuthorityNodeView".to_owned(),
            "TxAuthorityStateRead".to_owned(),
            "UnsignedTxShapeObservation".to_owned(),
            "ValidatedUnsignedAtomicTx".to_owned(),
        ])
    );
    let (bridge_exports, bridge_export_violations) = inventory.root_exports("t4d-bridge");
    assert!(
        bridge_export_violations.is_empty(),
        "invalid t4d-bridge exports: {bridge_export_violations:?}"
    );
    assert_eq!(
        bridge_exports,
        BTreeSet::from([
            "AdapterAwareProofBindings".to_owned(),
            "BridgeError".to_owned(),
            "InstalledSubmissionBridge".to_owned(),
            "SealedUnsignedCandidate".to_owned(),
        ])
    );
    let bridge = parse_production(&read(crate_dir.join("src/tx_authority/bridge.rs")));
    let bridge_inventory = AuthoritySurfaceInventory::new(&submit_lib, &bridge);
    let bridge_types = [
        "AdapterAwareProofBindings",
        "BridgeConversionSeal",
        "InstalledSubmissionBridge",
        "SealedUnsignedCandidate",
    ];
    assert_private_fields(&bridge, &bridge_types);
    let bridge_methods = BTreeMap::from([
        (
            "AdapterAwareProofBindings",
            method_set(&[
                "executor",
                "frame",
                "nonce",
                "plan_digest",
                "route_adapters",
                "route_protocols",
                "sender",
                "unsigned_signing_hash",
                "valid_until_block",
                "validated_parent",
                "victim",
            ]),
        ),
        (
            "InstalledSubmissionBridge",
            method_set(&[
                "assemble_sealed",
                "base_mainnet",
                "into_checked_candidate",
                "revalidate_for_handoff",
            ]),
        ),
        ("SealedUnsignedCandidate", method_set(&["bindings"])),
    ]);
    for (type_name, expected) in bridge_methods {
        assert_eq!(
            bridge_inventory.public_impl_items(type_name),
            expected,
            "{type_name} public bridge surface changed"
        );
    }
    assert_eq!(
        bridge_inventory.implemented_traits("SealedUnsignedCandidate"),
        BTreeSet::from(["Debug".to_owned()])
    );
    let forbidden_bridge_traits = BTreeSet::from([
        "Clone".to_owned(),
        "Copy".to_owned(),
        "Default".to_owned(),
        "Deserialize".to_owned(),
        "From".to_owned(),
        "Serialize".to_owned(),
        "TryFrom".to_owned(),
    ]);
    for type_name in bridge_types {
        assert!(
            bridge_inventory.implemented_traits(type_name).is_disjoint(&forbidden_bridge_traits),
            "{type_name} implements a forgeable bridge trait"
        );
        assert!(
            bridge_inventory.derived_traits(type_name).is_disjoint(&forbidden_bridge_traits),
            "{type_name} derives a forgeable bridge trait"
        );
    }
    for escaped in [
        "#[cfg(feature = \"t4d-bridge\")] pub type Extra = alloy_consensus::TxEip1559;",
        "#[cfg(feature = \"t4d-bridge\")] pub fn extra() {}",
        "#[cfg(feature = \"t4d-bridge\")] pub mod extra {}",
        "#[cfg(any(feature = \"t4d-bridge\", feature = \"arm\"))] pub const EXTRA: u8 = 0;",
        "#[cfg(feature = \"t4d-bridge\")] #[macro_export] macro_rules! extra { () => {} }",
    ] {
        let escaped_root = syn::parse_file(escaped).expect("root escape fixture parses");
        let escaped_inventory = AuthoritySurfaceInventory::new(&escaped_root, &bridge);
        let (_, violations) = escaped_inventory.root_exports("t4d-bridge");
        assert!(!violations.is_empty(), "non-use or widened T4d root escape was accepted");
    }
    for item in &submit_lib.items {
        let Item::Use(item) = item else {
            continue;
        };
        let leaves = use_leaves(item);
        if !leaves
            .iter()
            .any(|leaf| leaf.source.first().map(String::as_str) == Some("tx_authority"))
        {
            continue;
        }
        assert_eq!(item.attrs.len(), 1, "authority root export has ambiguous attributes");
        let exact_authority_gate_count = ["tx-authority", "t4d-bridge", "t4e-handoff"]
            .into_iter()
            .filter(|feature| has_cfg_feature(&item.attrs, feature))
            .count();
        assert_eq!(
            exact_authority_gate_count, 1,
            "authority root export escaped the exact reviewed feature gates"
        );
    }
    inventory.assert_private_module("tx_authority");
    inventory.assert_private_module("calldata");

    let authority_types = [
        "ProtocolAdapterMapping",
        "InstalledExecutionIdentity",
        "TxAuthorityAssembler",
        "UnsignedTxShapeObservation",
        "ValidatedUnsignedAtomicTx",
    ];
    assert_private_fields(&authority, &authority_types);

    let expected_methods = BTreeMap::from([
        ("ProtocolAdapterMapping", method_set(&["base_mainnet_pins", "resolve"])),
        (
            "InstalledExecutionIdentity",
            method_set(&["executor", "adapters", "sender", "validated_parent"]),
        ),
        ("TxAuthorityAssembler", method_set(&["base_mainnet", "assemble_validated"])),
        (
            "UnsignedTxShapeObservation",
            method_set(&[
                "frame",
                "victim",
                "plan_digest",
                "sender",
                "nonce",
                "chain_id",
                "executor",
                "hop_protocols",
                "hop_adapters",
                "hop_runtime_hashes",
                "gas_limit",
                "max_fee_per_gas",
                "max_priority_fee_per_gas",
                "base_fee",
                "valid_until_block",
                "unsigned_signing_hash",
            ]),
        ),
        (
            "ValidatedUnsignedAtomicTx",
            method_set(&["observation", "execution", "validate_at_drain"]),
        ),
    ]);
    for (type_name, expected) in expected_methods {
        assert_eq!(
            inventory.public_impl_items(type_name),
            expected,
            "{type_name} public surface changed"
        );
    }

    let forbidden_traits = BTreeSet::from([
        "Default".to_owned(),
        "From".to_owned(),
        "TryFrom".to_owned(),
        "Deserialize".to_owned(),
    ]);
    for type_name in authority_types {
        let implemented = inventory.implemented_traits(type_name);
        let derived = inventory.derived_traits(type_name);
        assert!(
            implemented.is_disjoint(&forbidden_traits),
            "{type_name} implements a forgeable trait: {implemented:?}"
        );
        assert!(
            derived.is_disjoint(&forbidden_traits),
            "{type_name} derives a forgeable trait: {derived:?}"
        );
    }
    let serialized = BTreeSet::from(["Serialize".to_owned(), "Deserialize".to_owned()]);
    assert!(inventory.implemented_traits("ValidatedUnsignedAtomicTx").is_disjoint(&serialized));
    assert!(inventory.derived_traits("ValidatedUnsignedAtomicTx").is_disjoint(&serialized));
}

#[test]
fn t4e_raw_tx_access_requires_the_unique_bridge_capability() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_dir = crate_dir.join("src");
    let mut constructor_files = BTreeMap::new();
    let mut accessor_definition_files = BTreeMap::new();
    let mut accessor_call_files = BTreeMap::new();
    let mut totals = RawTxCapabilitySeal::default();

    for path in rust_files(&source_dir) {
        let source = read(path.clone());
        let file = parse_production(&source);
        let mut seal = RawTxCapabilitySeal::default();
        seal.visit_file(&file);
        let relative = path
            .strip_prefix(&source_dir)
            .expect("submit source is below src")
            .to_string_lossy()
            .replace('\\', "/");
        if seal.token_constructions != 0 {
            constructor_files.insert(relative.clone(), seal.token_constructions);
        }
        if seal.accessor_definitions != 0 {
            accessor_definition_files.insert(relative.clone(), seal.accessor_definitions);
        }
        if seal.accessor_calls != 0 {
            accessor_call_files.insert(relative, seal.accessor_calls);
        }
        totals.token_definitions += seal.token_definitions;
        totals.token_private_fields += seal.token_private_fields;
        totals.token_constructions += seal.token_constructions;
        totals.accessor_definitions += seal.accessor_definitions;
        totals.valid_accessor_signatures += seal.valid_accessor_signatures;
        totals.accessor_calls += seal.accessor_calls;
        totals.macro_escapes += seal.macro_escapes;
    }

    assert_eq!(totals.token_definitions, 1, "bridge access token type count changed");
    assert_eq!(totals.token_private_fields, 1, "bridge access token must remain unforgeable");
    assert_eq!(
        constructor_files,
        BTreeMap::from([("tx_authority/bridge.rs".to_owned(), 1)]),
        "only bridge revalidation may mint raw-tx access"
    );
    assert_eq!(
        accessor_definition_files,
        BTreeMap::from([("tx_authority.rs".to_owned(), 1)]),
        "raw-tx accessor definition inventory changed"
    );
    assert_eq!(
        totals.valid_accessor_signatures, 1,
        "raw-tx accessor must require shared access to the unforgeable token"
    );
    assert_eq!(
        accessor_call_files,
        BTreeMap::from([("arm/witness.rs".to_owned(), 1)]),
        "only the arm witness may consume authority raw-tx access"
    );
    assert_eq!(totals.macro_escapes, 0, "raw-tx capability escaped through a macro");
}

#[test]
fn t4c_adapter_witness_surface_is_route_exact_and_unforgeable() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let authority = parse_production(&read(crate_dir.join("src/tx_authority.rs")));
    let submit_lib =
        syn::parse_file(&read(crate_dir.join("src/lib.rs"))).expect("submit lib parses");
    let inventory = AuthoritySurfaceInventory::new(&submit_lib, &authority);
    let observation = item_struct(&authority, "UnsignedTxShapeObservation");
    assert_array_field(observation, "hop_protocols", "ExactProtocol", 2);
    assert_array_field(observation, "hop_adapters", "Address", 2);
    assert_array_field(observation, "hop_runtime_hashes", "B256", 2);

    let assemble = inventory.inherent_method("TxAuthorityAssembler", "assemble_view");
    let protocols = local_initializer(assemble, "hop_protocols");
    let Expr::Array(protocols) = protocols else {
        panic!("hop_protocols is not an array");
    };
    assert_eq!(
        protocols.elems.iter().map(route_protocol_index).collect::<Vec<_>>(),
        [Some(0), Some(1)]
    );
    assert!(mapping_projection(local_initializer(assemble, "hop_adapters"), "address"));
    assert!(mapping_projection(local_initializer(assemble, "hop_runtime_hashes"), "runtime_hash"));

    let mut constructions = WitnessConstructionSeal::default();
    constructions.visit_block(&assemble.block);
    assert_eq!(constructions.observation_fields.len(), 1);
    assert_eq!(constructions.output_fields.len(), 1);
    assert_eq!(constructions.installed_identity_constructions, 0);
    let observation_fields = &constructions.observation_fields[0];
    for field in ["hop_protocols", "hop_adapters", "hop_runtime_hashes"] {
        assert!(
            observation_fields.iter().any(|value| field_binds_ident(value, field)),
            "observation no longer binds `{field}` directly"
        );
    }
    let output_fields = &constructions.output_fields[0];
    for field in ["unsigned_tx", "observation", "execution"] {
        assert!(
            output_fields.iter().any(|value| field_binds_ident(value, field)),
            "linear output no longer retains `{field}`"
        );
    }

    let mapping = ProtocolAdapterMapping::base_mainnet_pins();
    assert_eq!(
        mapping.resolve(ExactProtocol::AerodromeVolatile),
        mapping.resolve(ExactProtocol::AerodromeStable)
    );
    assert_private_fields(
        &authority,
        &[
            "ProtocolAdapterMapping",
            "InstalledExecutionIdentity",
            "UnsignedTxShapeObservation",
            "ValidatedUnsignedAtomicTx",
        ],
    );
    assert_eq!(
        inventory.public_impl_items("ProtocolAdapterMapping"),
        method_set(&["base_mainnet_pins", "resolve"])
    );
}

#[test]
fn t4d_workspace_has_exactly_one_facade_install_observer_and_slot() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../../..");
    let linkers = workspace_submit_linkers(&root);
    assert_eq!(
        linkers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
        ["base-execution-cli"]
    );

    let mut consumers = Vec::new();
    for (package, source_root) in &linkers {
        for path in rust_files(source_root) {
            let source = read(path.clone());
            let file = syn::parse_file(&source)
                .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
            let (imports, uses, slot_uses) = authoritative_uses(&file, true);
            assert!(
                imports.violations.is_empty(),
                "{} has ambiguous authority import: {:?}",
                path.display(),
                imports.violations
            );
            assert_eq!(
                imports.public_reexports,
                0,
                "{} re-exports unsigned authority",
                path.display()
            );
            if imports.imports != 0 || uses != 0 || slot_uses != 0 {
                consumers.push((package.clone(), path, imports.imports, uses, slot_uses));
            }
        }
    }
    assert_eq!(consumers.len(), 1, "unexpected unsigned authority consumers: {consumers:?}");
    let (package, path, imports, uses, slot_uses) = &consumers[0];
    assert_eq!(package, "base-execution-cli");
    assert!(path.ends_with("crates/execution/cli/src/mev_trader.rs"));
    assert_eq!((*imports, *uses, *slot_uses), (3, 3, 2));

    let mut global_topology = [0usize; 11];
    for path in rust_files(&root.join("crates/execution/cli/src")) {
        let file = parse_production(&read(path));
        let mut seal = AstSeal::default();
        seal.visit_file(&file);
        let topology_relevant = seal.observer_impls != 0
            || seal.node_view_impls != 0
            || seal.slot_constructions != 0
            || seal.observer_factories != 0
            || seal.observer_install_calls != 0
            || seal.base_mainnet_calls != 0
            || seal.assemble_sealed_calls != 0
            || seal.t4d_observer_calls != 0;
        global_topology[0] += seal.observer_impls;
        global_topology[1] += seal.node_view_impls;
        global_topology[2] += seal.slot_constructions;
        global_topology[3] += seal.observer_factories;
        global_topology[4] += seal.observer_install_calls;
        global_topology[5] += seal.base_mainnet_calls;
        global_topology[6] += seal.assemble_sealed_calls;
        global_topology[7] += seal.t4d_observer_calls;
        if topology_relevant {
            global_topology[8] += seal.spawn_calls;
            global_topology[9] += seal.thread_calls;
            global_topology[10] += seal.subscription_calls;
        }
    }
    assert_eq!(
        global_topology,
        // Seven pre-existing production task spawns live outside the T4d module.
        // The module-local seal below remains pinned to zero for this handoff.
        [2, 1, 2, 2, 4, 2, 1, 1, 7, 0, 1],
        "CLI-wide T4d observer topology changed"
    );
    let submit_src = crate_dir.join("src");
    let mut internal_consumers = Vec::new();
    for path in rust_files(&submit_src) {
        if path.ends_with("lib.rs") || path.ends_with("tx_authority.rs") {
            continue;
        }
        let source = read(path.clone());
        let file = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let (imports, uses, slot_uses) = authoritative_uses(&file, false);
        assert!(
            imports.violations.is_empty(),
            "{} has ambiguous internal authority import: {:?}",
            path.display(),
            imports.violations
        );
        assert_eq!(imports.public_reexports, 0, "{} re-exports private authority", path.display());
        if imports.imports != 0 || uses != 0 || slot_uses != 0 {
            internal_consumers.push((path, imports.imports, uses, slot_uses));
        }
    }
    assert_eq!(internal_consumers.len(), 2, "unreviewed submit-private consumer count");
    let internal_by_file = internal_consumers
        .iter()
        .map(|(path, imports, uses, slot_uses)| {
            (
                path.file_name().expect("consumer file name").to_string_lossy().into_owned(),
                (*imports, *uses, *slot_uses),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        internal_by_file,
        BTreeMap::from(
            [("bridge.rs".to_owned(), (1, 2, 0)), ("witness.rs".to_owned(), (1, 2, 0)),]
        ),
        "only the bridge and exhaustive arm witness may consume T4b authority"
    );
    let authority_source = read(crate_dir.join("src/tx_authority.rs"));
    let authority_ast = parse_production(&authority_source);
    let bridge_modules = authority_ast
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Mod(module) if module.ident == "bridge" => Some(module),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(bridge_modules.len(), 1);
    assert_eq!(bridge_modules[0].attrs.len(), 1, "bridge module has ambiguous attributes");
    assert!(has_cfg_feature(&bridge_modules[0].attrs, "t4d-bridge"));
    assert!(matches!(bridge_modules[0].vis, Visibility::Inherited));
    assert!(bridge_modules[0].content.is_none());
    assert!(bridge_modules[0].semi.is_some());
    assert!(
        !crate_dir.join("src/tx_authority/bridge/mod.rs").exists(),
        "bridge module has an ambiguous alternate source"
    );

    let cli_source = read(root.join("crates/execution/cli/src/mev_trader.rs"));
    let cli_ast = parse_production(&cli_source);
    let t4b_module = cli_ast
        .items
        .iter()
        .find_map(|item| match item {
            Item::Mod(module) if module.ident == "t4b_shadow" => Some(module),
            _ => None,
        })
        .expect("inline T4b module");
    let mut selected_ast = AstSeal::default();
    selected_ast.visit_item_mod(t4b_module);
    assert!(
        selected_ast.violations.is_empty(),
        "T4b observer connects to forbidden capability: {:?}",
        selected_ast.violations
    );

    let t4d_module = cli_ast
        .items
        .iter()
        .find_map(|item| match item {
            Item::Mod(module) if module.ident == "t4d_shadow" => Some(module),
            _ => None,
        })
        .expect("inline T4d module");
    assert_eq!(t4d_module.attrs.len(), 1, "T4d module has ambiguous attributes");
    assert!(has_cfg_feature(&t4d_module.attrs, "t4d-shadow"));
    assert!(matches!(t4d_module.vis, Visibility::Inherited));
    assert!(t4d_module.content.is_some(), "T4d module must stay inline and AST-sealed");
    let module_imports = t4d_module
        .content
        .as_ref()
        .expect("inline T4d module")
        .1
        .iter()
        .filter_map(|item| match item {
            Item::Use(item) => Some(item),
            _ => None,
        })
        .flat_map(use_leaves)
        .map(|leaf| leaf.source.join("::"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        module_imports,
        BTreeSet::from([
            "std::sync::Arc".to_owned(),
            "std::sync::atomic::AtomicU64".to_owned(),
            "std::sync::atomic::Ordering".to_owned(),
            "super::BlockReaderIdExt".to_owned(),
            "super::BridgeError".to_owned(),
            "super::CandidateAssemblyView".to_owned(),
            "super::CandidateTxShapeObserver".to_owned(),
            "super::CliTraderSnapshotPort".to_owned(),
            "super::Debug".to_owned(),
            "super::Header".to_owned(),
            "super::HeaderProvider".to_owned(),
            "super::InstalledSubmissionBridge".to_owned(),
            "super::SealedUnsignedCandidate".to_owned(),
            "super::ShadowLatestSlot".to_owned(),
            "super::ShadowSubmit".to_owned(),
            "super::StateProviderFactory".to_owned(),
            "super::T4bOutcome".to_owned(),
            "super::T4bOutcomeCounters".to_owned(),
            "super::T4eCandidateHandoff".to_owned(),
            "super::T4eHandoffError".to_owned(),
            "super::TxAuthorityError".to_owned(),
            "super::t4b_shadow".to_owned(),
        ])
    );
    let t4d_imports = cli_ast
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Use(item) if has_cfg_feature(&item.attrs, "t4d-shadow") => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(t4d_imports.len(), 1, "T4d capability imports must be one exact group");
    assert_eq!(t4d_imports[0].attrs.len(), 1, "T4d imports have ambiguous attributes");
    let import_leaves = use_leaves(t4d_imports[0]);
    assert!(
        import_leaves.iter().all(|leaf| {
            !leaf.glob
                && !leaf.renamed
                && leaf.source.first().map(String::as_str) == Some("mev_trader_submit")
                && leaf.source.len() == 2
        }),
        "T4d capability imports contain a glob, alias, or redirect"
    );
    assert_eq!(
        import_leaves.iter().map(|leaf| leaf.public_name.as_str()).collect::<BTreeSet<_>>(),
        BTreeSet::from(["BridgeError", "InstalledSubmissionBridge", "SealedUnsignedCandidate",])
    );
    let mut t4d_ast = AstSeal::unsigned_handoff();
    t4d_ast.visit_item_mod(t4d_module);
    assert!(
        t4d_ast.violations.is_empty(),
        "T4d observer connects to forbidden capability: {:?}",
        t4d_ast.violations
    );
    assert_eq!(t4d_ast.base_mainnet_calls, 1);
    assert_eq!(t4d_ast.assemble_sealed_calls, 1);
    assert_eq!(t4d_ast.observer_factories, 1);
    assert_eq!(t4d_ast.slot_constructions, 1);
    assert_eq!(t4d_ast.spawn_calls, 0);
    assert_eq!(t4d_ast.thread_calls, 0);
    assert_eq!(t4d_ast.subscription_calls, 0);
    assert_eq!(t4d_ast.item_macros, 0);

    let mut cli_inventory = AstSeal::default();
    cli_inventory.visit_file(&cli_ast);
    assert_eq!(cli_inventory.t4d_observer_calls, 1);
    assert_eq!(cli_inventory.assemble_sealed_calls, 1);
    assert_eq!(cli_inventory.slot_constructions, 2);
    assert_eq!(cli_inventory.spawn_calls, 7);
    assert_eq!(cli_inventory.thread_calls, 0);
    assert_eq!(cli_inventory.subscription_calls, 1);

    let provenance = submit_feature_provenance(&root, "t4b-shadow");
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
}

#[test]
fn t4d_default_and_selected_closures_have_zero_signer_and_egress_edges() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("../../..");
    let submit_manifest = read(crate_dir.join("Cargo.toml"));
    let submit_lib = read(crate_dir.join("src/lib.rs"));
    let assembler_source = read(crate_dir.join("src/assembler.rs"));
    let calldata_source = read(crate_dir.join("src/calldata.rs"));
    let fee_source = read(crate_dir.join("src/fee.rs"));
    let economics_source = read(crate_dir.join("src/economics.rs"));
    let authority_source = read(crate_dir.join("src/tx_authority.rs"));
    let bridge_source = read(crate_dir.join("src/tx_authority/bridge.rs"));
    let cli_manifest = read(root.join("crates/execution/cli/Cargo.toml"));
    let cli_source = read(root.join("crates/execution/cli/src/mev_trader.rs"));
    let trader_manifest = read(root.join("crates/execution/mev-trader/Cargo.toml"));
    let trader_runtime = read(root.join("crates/execution/mev-trader/src/runtime.rs"));
    let trader_port = read(root.join("crates/execution/mev-trader/src/port.rs"));
    let node_manifest = read(root.join("bin/node/Cargo.toml"));

    let production_after_tests =
        parse_production("#[cfg(test)] mod tests {}\nstruct ProductionAfterTests;");
    assert!(
        production_after_tests
            .items
            .iter()
            .any(|item| matches!(item, Item::Struct(item) if item.ident == "ProductionAfterTests")),
        "production items after test modules must remain sealed"
    );

    let test_only_call = parse_production(
        "struct TestOnly;\n\
         #[cfg(test)] impl TestOnly {\n\
             fn call() { AtomicCalldataEncoder::encode_validated(); }\n\
         }\n\
         struct ProductionAfterTestImpl;",
    );
    let mut test_only_seal = AstSeal::default();
    test_only_seal.visit_file(&test_only_call);
    assert_eq!(
        test_only_seal.encode_validated_calls, 0,
        "test-only impl calls must not satisfy production exact counts"
    );

    let exact: Attribute = syn::parse_quote!(#[cfg(feature = "t4d-shadow")]);
    let negative: Attribute = syn::parse_quote!(#[cfg(not(feature = "t4d-shadow"))]);
    let widened: Attribute =
        syn::parse_quote!(#[cfg(any(feature = "t4d-shadow", feature = "arm"))]);
    assert!(has_cfg_feature(&[exact], "t4d-shadow"));
    assert!(!has_cfg_feature(&[negative], "t4d-shadow"));
    assert!(!has_cfg_feature(&[widened], "t4d-shadow"));

    let signing_fixture = syn::parse_file(
        "type A = PrimitiveSignature;\n\
         type B = Signed;\n\
         type C = SigningKey;\n\
         type D = TxEnvelope;\n\
         type E = AuthorizedSignedSubmission;\n\
         type F = HotWalletKey;\n\
         fn escape() { sign_unsigned(); raw_signed(); raw_tx(); }",
    )
    .expect("signing fixture parses");
    let mut signing_seal = AstSeal::unsigned_handoff();
    signing_seal.visit_file(&signing_fixture);
    for forbidden in [
        "PrimitiveSignature",
        "Signed",
        "SigningKey",
        "TxEnvelope",
        "AuthorizedSignedSubmission",
        "HotWalletKey",
        "sign_unsigned",
        "raw_signed",
        "raw_tx",
    ] {
        assert!(
            signing_seal.violations.contains(forbidden),
            "strict handoff seal missed {forbidden}"
        );
    }

    let synthetic_consumer = syn::parse_file(
        "#[cfg(feature = \"t4d-shadow\")]\n\
         use mev_trader_submit::InstalledSubmissionBridge;\n\
         fn consume(value: InstalledSubmissionBridge) { drop(value); }",
    )
    .expect("T4d consumer fixture parses");
    let (imports, uses, slot_uses) = authoritative_uses(&synthetic_consumer, true);
    assert!(imports.violations.is_empty());
    assert_eq!((imports.imports, uses, slot_uses), (1, 1, 0));

    let sealed_only_consumer = syn::parse_file(
        "#[cfg(feature = \"t4d-shadow\")]\n\
         use mev_trader_submit::SealedUnsignedCandidate;\n\
         fn consume(value: SealedUnsignedCandidate) { drop(value); }",
    )
    .expect("sealed-only T4d consumer fixture parses");
    let (imports, uses, slot_uses) = authoritative_uses(&sealed_only_consumer, true);
    assert!(imports.violations.is_empty());
    assert_eq!((imports.imports, uses, slot_uses), (1, 1, 0));

    let direct_alias_consumer = syn::parse_file(
        "use mev_trader_submit as submit;\n\
         fn consume(value: submit::InstalledSubmissionBridge) { drop(value); }",
    )
    .expect("direct-alias T4d consumer fixture parses");
    let (imports, uses, slot_uses) = authoritative_uses(&direct_alias_consumer, true);
    assert_eq!((imports.imports, uses, slot_uses), (0, 1, 0));
    assert!(
        imports.violations.contains("non-canonical crate alias submit"),
        "direct crate aliases must fail closed"
    );

    let glob_consumer = syn::parse_file(
        "use mev_trader_submit::*;\n\
         fn consume(value: SealedUnsignedCandidate) { drop(value); }",
    )
    .expect("glob T4d consumer fixture parses");
    let (imports, _, _) = authoritative_uses(&glob_consumer, true);
    assert!(
        imports.violations.contains("non-canonical crate alias *"),
        "glob crate imports must fail closed"
    );

    let aliased_consumer = syn::parse_file(
        "#[cfg(feature = \"t4d-shadow\")]\n\
         use mev_trader_submit::{self as submit};\n\
         fn consume(value: submit::InstalledSubmissionBridge) { drop(value); }",
    )
    .expect("aliased T4d consumer fixture parses");
    let (imports, uses, slot_uses) = authoritative_uses(&aliased_consumer, true);
    assert_eq!((imports.imports, uses, slot_uses), (0, 1, 0));
    assert!(
        imports.violations.contains("non-canonical crate alias submit"),
        "grouped crate aliases must fail closed"
    );

    let extern_consumer = syn::parse_file(
        "extern crate mev_trader_submit as submit;\n\
         fn consume(value: submit::InstalledSubmissionBridge) { drop(value); }",
    )
    .expect("extern-crate T4d consumer fixture parses");
    let (imports, uses, slot_uses) = authoritative_uses(&extern_consumer, true);
    assert_eq!((imports.imports, uses, slot_uses), (0, 1, 0));
    assert!(
        imports.violations.contains("non-canonical extern crate alias submit"),
        "extern crate aliases must fail closed"
    );

    let macro_consumer = syn::parse_file(
        "macro_rules! escape { () => { let _: mev_trader_submit::InstalledSubmissionBridge; }; }",
    )
    .expect("macro consumer fixture parses");
    let (imports, _, _) = authoritative_uses(&macro_consumer, true);
    assert!(imports.violations.contains("macro authority redirect"));

    let path_consumer =
        syn::parse_file("#[path = \"outside.rs\"] mod hidden;").expect("path fixture parses");
    let (imports, _, _) = authoritative_uses(&path_consumer, true);
    assert!(imports.violations.contains("path module authority redirect"));

    for source in [
        &submit_lib,
        &assembler_source,
        &calldata_source,
        &fee_source,
        &economics_source,
        &authority_source,
    ] {
        syn::parse_file(source).expect("submit source parses as Rust AST");
    }
    assert_eq!(
        tx_authority_modules(&submit_lib),
        BTreeSet::from(["calldata".to_owned(), "fee".to_owned(), "tx_authority".to_owned(),])
    );
    let authority_ast = parse_production(&authority_source);
    assert_private_fields(
        &authority_ast,
        &["ValidatedAbiHop", "ValidatedAtomicCall", "ValidatedUnsignedAtomicTx"],
    );

    let mut victim_envelope = VictimEnvelopeSeal::default();
    victim_envelope.visit_file(&authority_ast);
    assert!(victim_envelope.violations.is_empty());
    assert_eq!(
        (
            authority_source.matches("TxEnvelope").count(),
            victim_envelope.imports,
            victim_envelope.decode_calls,
            victim_envelope.eip1559_patterns,
        ),
        (3, 1, 1, 1),
        "TxEnvelope must remain receive-only victim decoding"
    );

    assert!(!submit_manifest.lines().any(|line| line.trim_start().starts_with("default =")));
    let submit_tier = feature_body(&submit_manifest, "tx-authority");
    for forbidden in ["phase-b", "arm", "k256", "rand", "reqwest", "zeroize"] {
        assert!(!submit_tier.contains(forbidden), "tx-authority enables {forbidden}");
    }
    assert!(submit_tier.contains("base-mev-trader/t4b-shadow"));
    let bridge_tier = feature_body(&submit_manifest, "t4d-bridge");
    assert_eq!(
        bridge_tier
            .split(',')
            .map(|feature| feature.trim().trim_matches('"'))
            .filter(|feature| !feature.is_empty())
            .collect::<Vec<_>>(),
        ["tx-authority"]
    );
    for forbidden in [
        "phase-b",
        "arm",
        "arm-live-egress",
        "arm-provisioning",
        "k256",
        "rand",
        "reqwest",
        "zeroize",
    ] {
        assert!(!bridge_tier.contains(forbidden), "t4d-bridge enables {forbidden}");
    }
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
    let cli_t4d_tier = feature_body(&cli_manifest, "t4d-shadow");
    assert!(cli_t4d_tier.contains("mev-trader-submit/t4d-bridge"));
    assert!(cli_t4d_tier.contains("t4b-shadow"));
    for forbidden in [
        "phase-b",
        "arm",
        "arm-live-egress",
        "arm-provisioning",
        "k256",
        "rand",
        "reqwest",
        "signer",
        "zeroize",
    ] {
        assert!(!cli_t4d_tier.contains(forbidden), "CLI T4d enables {forbidden}");
    }
    assert!(node_manifest.contains("t4d-shadow = [ \"base-execution-cli/t4d-shadow\" ]"));
    let bridge_file = syn::parse_file(&bridge_source).expect("T4d bridge parses as Rust AST");
    let bridge_imports = bridge_file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Use(item) => Some(item),
            _ => None,
        })
        .flat_map(use_leaves)
        .map(|leaf| leaf.source.join("::"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        bridge_imports,
        BTreeSet::from([
            "std::fmt::Debug".to_owned(),
            "std::fmt::self".to_owned(),
            "std::sync::Arc".to_owned(),
            "std::time::Instant".to_owned(),
            "alloy_primitives::Address".to_owned(),
            "alloy_primitives::B256".to_owned(),
            "base_mev_trader::CampaignId".to_owned(),
            "base_mev_trader::CancellationProbe".to_owned(),
            "base_mev_trader::CandidateAssemblyView".to_owned(),
            "base_mev_trader::ExactProtocol".to_owned(),
            "base_mev_trader::MeasurementContext".to_owned(),
            "base_mev_trader::GlobalState".to_owned(),
            "base_mev_trader::TaskState".to_owned(),
            "crate::CheckedCandidate".to_owned(),
            "crate::CodeHashProvider".to_owned(),
            "super::DeployedContractIdentity".to_owned(),
            "super::InstalledExecutionIdentity".to_owned(),
            "super::TxAuthorityAssembler".to_owned(),
            "super::TxAuthorityError".to_owned(),
            "super::TxAuthorityNodeView".to_owned(),
            "super::ValidatedUnsignedAtomicTx".to_owned(),
        ])
    );
    let mut bridge_ast = AstSeal::unsigned_handoff();
    bridge_ast.visit_file(&bridge_file);
    assert!(
        bridge_ast.violations.is_empty(),
        "T4d bridge contains signer or egress capability: {:?}",
        bridge_ast.violations
    );
    assert_eq!(bridge_ast.spawn_calls, 0);
    assert_eq!(bridge_ast.thread_calls, 0);
    assert_eq!(bridge_ast.subscription_calls, 0);
    assert_eq!(bridge_ast.item_macros, 0);
    assert_eq!(bridge_ast.production_modules, 0);
    let nested_bridge_escape = syn::parse_file(
        "mod leak {
            impl crate::SealedUnsignedCandidate {
                pub fn payload(&self) -> &alloy_consensus::TxEip1559 {
                    &self.detail.unsigned_tx
                }
            }
        }",
    )
    .expect("nested bridge escape fixture parses");
    let mut nested_bridge_seal = AstSeal::unsigned_handoff();
    nested_bridge_seal.visit_file(&nested_bridge_escape);
    assert_eq!(nested_bridge_seal.production_modules, 1);

    let mut selected_seal = AstSeal::unsigned_handoff();
    for source in [&calldata_source, &fee_source] {
        selected_seal.visit_file(&parse_production(source));
    }
    assert!(
        selected_seal.violations.is_empty(),
        "forbidden selected helper AST: {:?}",
        selected_seal.violations
    );
    let mut authority_seal = AstSeal::unsigned_authority();
    authority_seal.visit_file(&authority_ast);
    assert!(
        authority_seal.violations.is_empty(),
        "forbidden selected authority AST: {:?}",
        authority_seal.violations
    );
    assert_eq!(selected_seal.spawn_calls + authority_seal.spawn_calls, 0);
    assert_eq!(selected_seal.subscription_calls + authority_seal.subscription_calls, 0);
    assert_eq!(calldata_source.matches("executeBlinkOfaAtomicCall").count(), 1);
    assert_eq!(assembler_source.matches("AtomicCalldataEncoder::encode_legacy").count(), 1);
    assert_eq!(authority_seal.encode_validated_calls, 1);

    let default_tree = cargo_tree(&root, None);
    assert!(!default_tree.contains("mev-trader-submit v"));

    let t4b_tree = cargo_tree(&root, Some("t4b-shadow"));
    assert!(t4b_tree.contains("mev-trader-submit v"));
    let t4b_provenance = submit_feature_provenance(&root, "t4b-shadow");
    let t4b_features = t4b_provenance
        .lines()
        .filter_map(|line| {
            line.strip_prefix("mev-trader-submit feature \"")?.split_once('"').map(|(name, _)| name)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(t4b_features, BTreeSet::from(["base-mev-trader", "default", "tx-authority"]));

    let selected_tree = cargo_tree(&root, Some("t4d-shadow"));
    assert!(selected_tree.contains("mev-trader-submit v"));
    let provenance = submit_feature_provenance(&root, "t4d-shadow");
    let enabled_submit_features = provenance
        .lines()
        .filter_map(|line| {
            line.strip_prefix("mev-trader-submit feature \"")?.split_once('"').map(|(name, _)| name)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        enabled_submit_features,
        BTreeSet::from(["base-mev-trader", "default", "t4d-bridge", "tx-authority"])
    );
    for forbidden in ["phase-b", "arm", "arm-live-egress", "arm-provisioning"] {
        assert!(!provenance.contains(&format!("mev-trader-submit feature \"{forbidden}\"")));
    }
    let selected_packages = package_set(&selected_tree);
    let t4b_packages = package_set(&t4b_tree);
    assert_eq!(selected_packages, t4b_packages, "T4d selected closure added a package beyond T4b");
    assert_eq!(
        feature_set(&selected_tree),
        feature_set(&t4b_tree),
        "T4d selected closure enabled a dependency feature beyond T4b"
    );
    let bridge_submit_closure = package_closure(&root, "mev-trader-submit", Some("t4d-bridge"));
    let submit_closure = package_closure(&root, "mev-trader-submit", Some("tx-authority"));
    assert_eq!(
        package_set(&bridge_submit_closure),
        package_set(&submit_closure),
        "T4d bridge added a package beyond the reviewed unsigned-authority baseline"
    );
    let trader_closure = package_closure(&root, "base-mev-trader", Some("t4b-shadow"));
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
    let mut cli_seal = AstSeal::unsigned_handoff();
    cli_seal.visit_file(&cli_ast);
    assert_eq!(cli_seal.violations, BTreeSet::from(["OpenOptions".to_owned(), "send".to_owned()]));
    assert_eq!(cli_seal.node_view_impls, 1);
    assert_eq!(cli_seal.observer_impls, 2);
    assert_eq!(cli_seal.pending_view_impls, 1);
    assert_eq!(cli_seal.base_mainnet_calls, 2);
    assert_eq!(cli_seal.assemble_sealed_calls, 1);
    assert_eq!(cli_seal.observe_candidate_calls, 0);
    assert_eq!(cli_seal.subscription_calls, 1);
    assert_eq!(cli_seal.spawn_calls, 7);
    assert_eq!(cli_seal.thread_calls, 0);
    assert_eq!(cli_seal.pending_adapter_constructions, 1);
    assert_eq!(cli_seal.node_view_constructions, 1);
    assert_eq!(cli_seal.authority_constructions, 1);
    assert_eq!(cli_seal.observer_factories, 2);
    assert_eq!(cli_seal.observer_install_calls, 4);

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
