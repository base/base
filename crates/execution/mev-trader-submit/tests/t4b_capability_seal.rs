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
    Attribute, Expr, ExprCall, ExprMethodCall, ExprPath, ExprStruct, File, Ident, ImplItem,
    ImplItemFn, Item, ItemFn, ItemImpl, ItemStruct, Lit, Macro, Member, Meta, Pat, Path, Stmt,
    Token, Type, TypePath, UseTree, Visibility, parse::Parser, punctuated::Punctuated,
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
        for forbidden in [
            "reqwest",
            "txpool",
            "signer",
            "RawEgress",
            "ProdBackend",
            "OpenOptions",
            "AuthorizedCandidate",
        ] {
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
            "AuthorizedCandidate",
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
    match meta {
        Meta::NameValue(value) if value.path.is_ident("feature") => {
            matches!(
                &value.value,
                Expr::Lit(literal)
                    if matches!(&literal.lit, Lit::Str(value) if value.value() == feature)
            )
        }
        Meta::List(list) => {
            nested_meta(list).iter().any(|nested| meta_has_feature(nested, feature))
        }
        Meta::Path(_) | Meta::NameValue(_) => false,
    }
}

fn has_cfg_feature(attrs: &[Attribute], feature: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("cfg") && meta_has_feature(&attr.meta, feature))
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let Meta::List(cfg) = &attr.meta else {
            return false;
        };
        if !cfg.path.is_ident("cfg") {
            return false;
        }
        let nested = nested_meta(cfg);
        nested.len() == 1
            && matches!(nested.first(), Some(Meta::Path(path)) if path.is_ident("test"))
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

    fn root_exports(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut exports = BTreeSet::new();
        let mut violations = BTreeSet::new();
        for item in &self.submit_lib.items {
            let Item::Use(item) = item else {
                continue;
            };
            if !matches!(item.vis, Visibility::Public(_)) {
                continue;
            }
            let selected = has_cfg_feature(&item.attrs, "tx-authority");
            for leaf in use_leaves(item) {
                let mentions_authority =
                    leaf.source.iter().any(|segment| segment == "tx_authority");
                if !selected && !mentions_authority {
                    continue;
                }
                if !selected
                    || leaf.source.first().map(String::as_str) != Some("tx_authority")
                    || leaf.source.len() != 2
                    || leaf.renamed
                    || leaf.glob
                {
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
        assert!(
            has_cfg_feature(&modules[0].attrs, "tx-authority"),
            "module `{name}` escaped the tx-authority feature gate"
        );
        assert!(
            matches!(modules[0].vis, Visibility::Inherited),
            "tx-authority module `{name}` must remain private"
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
            let links_submit = package["dependencies"]
                .as_array()
                .expect("dependencies")
                .iter()
                .any(|dependency| dependency["name"] == "mev-trader-submit");
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
            if self.external && first == Some("mev_trader_submit") && leaf.source.len() == 1 {
                self.inventory.crate_aliases.insert(leaf.public_name.clone());
                continue;
            }
            if last != Some("ValidatedUnsignedAtomicTx") {
                continue;
            }
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
            }
        }
    }
}

impl<'ast> Visit<'ast> for ImportedAuthorityCollector {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.inspect_use(item);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
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
        if last != "ValidatedUnsignedAtomicTx" {
            return false;
        }
        if segments.len() == 1 && self.aliases.contains(last) {
            return true;
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

    let (exports, export_violations) = inventory.root_exports();
    assert!(export_violations.is_empty(), "invalid tx-authority exports: {export_violations:?}");
    assert_eq!(
        exports,
        BTreeSet::from([
            "DeployedContractIdentity".to_owned(),
            "InstalledExecutionIdentity".to_owned(),
            "ProtocolAdapterMapping".to_owned(),
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
fn t4c_unsigned_authority_has_no_arm_signer_or_egress_consumer() {
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
    assert_eq!((*imports, *uses, *slot_uses), (1, 1, 1));

    let submit_src = crate_dir.join("src");
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
        assert_eq!(
            (imports.imports, imports.public_reexports, uses, slot_uses),
            (0, 0, 0, 0),
            "{} connects T4b authority to another submit tier",
            path.display()
        );
    }

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
