//! Mode-B red-line capability seal for the B3-arm tier (§1.3 b1–b8). Author ≠
//! reviewer: these are machine-checks a reviewer re-runs. Compiles only under
//! `--features arm` (the acceptance lane); absent otherwise.
#![cfg(feature = "arm")]

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use syn::{ext::IdentExt, visit::Visit};

/// The exact production files under `src/arm/`. A NEW arm file must be added here
/// (and re-reviewed) before it can ship (b7, fail-closed).
const ARM_FILES: [&str; 17] = [
    "claim.rs",
    "custody.rs",
    "fail_sink.rs",
    "mod.rs",
    "proofs.rs",
    "producer.rs",
    "provisioning_tool.rs",
    "providers.rs",
    "production_bundle.rs",
    "production_handoff.rs",
    "simulation_entrypoint.rs",
    "simulation_store.rs",
    "settled_loss.rs",
    "request.rs",
    "suppression.rs",
    "transport.rs",
    "witness.rs",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn arm_dir() -> PathBuf {
    manifest_dir().join("src").join("arm")
}

/// Strip `//` line and `/* */` block comments, keeping string literals, so scans
/// see real code but never trip on doc comments that legitimately name forbidden
/// things.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                out.push('"');
                i += 1;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    match bytes[i] {
                        b'\\' if i + 1 < bytes.len() => {
                            out.push(bytes[i + 1] as char);
                            i += 2;
                        }
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            other => {
                out.push(other as char);
                i += 1;
            }
        }
    }
    out
}

/// Return the raw production prefix before the structurally validated terminal
/// test-module tail. The exact marker must occur once in the raw source: a copy
/// hidden in a string or comment therefore fails closed rather than moving the cut.
fn production_prefix<'a>(source: &'a str, file: &str) -> Result<&'a str, String> {
    let parsed = syn::parse_file(source).map_err(|error| format!("parse {file}: {error}"))?;
    let expected: &[(&str, bool, &str)] = if file == "mod.rs" {
        &[
            ("testkit", true, "#[cfg(test)]\npub(crate) mod testkit"),
            ("tests", false, "#[cfg(test)]\nmod tests"),
        ]
    } else {
        &[("tests", false, "#[cfg(test)]\nmod tests")]
    };
    let Some(first_tail) = parsed.items.iter().position(|item| {
        matches!(item, syn::Item::Mod(module) if has_cfg_test(&module.attrs)
            && expected.first().is_some_and(|entry| ident_name(&module.ident) == entry.0))
    }) else {
        for (_, _, marker) in expected {
            if source.contains(marker) {
                return Err(format!("{file} contains a test-tail marker without that module"));
            }
        }
        return Ok(source);
    };

    let tail = &parsed.items[first_tail..];
    if tail.len() != expected.len() {
        return Err(format!("{file} has production or an unexpected module in its test tail"));
    }
    for (item, (name, crate_visible, _)) in tail.iter().zip(expected) {
        let syn::Item::Mod(module) = item else {
            return Err(format!("{file} has a non-module item after its test tail starts"));
        };
        let exact_visibility = if *crate_visible {
            matches!(
                &module.vis,
                syn::Visibility::Restricted(restricted)
                    if restricted.in_token.is_none() && restricted.path.is_ident("crate")
            )
        } else {
            matches!(module.vis, syn::Visibility::Inherited)
        };
        if ident_name(&module.ident) != *name
            || !exact_visibility
            || module.content.is_none()
            || module.attrs.len() != 1
            || !has_exact_cfg_test(&module.attrs[0])
        {
            return Err(format!("{file} has a non-canonical terminal test module `{name}`"));
        }
    }

    let mut cut = None;
    for (name, _, marker) in expected {
        let mut occurrences = source.match_indices(marker);
        let Some((index, _)) = occurrences.next() else {
            return Err(format!(
                "{file} structurally has `{name}` but its exact raw marker is absent"
            ));
        };
        if occurrences.next().is_some() {
            return Err(format!(
                "{file} contains multiple raw copies of `{name}` test-tail marker"
            ));
        }
        cut.get_or_insert(index);
    }
    Ok(&source[..cut.expect("non-empty expected test tail")])
}

fn test_modules_are_terminal(source: &str) -> bool {
    production_prefix(source, "fixture.rs").is_ok()
}

/// Production code of one arm file: the structurally validated test tail is
/// removed from raw source before comments are stripped.
fn arm_production(file: &str) -> String {
    let raw = std::fs::read_to_string(arm_dir().join(file)).expect("arm source");
    strip_comments(production_prefix(&raw, file).unwrap_or_else(|error| panic!("{error}")))
}

fn arm_production_mod() -> String {
    arm_production("mod.rs")
}

fn all_arm_production() -> Vec<(&'static str, String)> {
    ARM_FILES
        .iter()
        .map(|file| {
            let body = if *file == "mod.rs" { arm_production_mod() } else { arm_production(file) };
            (*file, body)
        })
        .collect()
}

// -- b7: the ARM_FILES set is exactly what is on disk --------------------------

#[test]
fn arm_source_is_exactly_the_declared_set() {
    let actual: BTreeSet<String> = std::fs::read_dir(arm_dir())
        .expect("arm dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    let expected: BTreeSet<String> = ARM_FILES.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(actual, expected, "unscanned arm source file present in src/arm/");
}

// -- b9: the crate's public `arm` surface is EXACTLY the curated allowlist ------

/// The complete root API. T4e adds only the reviewed checked authorities, exact-one
/// production handoff, bounded worker/status types, and simulation-only runtime surface.
const PUBLIC_API_ALLOWLIST: [&str; 100] = [
    "AdmittedCandidate",
    "AuthorizationGateError",
    "BlockNumHash",
    "BoundedSubmissionIdV1",
    "BoundedUnresolvedSummaryV1",
    "CanonicalDeploymentPairV1",
    "CanonicalG7PairV1",
    "CanonicalLivePairV1",
    "CanonicalMismatchClass",
    "CheckedCandidate",
    "CodeHashProvider",
    "CommittedStateAuthority",
    "FinalizedChainAuthority",
    "FinalizedChainError",
    "FrozenP2PopulationManifestV1",
    "NodeLocalSettledLossAuthority",
    "PopulationClosureFieldsV1",
    "PreparedSettledLossAuthority",
    "ProdBackend",
    "ProducerConformance",
    "ProducerError",
    "ProductionArmFailure",
    "ProductionArmRuntimeOpenFailure",
    "ProductionBridgeFailure",
    "ProductionBundleInputs",
    "ProductionCampaignBundleFailure",
    "ProductionCandidateError",
    "ProductionCandidateReceiver",
    "ProductionClaimError",
    "ProductionClaimFailure",
    "ProductionClaimResult",
    "ProductionCustodyFailure",
    "ProductionDeploymentFailure",
    "ProductionDrawdownSource",
    "ProductionHandoffClosed",
    "ProductionHandoffShared",
    "ProductionHandoffInstaller",
    "ProductionHandoffState",
    "ProductionInstallBundle",
    "ProductionInstallDisposition",
    "ProductionInstallInputs",
    "ProductionLatchOutcome",
    "ProductionPersistenceFailure",
    "ProductionProofBundle",
    "ProductionProviderFailure",
    "ProductionSignFailure",
    "ProductionSignedField",
    "ProductionSigningError",
    "ProductionSimulationHandoff",
    "ProductionSimulationHandoffStatus",
    "ProductionSimulationInstallError",
    "ProductionSimulationWorkerOwner",
    "ProductionSpawnDisposition",
    "ProductionStartup",
    "ProductionStoreOpenFailure",
    "ProductionWorkerBootstrap",
    "ProductionWorkerError",
    "ParsedFrozenExportV1",
    "ProviderError",
    "ProjectionClosureFieldsV1",
    "ProvisioningToolError",
    "PublicationIoClass",
    "PublishedPopulationManifestV1",
    "RuntimeBackend",
    "SETTLED_LOSS_ANCHOR_PATH",
    "SETTLED_LOSS_PROJECTION_PATH",
    "SettledLossLoad",
    "SettledLossReader",
    "SettledLossUnavailableReason",
    "SignedInstallBundleV1",
    "SignedPopulationManifestV1",
    "SignedProjectionV1",
    "SimBackend",
    "SimulationCorrelationEnvelopeV1",
    "SimulationCorrelationKey",
    "SimulationEntrypointStatus",
    "SimulationEntrypointUnavailable",
    "SimulationLedgerClosure",
    "SimulationLedgerEpoch",
    "SimulationLedgerInvalid",
    "SimulationReservation",
    "SimulationStoreOperation",
    "SourceLedgerRowV1",
    "SourceSubmissionManifestEntryV1",
    "SuppressionRollbackError",
    "TerminalSettlementProjectionV1",
    "T4eProvisioningTool",
    "TerminalKindV1",
    "TerminalSettlementEntryV1",
    "UnsignedInstallBundleV1",
    "UnsignedPopulationManifestV1",
    "UnsignedProjectionV1",
    "UnresolvedReasonV1",
    "VerifiedProductionProofs",
    "WorkerStartup",
    "WorkerStartupFailure",
    "SimulationWorker",
    "production_custody_preflight",
    "provision_suppression_anchor",
    "try_claim_detailed",
];

static MODULE_FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct ModuleFixture {
    root: PathBuf,
}

impl ModuleFixture {
    fn new(tag: &str) -> Self {
        let unique = MODULE_FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("arm-capability-seal-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create module fixture");
        Self { root }
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture module directory");
        }
        std::fs::write(path, source).expect("write module fixture");
    }

    fn lib(&self) -> PathBuf {
        self.root.join("lib.rs")
    }
}

impl Drop for ModuleFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).expect("remove module fixture");
    }
}

// -- AST-based seal primitives (syn) — robust against the evasion vectors -------

/// Parse Rust source (syn keeps `#[cfg(test)]` items with their attributes and
/// does not evaluate `cfg`, so seams stay visible for inspection).
fn parse(src: &str) -> syn::File {
    syn::parse_file(src).expect("parse rust source")
}

fn module_attrs_are_exact(module: &syn::ItemMod, expected_cfg: Option<&str>) -> bool {
    match expected_cfg {
        None => module.attrs.is_empty(),
        Some(tokens) => {
            module.attrs.len() == 1
                && matches!(
                    &module.attrs[0].meta,
                    syn::Meta::List(list)
                        if list.path.is_ident("cfg") && list.tokens.to_string() == tokens
                )
        }
    }
}

fn exact_module_declaration(
    src: &str,
    name: &str,
    expected_cfg: Option<&str>,
) -> Option<syn::ItemMod> {
    let parsed = parse(src);
    let mut modules = parsed.items.into_iter().filter_map(|item| {
        let syn::Item::Mod(module) = item else { return None };
        (ident_name(&module.ident) == name).then_some(module)
    });
    let module = modules.next()?;
    if modules.next().is_some()
        || !matches!(module.vis, syn::Visibility::Inherited)
        || module.content.is_some()
        || module.semi.is_none()
        || !module_attrs_are_exact(&module, expected_cfg)
    {
        return None;
    }
    Some(module)
}

fn mod_is_private(src: &str, name: &str) -> bool {
    let expected_cfg = matches!(name, "production_bundle" | "production_handoff")
        .then_some("feature = \"t4e-handoff\"");
    exact_module_declaration(src, name, expected_cfg).is_some()
}

fn exact_module_resolves_to(
    src: &str,
    name: &str,
    expected_cfg: Option<&str>,
    current_dir: &Path,
    graph_root: &Path,
    expected_file: &Path,
) -> Result<(), String> {
    let module = exact_module_declaration(src, name, expected_cfg).ok_or_else(|| {
        format!("module `{name}` is not an exact private out-of-line declaration")
    })?;
    let mut visited = BTreeSet::new();
    load_local_module(&module, Some(current_dir), Some(graph_root), &mut visited)?;
    let resolved = visited
        .into_iter()
        .next()
        .ok_or_else(|| format!("module `{name}` did not resolve to a file"))?;
    let expected = std::fs::canonicalize(expected_file)
        .map_err(|error| format!("canonicalize {}: {error}", expected_file.display()))?;
    if resolved != expected {
        return Err(format!(
            "module `{name}` resolves to {}, expected reviewed {}",
            resolved.display(),
            expected.display()
        ));
    }
    Ok(())
}

fn reviewed_arm_module_tree(root: &Path) -> Result<(), String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("canonicalize {}: {error}", root.display()))?;
    let src_dir = root.parent().ok_or_else(|| "root source has no parent".to_string())?;
    let root_source = std::fs::read_to_string(&root)
        .map_err(|error| format!("read {}: {error}", root.display()))?;
    let arm_mod = src_dir.join("arm").join("mod.rs");
    exact_module_resolves_to(
        &root_source,
        "arm",
        Some("feature = \"arm\""),
        src_dir,
        src_dir,
        &arm_mod,
    )?;

    let arm_dir = arm_mod.parent().ok_or_else(|| "arm mod has no parent".to_string())?;
    let arm_source = std::fs::read_to_string(&arm_mod)
        .map_err(|error| format!("read {}: {error}", arm_mod.display()))?;
    let expected_children = [
        "claim",
        "custody",
        "fail_sink",
        "proofs",
        "providers",
        "producer",
        "provisioning_tool",
        "production_bundle",
        "production_handoff",
        "simulation_entrypoint",
        "simulation_store",
        "settled_loss",
        "request",
        "suppression",
        "transport",
        "witness",
    ];
    let parsed_arm = parse(&arm_source);
    let actual_children: Vec<String> = parsed_arm
        .items
        .iter()
        .filter_map(|item| {
            let syn::Item::Mod(module) = item else { return None };
            (!has_cfg_test(&module.attrs)).then(|| ident_name(&module.ident))
        })
        .collect();
    let actual_set: BTreeSet<&str> = actual_children.iter().map(String::as_str).collect();
    let expected_set: BTreeSet<&str> = expected_children.into_iter().collect();
    if actual_children.len() != expected_children.len() || actual_set != expected_set {
        return Err(format!(
            "arm production child module set differs from reviewed set: {actual_children:?}"
        ));
    }
    for child in expected_children {
        let expected_cfg = matches!(child, "production_bundle" | "production_handoff")
            .then_some("feature = \"t4e-handoff\"");
        exact_module_resolves_to(
            &arm_source,
            child,
            expected_cfg,
            arm_dir,
            src_dir,
            &arm_dir.join(format!("{child}.rs")),
        )?;
    }
    Ok(())
}

/// A fully-flattened `use` leaf: the canonical source path segments (raw
/// identifiers are unrawed) plus either the exported name or a glob marker.
#[derive(Debug)]
enum UseLeaf {
    Item { source_path: Vec<String>, public_name: String },
    Glob { source_path: Vec<String> },
}

fn ident_name(ident: &syn::Ident) -> String {
    ident.unraw().to_string()
}

/// Flatten a `use` tree through groups at every position. Rust raw identifiers
/// are canonicalized here, before any security decision is made.
fn flatten_use(tree: &syn::UseTree, prefix: &[String], out: &mut Vec<UseLeaf>) {
    match tree {
        syn::UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            let name = ident_name(&name.ident);
            path.push(name.clone());
            out.push(UseLeaf::Item { source_path: path, public_name: name });
        }
        syn::UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            path.push(ident_name(&rename.ident));
            out.push(UseLeaf::Item { source_path: path, public_name: ident_name(&rename.rename) });
        }
        syn::UseTree::Path(inner) => {
            let mut path = prefix.to_vec();
            path.push(ident_name(&inner.ident));
            flatten_use(&inner.tree, &path, out);
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                flatten_use(item, prefix, out);
            }
        }
        syn::UseTree::Glob(_) => out.push(UseLeaf::Glob { source_path: prefix.to_vec() }),
    }
}

/// Strip the path anchors that can precede a crate-local path.
fn strip_crate_self(path: &[String]) -> &[String] {
    let mut start = 0;
    while start < path.len() && matches!(path[start].as_str(), "crate" | "self" | "super") {
        start += 1;
    }
    &path[start..]
}

fn path_names_arm(path: &[String]) -> bool {
    path.iter().any(|segment| segment == "arm")
}

fn module_path_redirect(module: &syn::ItemMod) -> Result<Option<PathBuf>, String> {
    let mut redirect = None;
    for attr in &module.attrs {
        if attr.path().is_ident("cfg_attr") {
            return Err(format!(
                "#[cfg_attr] is forbidden on traversed module `{}`",
                ident_name(&module.ident)
            ));
        }
        if !attr.path().is_ident("path") {
            continue;
        }
        if redirect.is_some() {
            return Err(format!("duplicate #[path] on module `{}`", ident_name(&module.ident)));
        }
        let syn::Meta::NameValue(name_value) = &attr.meta else {
            return Err(format!("malformed #[path] on module `{}`", ident_name(&module.ident)));
        };
        let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(path), .. }) = &name_value.value
        else {
            return Err(format!("non-string #[path] on module `{}`", ident_name(&module.ident)));
        };
        let value = path.value();
        if value.is_empty() || value.contains('\0') {
            return Err(format!("malformed #[path] on module `{}`", ident_name(&module.ident)));
        }
        redirect = Some(PathBuf::from(value));
    }
    Ok(redirect)
}

fn normalize_local_path(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!("path escapes local module graph: {}", path.display()));
                }
            }
            std::path::Component::Normal(segment) => normalized.push(segment),
        }
    }
    Ok(normalized)
}

fn redirected_module_path(
    current_dir: &Path,
    graph_root: &Path,
    redirect: &Path,
    module_name: &str,
) -> Result<PathBuf, String> {
    if redirect.is_absolute() {
        return Err(format!("absolute #[path] for module `{module_name}`"));
    }
    let resolved = normalize_local_path(&current_dir.join(redirect))?;
    let root = normalize_local_path(graph_root)?;
    if !resolved.starts_with(&root) {
        return Err(format!(
            "#[path] for module `{module_name}` leaves graph root {}",
            root.display()
        ));
    }
    Ok(resolved)
}

fn child_module_dir(path: &Path, module_name: &str) -> Result<PathBuf, String> {
    let parent =
        path.parent().ok_or_else(|| format!("module file has no parent: {}", path.display()))?;
    if path.file_name().is_some_and(|name| name == "mod.rs") {
        Ok(parent.to_path_buf())
    } else {
        Ok(parent.join(module_name))
    }
}

/// The one loader used by every graph traversal. It resolves inline modules,
/// conventional flat/mod.rs files, and safe `#[path]` redirects identically.
fn load_local_module(
    module: &syn::ItemMod,
    current_dir: Option<&Path>,
    graph_root: Option<&Path>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<(Vec<syn::Item>, Option<PathBuf>), String> {
    let name = ident_name(&module.ident);
    let redirect = module_path_redirect(module)?;
    if let Some((_, inner)) = &module.content {
        let child_dir = match (current_dir, graph_root, redirect.as_deref()) {
            (Some(dir), Some(root), Some(path)) => {
                let resolved = redirected_module_path(dir, root, path, &name)?;
                Some(child_module_dir(&resolved, &name)?)
            }
            (_, _, Some(_)) => {
                return Err(format!("cannot resolve inline #[path] module `{name}`"));
            }
            (Some(dir), _, None) => Some(dir.join(&name)),
            (None, _, None) => None,
        };
        return Ok((inner.clone(), child_dir));
    }

    let current_dir = current_dir.ok_or_else(|| format!("no directory for module `{name}`"))?;
    let graph_root = graph_root.ok_or_else(|| format!("no graph root for module `{name}`"))?;
    let path = if let Some(redirect) = redirect {
        let path = redirected_module_path(current_dir, graph_root, &redirect, &name)?;
        if !path.is_file() {
            return Err(format!("unresolved redirected module `{name}` at {}", path.display()));
        }
        path
    } else {
        let flat = current_dir.join(format!("{name}.rs"));
        let nested = current_dir.join(&name).join("mod.rs");
        match (flat.is_file(), nested.is_file()) {
            (true, false) => flat,
            (false, true) => nested,
            (true, true) => {
                return Err(format!(
                    "ambiguous local module `{name}`: {} and {}",
                    flat.display(),
                    nested.display()
                ));
            }
            (false, false) => {
                return Err(format!(
                    "unresolved local module `{name}` below {}",
                    current_dir.display()
                ));
            }
        }
    };
    let path = std::fs::canonicalize(&path)
        .map_err(|error| format!("canonicalize {}: {error}", path.display()))?;
    let canonical_root = std::fs::canonicalize(graph_root)
        .map_err(|error| format!("canonicalize {}: {error}", graph_root.display()))?;
    if !path.starts_with(&canonical_root) {
        return Err(format!(
            "module `{name}` resolves outside graph root {}",
            canonical_root.display()
        ));
    }
    if !visited.insert(path.clone()) {
        return Err(format!("local module cycle or duplicate at {}", path.display()));
    }
    let child_source = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let child_file = syn::parse_file(&child_source)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let child_dir = child_module_dir(&path, &name)?;
    Ok((child_file.items, Some(child_dir)))
}

fn items_name_arm(items: &[syn::Item]) -> bool {
    struct ArmReference {
        found: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for ArmReference {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            let mut leaves = Vec::new();
            flatten_use(&item.tree, &[], &mut leaves);
            self.found |= leaves.iter().any(|leaf| match leaf {
                UseLeaf::Item { source_path, .. } | UseLeaf::Glob { source_path } => {
                    path_names_arm(source_path)
                }
            });
            syn::visit::visit_item_use(self, item);
        }

        fn visit_path(&mut self, path: &'ast syn::Path) {
            self.found |= path.segments.iter().any(|segment| ident_name(&segment.ident) == "arm");
            syn::visit::visit_path(self, path);
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            self.found |=
                mac.path.segments.iter().any(|segment| ident_name(&segment.ident) == "arm");
            syn::visit::visit_macro(self, mac);
        }
    }

    let mut reference = ArmReference { found: false };
    syn::visit::Visit::visit_file(
        &mut reference,
        &syn::File { shebang: None, attrs: Vec::new(), items: items.to_vec() },
    );
    reference.found
}

fn has_exact_cfg_test(attr: &syn::Attribute) -> bool {
    matches!(
        &attr.meta,
        syn::Meta::List(list) if list.path.is_ident("cfg") && list.tokens.to_string() == "test"
    )
}

fn reviewed_attribute(attr: &syn::Attribute) -> bool {
    match &attr.meta {
        syn::Meta::NameValue(value) if value.path.is_ident("doc") => {
            matches!(&value.value, syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(_), .. }))
        }
        syn::Meta::List(list) if list.path.is_ident("cfg") => true,
        syn::Meta::List(list) if list.path.is_ident("derive") => matches!(
            list.tokens.to_string().as_str(),
            "Debug"
                | "Debug , PartialEq , Eq"
                | "Debug , Clone"
                | "Debug , Clone , Copy"
                | "Debug , Clone , PartialEq , Eq"
                | "Debug , Clone , Copy , PartialEq , Eq"
        ),
        syn::Meta::List(list) if list.path.is_ident("allow") => matches!(
            list.tokens.to_string().as_str(),
            "clippy :: large_enum_variant"
                | "clippy :: too_many_arguments"
                | "missing_docs"
                | "unnameable_types"
        ),
        _ => false,
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
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
        _ => &[],
    }
}

fn production_attributes_are_reviewed(items: &[syn::Item]) -> bool {
    struct AttributeSeal {
        reviewed: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for AttributeSeal {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            let attrs = item_attrs(item);
            if has_cfg_test(attrs) {
                return;
            }
            self.reviewed &= attrs.iter().all(reviewed_attribute);
            syn::visit::visit_item(self, item);
        }

        fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
            let attrs: &[syn::Attribute] = match item {
                syn::ImplItem::Const(item) => &item.attrs,
                syn::ImplItem::Fn(item) => &item.attrs,
                syn::ImplItem::Type(item) => &item.attrs,
                syn::ImplItem::Macro(item) => &item.attrs,
                _ => &[],
            };
            if has_cfg_test(attrs) {
                return;
            }
            self.reviewed &= attrs.iter().all(reviewed_attribute);
            syn::visit::visit_impl_item(self, item);
        }

        fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
            let attrs: &[syn::Attribute] = match item {
                syn::TraitItem::Const(item) => &item.attrs,
                syn::TraitItem::Fn(item) => &item.attrs,
                syn::TraitItem::Type(item) => &item.attrs,
                syn::TraitItem::Macro(item) => &item.attrs,
                _ => &[],
            };
            if has_cfg_test(attrs) {
                return;
            }
            self.reviewed &= attrs.iter().all(reviewed_attribute);
            syn::visit::visit_trait_item(self, item);
        }

        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            self.reviewed &= reviewed_attribute(attr);
        }
    }

    let file = syn::File { shebang: None, attrs: Vec::new(), items: items.to_vec() };
    let mut seal = AttributeSeal { reviewed: true };
    syn::visit::Visit::visit_file(&mut seal, &file);
    seal.reviewed
}
/// Item-position macros can emit imports, modules, and other public surface that
/// `syn` cannot expand. Reject them throughout the traversed production graph,
/// except for the single reviewed Solidity declaration in root `calldata.rs`.
fn item_macro_surface_is_reviewed(
    items: &[syn::Item],
    allow_assembler_sol: bool,
) -> Result<(), String> {
    if !production_attributes_are_reviewed(items) {
        return Err(
            "unknown or unreviewed production attribute in sealed local module graph".into()
        );
    }
    let sol_bindings: Vec<Vec<String>> = items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Use(item_use) if !has_cfg_test(&item_use.attrs) => Some(item_use),
            _ => None,
        })
        .flat_map(|item_use| {
            let mut leaves = Vec::new();
            flatten_use(&item_use.tree, &[], &mut leaves);
            leaves
        })
        .filter_map(|leaf| match leaf {
            UseLeaf::Item { source_path, public_name } if public_name == "sol" => Some(source_path),
            _ => None,
        })
        .collect();
    let exact_sol_binding =
        sol_bindings == [vec!["alloy_sol_types".to_string(), "sol".to_string()]];
    let mut reviewed_sol = 0;
    for item in items {
        let syn::Item::Macro(item_macro) = item else {
            continue;
        };
        if has_cfg_test(&item_macro.attrs) {
            continue;
        }
        let is_reviewed_sol = allow_assembler_sol
            && exact_sol_binding
            && item_macro.ident.is_none()
            && item_macro.mac.path.leading_colon.is_none()
            && item_macro.mac.path.segments.len() == 1
            && ident_name(&item_macro.mac.path.segments[0].ident) == "sol";
        if !is_reviewed_sol {
            return Err("item macro definition/invocation in sealed local module graph".to_string());
        }
        reviewed_sol += 1;
    }
    if reviewed_sol > 1 {
        return Err("multiple sol! item macros in assembler.rs".to_string());
    }
    Ok(())
}

/// Walk every local production module except the reviewed private `arm` subtree.
/// Any direct `arm` import/reference there is rejected, irrespective of visibility
/// or how many private aliases later carry it to a public re-export.
fn seal_local_module_graph(
    items: &[syn::Item],
    current_dir: Option<&Path>,
    graph_root: Option<&Path>,
    visited: &mut BTreeSet<PathBuf>,
    allow_assembler_sol: bool,
) -> Result<(), String> {
    item_macro_surface_is_reviewed(items, allow_assembler_sol)?;
    if items_name_arm(items) {
        return Err("direct arm import/reference outside reviewed root facade".to_string());
    }
    for item in items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        if has_cfg_test(&module.attrs) {
            continue;
        }
        let (child_items, child_dir) = load_local_module(module, current_dir, graph_root, visited)?;
        seal_local_module_graph(&child_items, child_dir.as_deref(), graph_root, visited, false)?;
    }
    Ok(())
}

/// Collect only the reviewed root facade. Every non-arm root module is then
/// traversed by the conservative whole-local-module-graph seal.
fn collect_arm_reexports(
    items: &[syn::Item],
    current_dir: Option<&Path>,
    graph_root: Option<&Path>,
    source: &mut BTreeSet<String>,
    public: &mut BTreeSet<String>,
    visited: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    item_macro_surface_is_reviewed(items, false)?;
    let non_facade_items: Vec<syn::Item> = items
        .iter()
        .filter(|item| {
            !matches!(item, syn::Item::Use(_))
                && !matches!(
                    item,
                    syn::Item::Mod(module)
                        if ident_name(&module.ident) == "arm" || has_cfg_test(&module.attrs)
                )
        })
        .cloned()
        .collect();
    if items_name_arm(&non_facade_items) {
        return Err("direct arm reference in root outside reviewed facade".to_string());
    }
    for item in items {
        match item {
            syn::Item::Use(use_item) => {
                let mut leaves = Vec::new();
                flatten_use(&use_item.tree, &[], &mut leaves);
                for leaf in leaves {
                    let (source_path, public_name) = match leaf {
                        UseLeaf::Item { source_path, public_name } => {
                            (source_path, Some(public_name))
                        }
                        UseLeaf::Glob { source_path } => (source_path, None),
                    };
                    let norm = strip_crate_self(&source_path);
                    if !path_names_arm(norm) {
                        continue;
                    }
                    if !matches!(use_item.vis, syn::Visibility::Public(_)) {
                        return Err(format!("non-public root arm import: {source_path:?}"));
                    }
                    if norm.first().map(String::as_str) != Some("arm") || norm.len() != 2 {
                        return Err(format!("non-direct root arm re-export: {norm:?}"));
                    }
                    let Some(public_name) = public_name else {
                        return Err(format!("glob re-export into arm: {source_path:?}"));
                    };
                    source.insert(norm[1].clone());
                    public.insert(public_name);
                }
            }
            syn::Item::Mod(module) if ident_name(&module.ident) == "arm" => {
                if !matches!(module.vis, syn::Visibility::Inherited)
                    || module.content.is_some()
                    || module.semi.is_none()
                    || !module_attrs_are_exact(module, Some("feature = \"arm\""))
                {
                    return Err(
                        "reviewed root arm module must be exact private out-of-line source".into(),
                    );
                }
            }
            syn::Item::Mod(module) if has_cfg_test(&module.attrs) => {}
            syn::Item::Mod(module) => {
                let allow_assembler_sol = ident_name(&module.ident) == "calldata";
                let (child_items, child_dir) =
                    load_local_module(module, current_dir, graph_root, visited)?;
                seal_local_module_graph(
                    &child_items,
                    child_dir.as_deref(),
                    graph_root,
                    visited,
                    allow_assembler_sol,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn arm_reexports(src: &str) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let file = parse(src);
    let mut source = BTreeSet::new();
    let mut public = BTreeSet::new();
    collect_arm_reexports(&file.items, None, None, &mut source, &mut public, &mut BTreeSet::new())?;
    Ok((source, public))
}

fn arm_reexports_from_path(root: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("canonicalize {}: {error}", root.display()))?;
    let src = std::fs::read_to_string(&root)
        .map_err(|error| format!("read {}: {error}", root.display()))?;
    let file =
        syn::parse_file(&src).map_err(|error| format!("parse {}: {error}", root.display()))?;
    let graph_root =
        root.parent().ok_or_else(|| format!("root source has no parent: {}", root.display()))?;
    let mut source = BTreeSet::new();
    let mut public = BTreeSet::new();
    let mut visited = BTreeSet::from([root.clone()]);
    collect_arm_reexports(
        &file.items,
        Some(graph_root),
        Some(graph_root),
        &mut source,
        &mut public,
        &mut visited,
    )?;
    Ok((source, public))
}

/// Whether `attrs` carries a `#[cfg(test)]`.
fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .map(|meta| meta.path().is_ident("test"))
                .unwrap_or(false)
    })
}

/// Vector 3: the FULL non-`#[cfg(test)]`, non-private (`pub`/`pub(crate)`/`pub(…)`)
/// inherent-method surface of `ty_name`, RECURSING into inline (non-test) `mod { … }`
/// blocks. This captures constructors AND mutators AND path/source setters — ANY
/// newly-added non-test method — so it must be added to the reviewed allowlist.
fn inherent_method_surface(src: &str, ty_name: &str) -> BTreeSet<String> {
    fn walk(items: &[syn::Item], ty_name: &str, out: &mut BTreeSet<String>) {
        for item in items {
            match item {
                syn::Item::Impl(imp) if imp.trait_.is_none() => {
                    let syn::Type::Path(type_path) = &*imp.self_ty else {
                        continue;
                    };
                    let name = type_path
                        .path
                        .segments
                        .last()
                        .map(|s| s.ident.to_string())
                        .unwrap_or_default();
                    if name != ty_name {
                        continue;
                    }
                    for impl_item in &imp.items {
                        if let syn::ImplItem::Fn(method) = impl_item {
                            if has_cfg_test(&method.attrs) {
                                continue;
                            }
                            // Any visibility beyond private (`Inherited`) is surface.
                            if !matches!(method.vis, syn::Visibility::Inherited) {
                                out.insert(method.sig.ident.to_string());
                            }
                        }
                    }
                }
                // Recurse into inline modules so a hidden `impl` is still seen; skip
                // `#[cfg(test)]` modules (test-only).
                syn::Item::Mod(module) => {
                    if let Some((_, inner)) = &module.content
                        && !has_cfg_test(&module.attrs)
                    {
                        walk(inner, ty_name, out);
                    }
                }
                _ => {}
            }
        }
    }
    let file = parse(src);
    let mut out = BTreeSet::new();
    walk(&file.items, ty_name, &mut out);
    out
}

fn inherent_methods<'a>(file: &'a syn::File, type_name: &str) -> Vec<&'a syn::ImplItemFn> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Impl(implementation) if implementation.trait_.is_none() => {
                Some(implementation)
            }
            _ => None,
        })
        .filter(|implementation| {
            let syn::Type::Path(type_path) = &*implementation.self_ty else {
                return false;
            };
            type_path.path.segments.last().map(|segment| ident_name(&segment.ident))
                == Some(type_name.to_owned())
        })
        .flat_map(|implementation| {
            implementation.items.iter().filter_map(|item| match item {
                syn::ImplItem::Fn(method) if !has_cfg_test(&method.attrs) => Some(method),
                _ => None,
            })
        })
        .collect()
}

fn inherent_method<'a>(
    file: &'a syn::File,
    type_name: &str,
    method_name: &str,
) -> &'a syn::ImplItemFn {
    inherent_methods(file, type_name)
        .into_iter()
        .find(|method| ident_name(&method.sig.ident) == method_name)
        .unwrap_or_else(|| panic!("missing {type_name}::{method_name}"))
}

fn simple_path(expression: &syn::Expr) -> Option<String> {
    let syn::Expr::Path(path) = expression else {
        return None;
    };
    Some(
        path.path
            .segments
            .iter()
            .map(|segment| ident_name(&segment.ident))
            .collect::<Vec<_>>()
            .join("::"),
    )
}

#[derive(Debug, PartialEq, Eq)]
struct CallRecord {
    path: String,
    arguments: Vec<Option<String>>,
    string_argument: Option<String>,
}

#[derive(Default)]
struct ProductionCallInventory {
    calls: Vec<CallRecord>,
    function_names: Vec<String>,
}

impl<'ast> syn::visit::Visit<'ast> for ProductionCallInventory {
    fn visit_impl_item_fn(&mut self, method: &'ast syn::ImplItemFn) {
        if has_cfg_test(&method.attrs) {
            return;
        }
        self.function_names.push(ident_name(&method.sig.ident));
        syn::visit::visit_impl_item_fn(self, method);
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if has_cfg_test(&function.attrs) {
            return;
        }
        self.function_names.push(ident_name(&function.sig.ident));
        syn::visit::visit_item_fn(self, function);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Some(path) = simple_path(&call.func) {
            let string_argument = call.args.first().and_then(|argument| match argument {
                syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. }) => {
                    Some(value.value())
                }
                _ => None,
            });
            self.calls.push(CallRecord {
                path,
                arguments: call.args.iter().map(simple_path).collect(),
                string_argument,
            });
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn fallible_local_name(statement: &syn::Stmt) -> Option<String> {
    let syn::Stmt::Local(local) = statement else {
        return None;
    };
    let syn::Pat::Ident(pattern) = &local.pat else {
        return None;
    };
    let initializer = local.init.as_ref()?;
    matches!(&*initializer.expr, syn::Expr::Try(_)).then(|| ident_name(&pattern.ident))
}

// -- b9: facade — private module + private sub-modules -------------------------

#[test]
fn arm_module_is_private() {
    reviewed_arm_module_tree(&manifest_dir().join("src").join("lib.rs"))
        .expect("root arm and every production child must resolve to canonical reviewed source");
}

#[test]
fn arm_submodules_are_private() {
    let modrs = std::fs::read_to_string(arm_dir().join("mod.rs")).expect("mod.rs");
    // Rejects `pub mod <sub>;` AND `pub(crate) mod <sub>;` for every real sub-module.
    // (The `#[cfg(test)] pub(crate) mod testkit` is a test utility and is NOT scanned.)
    for sub in [
        "claim",
        "custody",
        "fail_sink",
        "proofs",
        "providers",
        "producer",
        "provisioning_tool",
        "production_bundle",
        "production_handoff",
        "simulation_entrypoint",
        "simulation_store",
        "settled_loss",
        "request",
        "suppression",
        "transport",
        "witness",
    ] {
        assert!(
            mod_is_private(&modrs, sub),
            "arm sub-module `{sub}` must be a plain `mod {sub};` (no visibility modifier)"
        );
    }
}

// -- b10: exported surface == curated allowlist (normalized, glob-rejecting) ----

#[test]
fn t4d_arm_surface_has_only_the_reviewed_unsigned_candidate_handoff() {
    let (source, public) = arm_reexports_from_path(&manifest_dir().join("src").join("lib.rs"))
        .expect("no glob/deep arm re-export across the public module graph");
    let expected: BTreeSet<String> =
        PUBLIC_API_ALLOWLIST.iter().map(|name| (*name).to_string()).collect();
    // The SOURCE items re-exported from arm must equal the allowlist...
    assert_eq!(source, expected, "arm SOURCE re-export set does not match the curated allowlist");
    // ...AND the PUBLIC exported names too (an `as`-alias to a non-allowlisted name
    // would diverge here).
    assert_eq!(
        public, expected,
        "arm PUBLIC exported-name set does not match the allowlist (alias to a non-allowlisted name?)"
    );
    let store = arm_production("simulation_store.rs");
    assert!(
        !store.contains("pub struct SimulationLedgerHead")
            && !store.contains("pub(crate) struct SimulationLedgerHead"),
        "ledger head must remain private"
    );
    assert_eq!(
        inherent_method_surface(&store, "SimulationLedgerEpoch"),
        BTreeSet::from(["as_bytes".to_owned()]),
        "ledger epoch constructor or mutator escaped"
    );
    assert_eq!(
        inherent_method_surface(&store, "SimulationCorrelationEnvelopeV1"),
        BTreeSet::from([
            "correlation_key".to_owned(),
            "ledger_epoch".to_owned(),
            "sequence".to_owned(),
        ]),
        "correlation envelope constructor or mutator escaped"
    );
    let lib = std::fs::read_to_string(manifest_dir().join("src").join("lib.rs")).expect("lib.rs");
    let parsed = parse(&lib);
    let arm_exports = parsed
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Use(item) => {
                let mut leaves = Vec::new();
                flatten_use(&item.tree, &[], &mut leaves);
                leaves
                    .iter()
                    .any(|leaf| match leaf {
                        UseLeaf::Item { source_path, .. } | UseLeaf::Glob { source_path } => {
                            strip_crate_self(source_path).first().map(String::as_str) == Some("arm")
                        }
                    })
                    .then_some(item)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(arm_exports.len(), 5, "arm root surface must be five reviewed facades");
    let gates = arm_exports
        .iter()
        .map(|item| {
            assert_eq!(item.attrs.len(), 1, "arm facade has ambiguous attributes");
            let syn::Meta::List(list) = &item.attrs[0].meta else {
                panic!("arm facade must have one cfg list");
            };
            assert!(list.path.is_ident("cfg"), "arm facade attribute must be cfg");
            list.tokens.to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        gates,
        BTreeSet::from([
            "all (feature = \"arm\" , feature = \"arm-provisioning\")".to_owned(),
            "feature = \"t4e-handoff\"".to_owned(),
            "feature = \"arm\"".to_owned(),
            "all (feature = \"arm-live-egress\" , not (test))".to_owned(),
        ]),
        "arm facades escaped their exact feature gates"
    );
    #[derive(Default)]
    struct ExternalArmCallsites {
        violations: BTreeSet<String>,
    }

    impl<'ast> syn::visit::Visit<'ast> for ExternalArmCallsites {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }

        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if has_cfg_test(&item.attrs) {
                return;
            }
            let mut leaves = Vec::new();
            flatten_use(&item.tree, &[], &mut leaves);
            for leaf in leaves {
                match leaf {
                    UseLeaf::Glob { source_path }
                        if source_path.first().map(String::as_str) == Some("mev_trader_submit") =>
                    {
                        self.violations.insert("submit glob import".to_string());
                    }
                    UseLeaf::Item { source_path, public_name } => {
                        let source = source_path.last().map(String::as_str);
                        if matches!(
                            source,
                            Some(
                                "AuthorizedCandidate"
                                    | "AuthorizedSignedSubmission"
                                    | "CheckedCandidate"
                                    | "ProdBackend"
                                    | "RawBackend"
                                    | "RawEgress"
                                    | "send_gated"
                            )
                        ) {
                            self.violations
                                .insert(format!("arm import {source_path:?} as {public_name}"));
                        }
                    }
                    UseLeaf::Glob { .. } => {}
                }
            }
            syn::visit::visit_item_use(self, item);
        }

        fn visit_path(&mut self, path: &'ast syn::Path) {
            if let Some(name) = path.segments.last().map(|segment| ident_name(&segment.ident))
                && matches!(
                    name.as_str(),
                    "AuthorizedCandidate"
                        | "AuthorizedSignedSubmission"
                        | "CheckedCandidate"
                        | "ProdBackend"
                        | "RawBackend"
                        | "RawEgress"
                        | "send_gated"
                )
            {
                self.violations.insert(format!("arm path {}", path_name(path)));
            }
            syn::visit::visit_path(self, path);
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            let name = ident_name(&call.method);
            if matches!(name.as_str(), "load_and_sign" | "send_gated") {
                self.violations.insert(format!("arm method call {name}"));
            }
            syn::visit::visit_expr_method_call(self, call);
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            if mac.path.segments.last().is_some_and(|segment| segment.ident == "include") {
                self.violations.insert("include macro redirect".to_string());
            }
            syn::visit::visit_macro(self, mac);
        }
    }

    fn collect_rust(path: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = std::fs::read_dir(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
            .map(|entry| entry.expect("source entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                collect_rust(&entry, files);
            } else if entry.extension().is_some_and(|extension| extension == "rs") {
                files.push(entry);
            }
        }
    }

    let metadata = workspace_metadata();
    let cli = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .find(|package| package["name"] == "base-execution-cli")
        .expect("CLI package");
    assert!(
        cli["dependencies"]
            .as_array()
            .expect("dependencies")
            .iter()
            .any(|dependency| dependency["name"] == "mev-trader-submit"),
        "reviewed CLI submit edge missing"
    );
    let cli_root = PathBuf::from(cli["manifest_path"].as_str().expect("CLI manifest"))
        .parent()
        .expect("CLI root")
        .join("src");
    let mut files = Vec::new();
    collect_rust(&cli_root, &mut files);
    let mut callsites = ExternalArmCallsites::default();
    for path in files {
        let source = std::fs::read_to_string(&path).expect("CLI source");
        let parsed = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        syn::visit::Visit::visit_file(&mut callsites, &parsed);
    }
    assert!(
        callsites.violations.is_empty(),
        "workspace CLI reaches a low-level arm capability: {:?}",
        callsites.violations
    );
}

// -- b11: injection-critical types' FULL non-test method surface is allowlisted --

/// The reviewed, exact non-`#[cfg(test)]` public/`pub(crate)` inherent-method
/// surface of the injection-critical types. Adding ANY non-test method (constructor,
/// mutator, or path/source setter) to these types must update this allowlist.
fn arm_runtime_methods() -> BTreeSet<String> {
    ["open", "sink", "suppression_clear", "suppression_clear_checked", "freshness"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}
fn freshness_sources_methods() -> BTreeSet<String> {
    ["revalidate"].iter().map(|s| (*s).to_string()).collect()
}
fn armed_fail_sink_methods() -> BTreeSet<String> {
    ["from_anchored", "is_poisoned", "observe_kill", "check", "latch", "latch_production"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn all_inherent_method_names(src: &str, ty_name: &str) -> BTreeSet<String> {
    fn walk(items: &[syn::Item], ty_name: &str, out: &mut BTreeSet<String>) {
        for item in items {
            match item {
                syn::Item::Impl(imp) if imp.trait_.is_none() => {
                    let syn::Type::Path(type_path) = &*imp.self_ty else {
                        continue;
                    };
                    if type_path
                        .path
                        .segments
                        .last()
                        .is_some_and(|segment| segment.ident == ty_name)
                    {
                        for impl_item in &imp.items {
                            if let syn::ImplItem::Fn(method) = impl_item {
                                out.insert(method.sig.ident.to_string());
                            }
                        }
                    }
                }
                syn::Item::Mod(module) => {
                    if let Some((_, inner)) = &module.content {
                        walk(inner, ty_name, out);
                    }
                }
                _ => {}
            }
        }
    }

    let file = parse(src);
    let mut out = BTreeSet::new();
    walk(&file.items, ty_name, &mut out);
    out
}

fn has_direct_generic_sink_construction(source: &str) -> bool {
    struct GenericConstructor {
        found: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for GenericConstructor {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }

        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_item_fn(self, item);
            }
        }

        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_item_impl(self, item);
            }
        }
        fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
            if !has_cfg_test(&item.attrs) {
                syn::visit::visit_impl_item_fn(self, item);
            }
        }

        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(function) = &*call.func {
                let segments: Vec<String> = function
                    .path
                    .segments
                    .iter()
                    .map(|segment| ident_name(&segment.ident))
                    .collect();
                self.found |= segments.ends_with(&["ArmedFailSink".to_string(), "new".to_string()]);
            }
            syn::visit::visit_expr_call(self, call);
        }
    }

    let mut inventory = GenericConstructor { found: false };
    syn::visit::Visit::visit_file(&mut inventory, &parse(source));
    inventory.found
}

fn path_name(path: &syn::Path) -> String {
    path.segments.iter().map(|segment| ident_name(&segment.ident)).collect::<Vec<_>>().join("::")
}

#[derive(Default)]
struct ProductionInventory {
    sink_alias: bool,
    free_functions: BTreeSet<String>,
    helper_references: Vec<(String, String, bool)>,
    sink_expressions: Vec<SinkStructExpression>,
    function: Option<String>,
    sink_impl: bool,
}

impl<'ast> syn::visit::Visit<'ast> for ProductionInventory {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if !has_cfg_test(item_attrs(item)) {
            syn::visit::visit_item(self, item);
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::ImplItem::Const(item) => &item.attrs,
            syn::ImplItem::Fn(item) => &item.attrs,
            syn::ImplItem::Type(item) => &item.attrs,
            syn::ImplItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if !has_cfg_test(attrs) {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs: &[syn::Attribute] = match item {
            syn::TraitItem::Const(item) => &item.attrs,
            syn::TraitItem::Fn(item) => &item.attrs,
            syn::TraitItem::Type(item) => &item.attrs,
            syn::TraitItem::Macro(item) => &item.attrs,
            _ => &[],
        };
        if !has_cfg_test(attrs) {
            syn::visit::visit_trait_item(self, item);
        }
    }
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !has_cfg_test(&item.attrs) {
            syn::visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        let prior = self.sink_impl;
        self.sink_impl = matches!(
            &*item.self_ty,
            syn::Type::Path(path)
                if path.path.segments.last().is_some_and(
                    |segment| ident_name(&segment.ident) == "ArmedFailSink"
                )
        );
        syn::visit::visit_item_impl(self, item);
        self.sink_impl = prior;
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        let name = ident_name(&item.sig.ident);
        self.free_functions.insert(name.clone());
        let prior = self.function.replace(name);
        syn::visit::visit_item_fn(self, item);
        self.function = prior;
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        let prior = self.function.replace(ident_name(&item.sig.ident));
        syn::visit::visit_impl_item_fn(self, item);
        self.function = prior;
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        let prior = self.function.replace(ident_name(&item.sig.ident));
        syn::visit::visit_trait_item_fn(self, item);
        self.function = prior;
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        let mut leaves = Vec::new();
        flatten_use(&item.tree, &[], &mut leaves);
        self.sink_alias |= leaves.iter().any(|leaf| {
            matches!(
                leaf,
                UseLeaf::Item { source_path, public_name }
                    if source_path.last().is_some_and(|name| name == "ArmedFailSink")
                        && public_name != "ArmedFailSink"
            )
        });
        syn::visit::visit_item_use(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if has_cfg_test(&item.attrs) {
            return;
        }
        self.sink_alias |= matches!(
            &*item.ty,
            syn::Type::Path(type_path)
                if type_path.path.segments.last().is_some_and(
                    |segment| ident_name(&segment.ident) == "ArmedFailSink"
                )
        );
        syn::visit::visit_item_type(self, item);
    }
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if !has_cfg_test(&local.attrs) {
            syn::visit::visit_local(self, local);
        }
    }

    fn visit_expr_block(&mut self, block: &'ast syn::ExprBlock) {
        if !has_cfg_test(&block.attrs) {
            syn::visit::visit_expr_block(self, block);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(function) = &*call.func
            && function
                .path
                .segments
                .last()
                .is_some_and(|segment| ident_name(&segment.ident) == "from_store_checked")
        {
            self.helper_references.push((
                self.function.clone().unwrap_or_default(),
                path_name(&function.path),
                true,
            ));
            for argument in &call.args {
                syn::visit::Visit::visit_expr(self, argument);
            }
            return;
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
        if expression
            .path
            .segments
            .last()
            .is_some_and(|segment| ident_name(&segment.ident) == "from_store_checked")
        {
            self.helper_references.push((
                self.function.clone().unwrap_or_default(),
                path_name(&expression.path),
                false,
            ));
        }
        syn::visit::visit_expr_path(self, expression);
    }

    fn visit_expr_struct(&mut self, expression: &'ast syn::ExprStruct) {
        let is_named_sink = expression
            .path
            .segments
            .last()
            .is_some_and(|segment| ident_name(&segment.ident) == "ArmedFailSink");
        let is_sink_self = self.sink_impl
            && expression.path.segments.len() == 1
            && ident_name(&expression.path.segments[0].ident) == "Self";
        if is_named_sink || is_sink_self {
            let fields = expression
                .fields
                .iter()
                .map(|field| {
                    let member = match &field.member {
                        syn::Member::Named(ident) => ident_name(ident),
                        syn::Member::Unnamed(index) => index.index.to_string(),
                    };
                    let direct_ident = match &field.expr {
                        syn::Expr::Path(path) if path.path.segments.len() == 1 => {
                            Some(ident_name(&path.path.segments[0].ident))
                        }
                        _ => None,
                    };
                    (member, field.colon_token.is_none(), direct_ident)
                })
                .collect();
            self.sink_expressions.push(SinkStructExpression {
                function: self.function.clone(),
                fields,
                has_rest: expression.rest.is_some(),
            });
        }
        syn::visit::visit_expr_struct(self, expression);
    }
}

fn production_inventory(source: &str) -> ProductionInventory {
    let mut inventory = ProductionInventory::default();
    syn::visit::Visit::visit_file(&mut inventory, &parse(source));
    inventory
}

fn sink_alias_surface_is_empty(source: &str) -> bool {
    !production_inventory(source).sink_alias
}

fn sink_trait_surface(source: &str) -> BTreeSet<String> {
    fn walk(items: &[syn::Item], out: &mut BTreeSet<String>) {
        for item in items {
            match item {
                syn::Item::Impl(item_impl) if !has_cfg_test(&item_impl.attrs) => {
                    let sink_impl = matches!(
                        &*item_impl.self_ty,
                        syn::Type::Path(type_path)
                            if type_path.path.segments.last().is_some_and(
                                |segment| ident_name(&segment.ident) == "ArmedFailSink"
                            )
                    );
                    if sink_impl && let Some((_, trait_path, _)) = &item_impl.trait_ {
                        out.insert(path_name(trait_path));
                    }
                }
                syn::Item::Mod(module) if !has_cfg_test(&module.attrs) => {
                    if let Some((_, inner)) = &module.content {
                        walk(inner, out);
                    }
                }
                _ => {}
            }
        }
    }

    let mut out = BTreeSet::new();
    walk(&parse(source).items, &mut out);
    out
}

fn production_free_function_surface(source: &str) -> BTreeSet<String> {
    production_inventory(source).free_functions
}

fn from_store_checked_calls(source: &str) -> Vec<(String, String, bool)> {
    production_inventory(source).helper_references
}

fn armed_fail_sink_fields_are_private(source: &str) -> bool {
    parse(source).items.iter().any(|item| {
        let syn::Item::Struct(item_struct) = item else {
            return false;
        };
        if item_struct.ident != "ArmedFailSink" {
            return false;
        }
        let names: Vec<String> = item_struct
            .fields
            .iter()
            .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
            .collect();
        names == ["kill".to_string(), "process_poison".to_string()]
            && item_struct
                .fields
                .iter()
                .all(|field| matches!(field.vis, syn::Visibility::Inherited))
    })
}

#[derive(Debug)]
struct SinkStructExpression {
    function: Option<String>,
    fields: Vec<(String, bool, Option<String>)>,
    has_rest: bool,
}

fn sink_struct_expressions(source: &str) -> Vec<SinkStructExpression> {
    production_inventory(source).sink_expressions
}

fn has_exact_canonical_sink_construction(source: &str) -> bool {
    let expressions = sink_struct_expressions(source);
    expressions.len() == 1
        && expressions[0].function.as_deref() == Some("from_store_checked")
        && !expressions[0].has_rest
        && expressions[0].fields
            == [
                ("kill".to_string(), true, Some("kill".to_string())),
                ("process_poison".to_string(), true, Some("process_poison".to_string())),
            ]
}
fn sink_macro_surface_is_reviewed(source: &str) -> bool {
    struct MacroInventory {
        reviewed_matches: usize,
        reviewed_unreachable: usize,
        rejected: bool,
    }

    impl<'ast> syn::visit::Visit<'ast> for MacroInventory {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            let reviewed = match &attr.meta {
                syn::Meta::List(list) if list.path.is_ident("cfg") => {
                    matches!(list.tokens.to_string().as_str(), "test" | "not (test)")
                }
                syn::Meta::List(list) if list.path.is_ident("derive") => matches!(
                    list.tokens.to_string().as_str(),
                    "Debug" | "Debug , Clone , Copy , PartialEq , Eq"
                ),
                _ => false,
            };
            if !reviewed {
                self.rejected = true;
            }
            syn::visit::visit_attribute(self, attr);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            let mut leaves = Vec::new();
            flatten_use(&item.tree, &[], &mut leaves);
            if leaves.iter().any(
                |leaf| matches!(leaf, UseLeaf::Item { public_name, .. } if public_name == "matches"),
            ) {
                self.rejected = true;
            }
            syn::visit::visit_item_use(self, item);
        }
        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            let direct = mac.path.leading_colon.is_none() && mac.path.segments.len() == 1;
            let name = mac.path.segments.first().map(|segment| ident_name(&segment.ident));
            let tokens = mac.tokens.to_string();
            if direct
                && name.as_deref() == Some("matches")
                && tokens == "state , KillState :: Clear { .. }"
            {
                self.reviewed_matches += 1;
            } else if direct
                && name.as_deref() == Some("unreachable")
                && tokens == "\"fixed production latch reason\""
            {
                self.reviewed_unreachable += 1;
            } else {
                self.rejected = true;
            }
            syn::visit::visit_macro(self, mac);
        }
    }

    let file = parse(source);
    let mut inventory =
        MacroInventory { reviewed_matches: 0, reviewed_unreachable: 0, rejected: false };
    syn::visit::Visit::visit_file(&mut inventory, &file);
    !inventory.rejected && inventory.reviewed_matches == 1 && inventory.reviewed_unreachable == 1
}

fn has_sibling_sink_construction_or_child_path(source: &str) -> bool {
    let compact: String = strip_comments(source).chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.contains("ArmedFailSink{") || compact.contains("fail_sink::ArmedFailSink")
}

fn has_arbitrary_kill_path(source: &str) -> bool {
    strip_comments(source).contains("\"/")
}

#[test]
fn constructor_surface_is_sealed() {
    let witness = std::fs::read_to_string(arm_dir().join("witness.rs")).expect("witness.rs");
    let modrs = std::fs::read_to_string(arm_dir().join("mod.rs")).expect("mod.rs");
    let fail_sink = std::fs::read_to_string(arm_dir().join("fail_sink.rs")).expect("fail_sink.rs");
    let fail_sink_production = arm_production("fail_sink.rs");
    assert!(
        modrs.contains(
            "mod fail_sink;\npub use fail_sink::{ArmedFailSink, ProductionLatchOutcome};"
        ),
        "fail sink must be a private child module grouped with its reviewed public re-exports"
    );
    assert!(
        armed_fail_sink_fields_are_private(&fail_sink),
        "ArmedFailSink must retain exactly its two child-private fields"
    );
    for file in ARM_FILES.iter().filter(|file| !matches!(**file, "fail_sink.rs" | "mod.rs")) {
        let source = std::fs::read_to_string(arm_dir().join(file)).expect("arm sibling source");
        assert!(
            !has_sibling_sink_construction_or_child_path(&source),
            "arm sibling `{file}` constructs ArmedFailSink or bypasses its canonical re-export"
        );
    }
    // `ArmRuntime`: only zero-argument `open` (pins stores internally), `freshness`, `sink`, and
    // `suppression_clear` are non-test surface.
    assert_eq!(
        inherent_method_surface(&witness, "ArmRuntime"),
        arm_runtime_methods(),
        "ArmRuntime non-#[cfg(test)] method surface changed — review before allowlisting"
    );
    assert_eq!(
        inherent_method_surface(&fail_sink, "ArmedFailSink"),
        armed_fail_sink_methods(),
        "ArmedFailSink production surface changed — arbitrary store construction may be exposed"
    );
    assert_eq!(
        all_inherent_method_names(&fail_sink, "ArmedFailSink"),
        [
            "check",
            "from_anchored",
            "from_store_checked",
            "is_poisoned",
            "latch",
            "latch_production",
            "new",
            "new_with_process_poison",
            "observe_kill",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        "ArmedFailSink must expose only the canonical production/test constructors and operations"
    );
    assert!(
        has_exact_canonical_sink_construction(&fail_sink_production),
        "the production defining module must contain exactly the canonical ArmedFailSink \
         struct expression inside from_store_checked"
    );
    assert!(
        sink_macro_surface_is_reviewed(&fail_sink_production),
        "fail_sink production must contain only the exact reviewed matches! invocation and no \
         macro definitions or other invocations"
    );
    assert!(
        sink_alias_surface_is_empty(&fail_sink_production),
        "production fail_sink may not alias ArmedFailSink through use or type aliases"
    );
    assert_eq!(
        sink_trait_surface(&fail_sink_production),
        BTreeSet::from(["core::fmt::Debug".to_string()]),
        "ArmedFailSink production trait surface must remain exactly the reviewed Debug impl"
    );
    assert!(
        production_free_function_surface(&fail_sink_production).is_empty(),
        "fail_sink production may not add free constructor/delegation functions"
    );
    assert_eq!(
        from_store_checked_calls(&fail_sink_production),
        vec![("from_anchored".to_string(), "Self::from_store_checked".to_string(), true)],
        "from_store_checked must have exactly the reviewed production caller"
    );
    assert!(
        fail_sink.contains(
            "pub fn from_anchored(kill: AnchoredKillStateStore) -> Result<Self, StartupError>"
        ),
        "production sink constructor must accept only the concrete anchored store"
    );
    assert!(
        fail_sink.contains(
            "#[cfg(test)]\n    pub(crate) fn new(kill: Box<dyn KillStateStore + Send + Sync>) -> Result<Self, StartupError>"
        ),
        "generic store injection constructor must retain its exact test-only definition"
    );
    assert!(
        witness.contains("pub fn open() -> Result<Self, ArmRuntimeOpenError>"),
        "production runtime open must be zero-argument and preserve composite causes"
    );
    // `FreshnessSources`: only `revalidate` is non-test surface (`new`/`with_forced_gate`
    // are `#[cfg(test)]`; production builds it only via `ArmRuntime::freshness`).
    assert_eq!(
        inherent_method_surface(&witness, "FreshnessSources"),
        freshness_sources_methods(),
        "FreshnessSources non-#[cfg(test)] method surface changed — review before allowlisting"
    );
    // The pinned suppression constructors are pub(crate) (internal), never `pub`.
    let suppression = arm_raw("suppression.rs");
    for pinned in ["at_pinned_path", "open_pinned"] {
        assert!(
            suppression.contains(&format!("pub(crate) fn {pinned}")),
            "pinned constructor `{pinned}` must be pub(crate) (internal)"
        );
        assert!(
            !suppression.contains(&format!("\n    pub fn {pinned}")),
            "pinned constructor `{pinned}` must NOT be publicly `pub`"
        );
    }
}

#[test]
fn anchored_kill_consumption_and_observation_are_exact() {
    let fail_sink = arm_production("fail_sink.rs");
    let witness = arm_production("witness.rs");
    let all = all_arm_production();

    assert_eq!(
        witness.matches("open_anchored_killstate()").count(),
        1,
        "runtime must call the sole trader factory exactly once"
    );
    assert_eq!(
        witness.matches("ArmedFailSink::from_anchored(kill)").count(),
        1,
        "factory output must flow directly into the concrete sink constructor"
    );
    assert!(
        all.iter().all(|(_, body)| !has_direct_generic_sink_construction(body)),
        "production may not call the test-only generic sink constructor"
    );
    assert_eq!(
        fail_sink.matches("self.kill.load()").count(),
        1,
        "raw durable load must exist only in observe_kill"
    );
    let observe = fail_sink.find("pub fn observe_kill").expect("observe_kill");
    let check = fail_sink.find("pub fn check").expect("check");
    assert!(
        fail_sink[observe..check].contains("self.kill.load()"),
        "the sole raw load must be inside observe_kill"
    );
    assert!(
        fail_sink[check..].contains("self.observe_kill()"),
        "check must delegate to observe_kill"
    );
    assert!(
        witness.contains("let Ok(kill) = self.sink.observe_kill() else")
            && witness.contains("kill,\n        };"),
        "egress must observe first and pass the returned Clear state into submit_gate"
    );
    let egress_observe = witness.find("self.sink.observe_kill()").expect("egress observe");
    assert!(
        witness[egress_observe..].find("self.force_gate_open").is_some(),
        "egress durable observation must precede the forced-open test seam"
    );
    assert!(
        !has_arbitrary_kill_path(&fail_sink) && !has_arbitrary_kill_path(&witness),
        "sink/runtime production contains an arbitrary store or alternate anchor path literal"
    );
}

// -- b12: NEGATIVE FIXTURES — the seal catches every evasion vector -------------

#[test]
fn negative_fixture_pub_crate_module_is_rejected() {
    // Vector (a): a `pub(crate) mod` (or any visibility) must NOT count as private.
    assert!(!mod_is_private("pub(crate) mod arm;", "arm"), "pub(crate) mod slipped through");
    assert!(!mod_is_private("pub mod arm;", "arm"), "pub mod slipped through");
    assert!(!mod_is_private("pub(in crate::x) mod arm;", "arm"), "pub(in ..) mod slipped through");
    // Sanity: a genuinely-plain declaration passes; a missing module fails.
    assert!(mod_is_private("mod arm;", "arm"), "plain mod wrongly rejected");
    assert!(!mod_is_private("mod other;", "arm"), "missing module wrongly accepted");
}
#[test]
fn negative_fixtures_reviewed_arm_modules_require_exact_source() {
    assert!(
        exact_module_declaration(
            "#[cfg(feature = \"arm\")] mod arm;",
            "arm",
            Some("feature = \"arm\""),
        )
        .is_some(),
        "canonical root arm declaration was rejected"
    );
    for source in [
        "#[cfg(feature = \"arm\")] mod arm {}",
        "#[cfg(feature = \"arm\")]\n#[path = \"decoy.rs\"] mod arm;",
        "#[cfg(feature = \"arm\")]\n#[cfg_attr(test, path = \"decoy.rs\")] mod arm;",
        "#[cfg(feature = \"arm\")]\n#[unknown] mod arm;",
    ] {
        assert!(
            exact_module_declaration(source, "arm", Some("feature = \"arm\"")).is_none(),
            "non-canonical root arm declaration escaped: {source}"
        );
    }

    for source in [
        "mod fail_sink {}",
        "#[path = \"decoy.rs\"] mod fail_sink;",
        "#[cfg_attr(feature = \"arm\", path = \"decoy.rs\")] mod fail_sink;",
        "#[path = \"../outside.rs\"] mod fail_sink;",
        "#[unknown] mod fail_sink;",
    ] {
        assert!(
            exact_module_declaration(source, "fail_sink", None).is_none(),
            "inline/redirected/attributed arm child escaped: {source}"
        );
    }
}

#[test]
fn negative_fixtures_fake_test_tail_markers_fail_closed() {
    let marker = "#[cfg(test)]\nmod tests";
    let real_tail = format!("{marker} {{}}\n");
    let normal = format!("const FAKE: &str = \"{marker}\";\n{real_tail}");
    let raw = format!("const FAKE: &str = r#\"{marker}\"#;\n{real_tail}");
    let comment = format!("/* {marker} */\n{real_tail}");
    for (kind, source) in
        [("normal string", normal), ("raw string", raw), ("block comment", comment)]
    {
        assert!(
            production_prefix(&source, "fixture.rs").is_err(),
            "{kind} fake marker did not fail closed"
        );
    }
    assert!(
        production_prefix(&format!("const FAKE: &str = r#\"{marker}\"#;"), "fixture.rs").is_err(),
        "marker with no structural terminal test module did not fail closed"
    );
    assert_eq!(
        production_prefix(&format!("fn production() {{}}\n{real_tail}"), "fixture.rs")
            .expect("canonical tail"),
        "fn production() {}\n"
    );
}

#[test]
fn negative_fixture_attribute_macro_generated_arm_reexport_is_rejected() {
    assert!(
        arm_reexports("#[expose_arm] mod generated {}").is_err(),
        "unknown attribute macro capable of generating an arm re-export escaped"
    );
    assert!(
        arm_reexports("#[cfg_attr(feature = \"arm\", expose_arm)] mod generated {}").is_err(),
        "cfg_attr attribute-macro payload escaped"
    );
}

#[test]
fn negative_fixture_leading_group_reexport_is_caught() {
    // A LEADING `Group` must be traversed: `pub use crate::{arm::SuppressionFileStore};`.
    let (source, _public) =
        arm_reexports("pub use crate::{arm::SuppressionFileStore};").expect("direct arm item");
    assert!(
        source.contains("SuppressionFileStore"),
        "leading-group arm re-export was not flattened: {source:?}"
    );
    // A leading group into a DEEP arm path is rejected.
    assert!(
        arm_reexports("pub use {self::arm::witness::SystemClock};").is_err(),
        "leading-group deep arm path not caught"
    );
    // A leading group into an arm glob is rejected.
    assert!(
        arm_reexports("pub use crate::{arm::*};").is_err(),
        "leading-group arm glob not caught"
    );
}

#[test]
fn negative_fixture_alias_to_nonallowlisted_name_is_caught() {
    // `arm::ArmRuntime as UnsafeRuntime` — SOURCE is allowlisted but the PUBLIC name
    // is not, so the public-name set diverges from the allowlist.
    let (source, public) =
        arm_reexports("pub use arm::ArmRuntime as UnsafeRuntime;").expect("direct");
    assert!(source.contains("ArmRuntime"), "source item not captured");
    assert!(
        public.contains("UnsafeRuntime") && !public.contains("ArmRuntime"),
        "the `as`-alias public name was not captured: {public:?}"
    );
    let expected: BTreeSet<String> =
        PUBLIC_API_ALLOWLIST.iter().map(|name| (*name).to_string()).collect();
    assert_ne!(public, expected, "aliased public-name set indistinguishable from allowlist");
    // An alias of a NON-allowlisted item is caught in the source set too.
    let (source2, _public2) =
        arm_reexports("pub use arm::EgressPlan as Backdoor;").expect("direct");
    assert!(source2.contains("EgressPlan"), "aliased source item not captured");
}

#[test]
fn negative_fixture_public_inline_module_alias_is_caught() {
    let fixture = "pub mod escape { pub use crate::arm::ArmRuntime as UnsafeRuntime; }";
    assert!(
        arm_reexports(fixture).is_err(),
        "arm alias outside the reviewed root facade escaped the local graph seal"
    );
}

#[test]
fn negative_fixture_public_out_of_line_module_alias_is_caught() {
    let fixture = ModuleFixture::new("alias");
    fixture.write("lib.rs", "pub mod escape;");
    fixture.write("escape.rs", "pub use crate::arm::ArmRuntime as UnsafeRuntime;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "out-of-line arm alias outside the reviewed root facade escaped the graph seal"
    );
}

#[test]
fn negative_fixture_public_out_of_line_module_deep_path_is_rejected() {
    let fixture = ModuleFixture::new("deep");
    fixture.write("lib.rs", "pub mod outer;");
    fixture.write("outer/mod.rs", "pub mod inner;");
    fixture.write("outer/inner.rs", "pub use crate::arm::witness::SystemClock;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "recursive out-of-line deep arm path escaped graph walk"
    );
}

#[test]
fn negative_fixture_public_out_of_line_module_glob_is_rejected() {
    let fixture = ModuleFixture::new("glob");
    fixture.write("lib.rs", "pub mod escape;");
    fixture.write("escape.rs", "pub use crate::arm::*;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "out-of-line arm glob escaped graph walk"
    );
}

#[test]
fn negative_fixture_private_out_of_line_module_alias_is_rejected() {
    let fixture = ModuleFixture::new("private-alias");
    fixture.write("lib.rs", "mod escape; pub use escape::UnsafeSink;");
    fixture.write("escape.rs", "pub use crate::arm::ArmedFailSink as UnsafeSink;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "arm alias through a private out-of-line module escaped graph resolution"
    );
}

#[test]
fn negative_fixture_private_out_of_line_module_deep_alias_chain_is_rejected() {
    let fixture = ModuleFixture::new("private-deep-alias");
    fixture.write("lib.rs", "mod escape; pub use escape::UnsafeSink;");
    fixture.write("escape.rs", "mod deeper; pub use deeper::UnsafeSink;");
    fixture.write("escape/deeper.rs", "pub use crate::arm::ArmedFailSink as UnsafeSink;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "arm alias through a deep private-module chain escaped graph resolution"
    );
}

#[test]
fn negative_fixture_private_out_of_line_module_glob_is_rejected() {
    let fixture = ModuleFixture::new("private-glob");
    fixture.write("lib.rs", "mod escape; pub use escape::*;");
    fixture.write("escape.rs", "pub use crate::arm::ArmedFailSink;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "arm glob through a private out-of-line module escaped graph resolution"
    );
}
#[test]
fn negative_fixture_private_import_alias_is_rejected() {
    let fixture = "use crate::arm as hidden; pub use hidden::ArmedFailSink;";
    assert!(
        arm_reexports(fixture).is_err(),
        "private arm import alias escaped the root facade seal"
    );
}

#[test]
fn negative_fixture_crate_qualified_sibling_chain_is_rejected() {
    let fixture = "mod sibling { use crate::arm as hidden; pub use hidden::ArmedFailSink; }";
    assert!(
        arm_reexports(fixture).is_err(),
        "crate-qualified arm import in a sibling module escaped the graph seal"
    );
}

#[test]
fn negative_fixture_super_qualified_ancestor_chain_is_rejected() {
    let fixture = r"
        mod outer {
            mod inner {
                use super::super::arm as hidden;
                pub use hidden::ArmedFailSink;
            }
        }
    ";
    assert!(
        arm_reexports(fixture).is_err(),
        "super-qualified ancestor arm import escaped the graph seal"
    );
}

#[test]
fn negative_fixtures_raw_arm_identifiers_are_canonicalized() {
    let (source, public) =
        arm_reexports("pub use r#arm::UnsafeSink;").expect("raw direct root facade leaf");
    assert!(
        source.contains("UnsafeSink") && public.contains("UnsafeSink"),
        "raw direct arm identifier was not canonicalized into the reviewed facade inventory"
    );
    let expected: BTreeSet<String> =
        PUBLIC_API_ALLOWLIST.iter().map(|name| (*name).to_string()).collect();
    assert_ne!(public, expected, "raw direct fixture matched the curated allowlist");
    assert!(
        arm_reexports("pub use crate::{r#arm::*};").is_err(),
        "raw grouped arm glob escaped canonicalization"
    );
    assert!(
        arm_reexports("use crate::r#arm as hidden; pub use hidden::ArmedFailSink;").is_err(),
        "raw aliased arm import escaped canonicalization"
    );
}

#[test]
fn negative_fixture_path_redirect_wins_over_benign_decoy() {
    let fixture = ModuleFixture::new("path-redirect");
    fixture.write("lib.rs", "#[path = \"redirected.rs\"] mod escape;");
    fixture.write("escape.rs", "pub struct Harmless;");
    fixture.write("redirected.rs", "use crate::arm as hidden; pub use hidden::ArmedFailSink;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "#[path] redirect was ignored in favor of the benign conventional-path decoy"
    );
}

#[test]
fn negative_fixture_cfg_attr_path_redirect_with_benign_decoy_fails_closed() {
    let fixture = ModuleFixture::new("cfg-attr-path-redirect");
    fixture.write("lib.rs", "#[cfg_attr(feature = \"arm\", path = \"redirected.rs\")] mod escape;");
    fixture.write("escape.rs", "pub struct Harmless;");
    fixture.write("redirected.rs", "pub use crate::arm::ArmedFailSink;");

    assert!(
        arm_reexports_from_path(&fixture.lib()).is_err(),
        "#[cfg_attr(..., path = ...)] redirect was ignored in favor of its benign decoy"
    );
}

#[test]
fn negative_fixtures_item_macros_in_local_graph_fail_closed() {
    let macro_rules = ModuleFixture::new("macro-rules-reexport");
    macro_rules.write("lib.rs", "mod escape;");
    macro_rules.write(
        "escape.rs",
        "macro_rules! expose { () => { pub use crate::arm::ArmedFailSink; } } expose!();",
    );
    assert!(
        arm_reexports_from_path(&macro_rules.lib()).is_err(),
        "macro_rules re-export escaped the local module graph seal"
    );

    let include = ModuleFixture::new("include-reexport");
    include.write("lib.rs", "mod escape;");
    include.write("escape.rs", "include!(\"generated.rs\");");
    include.write("generated.rs", "pub use crate::arm::ArmedFailSink;");
    assert!(
        arm_reexports_from_path(&include.lib()).is_err(),
        "include! item injection escaped the local module graph seal"
    );

    assert!(
        arm_reexports("expose_arm_surface! { pub use crate::arm::ArmedFailSink; }").is_err(),
        "item-macro invocation capable of generating a re-export escaped the root seal"
    );
}

#[test]
fn negative_fixture_production_after_test_module_fails_closed() {
    let fixture = r"
        #[cfg(test)]
        mod tests {}
        fn delegated_constructor(kill: K, process_poison: P) -> ArmedFailSink {
            ArmedFailSink::from_store_checked(kill, process_poison)
        }
    ";
    assert!(
        !test_modules_are_terminal(fixture),
        "production item after a test module was masked as a test tail"
    );
    assert_eq!(
        from_store_checked_calls(fixture),
        vec![(
            "delegated_constructor".to_string(),
            "ArmedFailSink::from_store_checked".to_string(),
            true,
        )],
        "AST call inventory failed to preserve production code after a test module"
    );
}
#[test]
fn negative_fixtures_unsafe_or_malformed_path_redirects_fail_closed() {
    let outside = ModuleFixture::new("path-outside");
    outside.write("lib.rs", "#[path = \"../outside.rs\"] mod escape;");
    assert!(
        arm_reexports_from_path(&outside.lib()).is_err(),
        "out-of-tree #[path] redirect did not fail closed"
    );

    let absolute = ModuleFixture::new("path-absolute");
    absolute.write("lib.rs", "#[path = \"/tmp/escape.rs\"] mod escape;");
    assert!(
        arm_reexports_from_path(&absolute.lib()).is_err(),
        "absolute #[path] redirect did not fail closed"
    );

    assert!(
        arm_reexports("#[path(misdirected)] mod escape {}").is_err(),
        "malformed inline #[path] redirect did not fail closed"
    );
}

#[test]
fn negative_fixture_deep_backdoor_path_is_caught() {
    // `arm::<submodule>::<item>` (deeper than direct `arm::<item>`) is rejected —
    // a `backdoor` sub-module cannot be used to smuggle a low-level type out.
    assert!(
        arm_reexports("pub use arm::backdoor::ArmRuntime;").is_err(),
        "deep arm re-export path not caught"
    );
    assert!(
        arm_reexports("pub use crate::arm::sub::Type;").is_err(),
        "nested deep arm re-export path not caught"
    );
    // Direct `arm::<name>` (renamed or not) is accepted (Ok, not Err).
    assert!(arm_reexports("pub use arm::ArmRuntime;").is_ok());
    assert!(arm_reexports("pub use arm::ArmRuntime as X;").is_ok());
}

#[test]
fn negative_fixture_inline_module_and_mutator_are_caught() {
    // An `impl ArmRuntime` hidden inside an inline (non-test) module is still seen.
    let inline = r"
        mod inner {
            impl ArmRuntime {
                pub fn from_path(p: &str) -> Result<Self, ()> { unimplemented!() }
            }
        }
    ";
    assert!(
        inherent_method_surface(inline, "ArmRuntime").contains("from_path"),
        "inline-module impl was not traversed"
    );
    // A non-`Self`-returning mutator / path setter is part of the method surface.
    let mutator = r"
        impl ArmRuntime {
            pub fn set_path(&mut self, p: &str) {}
            pub(crate) fn with_source(&mut self, s: &dyn DrawdownSource) {}
        }
    ";
    let surface = inherent_method_surface(mutator, "ArmRuntime");
    assert!(surface.contains("set_path"), "non-Self mutator not in method surface");
    assert!(surface.contains("with_source"), "pub(crate) source setter not in method surface");
    // A `#[cfg(test)]` inline module is skipped (its impls are test-only).
    let test_mod = r"
        #[cfg(test)]
        mod t {
            impl ArmRuntime {
                pub fn from_path() -> Self { unimplemented!() }
            }
        }
    ";
    assert!(
        !inherent_method_surface(test_mod, "ArmRuntime").contains("from_path"),
        "a #[cfg(test)] inline-module impl was wrongly counted as production surface"
    );
    // A private (`Inherited`) method is NOT surface.
    let private = r"
        impl ArmRuntime {
            fn internal_helper(&self) {}
        }
    ";
    assert!(
        inherent_method_surface(private, "ArmRuntime").is_empty(),
        "a private method was wrongly counted as surface"
    );
}

#[test]
fn negative_fixture_direct_sink_new_is_rejected() {
    let fixture =
        "fn bypass(store: Box<dyn KillStateStore + Send + Sync>) { ArmedFailSink::new(store); }";
    assert!(
        has_direct_generic_sink_construction(fixture),
        "direct generic ArmedFailSink::new fixture escaped detection"
    );
}

#[test]
fn negative_fixture_canonical_and_alias_sink_literals_are_rejected() {
    let fixture = r"
        use self::ArmedFailSink as SinkAlias;
        type OtherSinkAlias = self::ArmedFailSink;
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill, process_poison }
            }
        }
        fn forge(kill: K, process_poison: P) -> SinkAlias {
            SinkAlias { kill, process_poison }
        }
    ";
    assert!(
        has_exact_canonical_sink_construction(fixture),
        "fixture must demonstrate that literal inventory alone misses the alias literal"
    );
    assert!(
        !sink_alias_surface_is_empty(fixture),
        "use/type aliases of ArmedFailSink escaped the defining-module alias seal"
    );
    assert!(
        !sink_alias_surface_is_empty("use self::ArmedFailSink as SinkAlias;"),
        "renamed ArmedFailSink import escaped the alias seal"
    );
    assert!(
        !sink_alias_surface_is_empty("type SinkAlias = self::ArmedFailSink;"),
        "ArmedFailSink type alias escaped the alias seal"
    );
}
#[test]
fn negative_fixtures_block_scoped_aliases_and_local_helpers_are_rejected() {
    let import_alias = r"
        fn forge(kill: K, process_poison: P) -> ArmedFailSink {
            use self::ArmedFailSink as SinkAlias;
            SinkAlias { kill, process_poison }
        }
    ";
    let type_alias = r"
        fn forge(kill: K, process_poison: P) -> ArmedFailSink {
            type SinkAlias = self::ArmedFailSink;
            SinkAlias { kill, process_poison }
        }
    ";
    assert!(
        !sink_alias_surface_is_empty(import_alias),
        "block-scoped import alias escaped production inventory"
    );
    assert!(
        !sink_alias_surface_is_empty(type_alias),
        "block-scoped type alias escaped production inventory"
    );

    let local_helper = r"
        fn outer() {
            fn delegated(kill: K, process_poison: P) -> ArmedFailSink {
                ArmedFailSink::from_store_checked(kill, process_poison)
            }
        }
    ";
    let functions = production_free_function_surface(local_helper);
    assert!(
        functions.contains("outer") && functions.contains("delegated"),
        "block-scoped delegated constructor escaped free-function inventory"
    );
    assert_eq!(
        from_store_checked_calls(local_helper),
        vec![("delegated".to_string(), "ArmedFailSink::from_store_checked".to_string(), true,)],
        "block-scoped delegated constructor escaped helper-reference inventory"
    );
}

#[test]
fn negative_fixtures_indirect_private_helper_references_are_rejected() {
    let parenthesized = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                (Self::from_store_checked)(kill, process_poison())
            }
        }
    ";
    let cast = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                (Self::from_store_checked as fn(K, P) -> Self)(kill, process_poison())
            }
        }
    ";
    let bound = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                let checked = Self::from_store_checked;
                checked(kill, process_poison())
            }
        }
    ";
    let indirect = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                let checked = Self::from_store_checked;
                (checked)(kill, process_poison())
            }
        }
    ";
    let canonical =
        vec![("from_anchored".to_string(), "Self::from_store_checked".to_string(), true)];
    for (kind, source) in [
        ("parenthesized", parenthesized),
        ("cast", cast),
        ("bound function item", bound),
        ("indirect call", indirect),
    ] {
        let references = from_store_checked_calls(source);
        assert_ne!(references, canonical, "{kind} helper reference matched the canonical call");
        assert!(
            references
                .iter()
                .any(|(_, path, direct)| { path == "Self::from_store_checked" && !direct }),
            "{kind} helper reference was not inventoried as indirect"
        );
    }
}

#[test]
fn negative_fixture_trait_method_delegating_to_private_helper_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                Self::from_store_checked(kill, process_poison())
            }
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill, process_poison }
            }
        }
        impl Forge for ArmedFailSink {
            fn forge(kill: K, process_poison: P) -> Self {
                Self::from_store_checked(kill, process_poison)
            }
        }
    ";
    assert!(
        sink_trait_surface(fixture).contains("Forge"),
        "delegating trait impl escaped the exact trait surface"
    );
    assert_ne!(
        from_store_checked_calls(fixture),
        vec![("from_anchored".to_string(), "Self::from_store_checked".to_string(), true)],
        "delegating trait method escaped the private-helper caller inventory"
    );
}

#[test]
fn negative_fixture_free_function_delegating_to_private_helper_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_anchored(kill: K) -> Self {
                Self::from_store_checked(kill, process_poison())
            }
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill, process_poison }
            }
        }
        fn forge(kill: K, process_poison: P) -> ArmedFailSink {
            ArmedFailSink::from_store_checked(kill, process_poison)
        }
    ";
    assert!(
        production_free_function_surface(fixture).contains("forge"),
        "delegating free function escaped the production free-function surface"
    );
    assert_ne!(
        from_store_checked_calls(fixture),
        vec![("from_anchored".to_string(), "Self::from_store_checked".to_string(), true)],
        "delegating free function escaped the private-helper caller inventory"
    );
}
#[test]
fn negative_fixture_free_helper_sink_expression_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
        fn forge(kill: K, process_poison: P) -> ArmedFailSink {
            ArmedFailSink { kill: kill, process_poison: process_poison }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "free-helper ArmedFailSink expression escaped AST inventory"
    );
}

#[test]
fn negative_fixture_trait_impl_self_sink_expression_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
        impl Forge for ArmedFailSink {
            fn forge(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "trait-impl Self sink expression escaped AST inventory"
    );
}

#[test]
fn negative_fixture_qualified_sink_expression_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
        fn forge(kill: K, process_poison: P) -> self::ArmedFailSink {
            self::ArmedFailSink { kill: kill, process_poison: process_poison }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "qualified ArmedFailSink expression escaped AST inventory"
    );
}

#[test]
fn negative_fixture_defining_descendant_sink_expression_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
        mod descendant {
            fn forge(kill: K, process_poison: P) -> super::ArmedFailSink {
                super::ArmedFailSink { kill: kill, process_poison: process_poison }
            }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "defining-descendant ArmedFailSink expression escaped AST inventory"
    );
}

#[test]
fn negative_fixture_reordered_sink_fields_are_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { process_poison: process_poison, kill: kill }
            }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "reordered sink fields escaped canonical-expression check"
    );
}

#[test]
fn negative_fixture_shorthand_sink_fields_in_alternate_constructor_are_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn alternate(kill: K, process_poison: P) -> Self {
                Self { kill, process_poison }
            }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "alternate-constructor shorthand sink fields escaped canonical-expression check"
    );
}

#[test]
fn negative_fixture_computed_sink_fields_are_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: wrap(kill), process_poison: choose(process_poison) }
            }
        }
    ";
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "computed sink field expressions escaped canonical-expression check"
    );
}

#[test]
fn negative_fixture_defining_module_alternate_sink_constructor_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
            fn alternate(kill: K, process_poison: P) -> Self {
                Self { kill: kill, process_poison: process_poison }
            }
        }
    ";
    let expressions = sink_struct_expressions(fixture);
    assert_eq!(expressions.len(), 2, "defining-module alternate expression was not inventoried");
    assert!(
        !has_exact_canonical_sink_construction(fixture),
        "defining-module alternate constructor escaped canonical-expression check"
    );
}

#[test]
fn negative_fixture_descendant_relative_struct_construction_is_rejected() {
    let fixture = r"
        mod descendant {
            fn forge() -> super::ArmedFailSink {
                super::fail_sink::ArmedFailSink {
                    kill: computed_store(),
                    process_poison: computed_poison(),
                }
            }
        }
    ";
    assert!(
        has_sibling_sink_construction_or_child_path(fixture),
        "relative-path computed ArmedFailSink struct literal escaped the source seal"
    );
    assert!(
        armed_fail_sink_fields_are_private(
            &std::fs::read_to_string(arm_dir().join("fail_sink.rs")).expect("fail_sink.rs")
        ),
        "child-private fields no longer make descendant struct literals compiler errors"
    );
}

#[test]
fn negative_fixture_alternate_store_constructor_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            pub fn from_store(store: AlternateStore) -> Self { unimplemented!() }
        }
    ";
    assert_ne!(
        inherent_method_surface(fixture, "ArmedFailSink"),
        armed_fail_sink_methods(),
        "alternate store constructor fixture escaped the exact method allowlist"
    );
}
#[test]
fn negative_fixture_macro_generated_sink_literal_is_rejected() {
    let fixture = r"
        impl ArmedFailSink {
            fn from_store_checked(kill: K, process_poison: P) -> Self {
                let state = KillState::Clear { verified_at: 0 };
                let _ = matches!(state, KillState::Clear { .. });
                Self { kill, process_poison }
            }
        }
        macro_rules! forge_sink {
            ($kill:expr, $poison:expr) => {
                ArmedFailSink { kill: $kill, process_poison: $poison }
            };
        }
        fn alternate(kill: K, process_poison: P) -> ArmedFailSink {
            forge_sink!(kill, process_poison)
        }
    ";
    assert!(
        has_exact_canonical_sink_construction(fixture),
        "fixture must retain the canonical literal while hiding the second in macro tokens"
    );
    assert!(
        !sink_macro_surface_is_reviewed(fixture),
        "macro-generated alternate sink construction escaped the reviewed macro surface"
    );
}

#[test]
fn negative_fixture_alternate_kill_path_is_rejected() {
    let fixture = r#"fn open_alt() { let path = "/tmp/mev-killstate-anchor/epoch.redb"; }"#;
    assert!(has_arbitrary_kill_path(fixture), "alternate path fixture escaped detection");
}

// -- b1: witness capability types carry no Clone/Copy + single ownership -------

#[test]
fn witness_capability_types_are_not_duplicable() {
    let witness = arm_production("witness.rs");
    // Each capability type is declared immediately after a bare `#[derive(Debug)]`
    // — never Clone/Copy — so a capability value cannot be duplicated.
    for name in [
        "pub struct CheckedCandidate",
        "pub struct AuthorizedCandidate",
        "pub struct AuthorizedSignedSubmission",
        "pub struct PairedSubmission",
    ] {
        let marker = format!("#[derive(Debug)]\n{name}");
        assert!(witness.contains(&marker), "{name} must derive only Debug (no Clone/Copy)");
    }
    // And no hand-written Clone/Copy impl for them either.
    for name in [
        "CheckedCandidate",
        "AuthorizedCandidate",
        "AuthorizedSignedSubmission",
        "PairedSubmission",
    ] {
        assert!(!witness.contains(&format!("impl Clone for {name}")), "{name} Clone impl present");
        assert!(!witness.contains(&format!("impl Copy for {name}")), "{name} Copy impl present");
    }
}

// -- b2: exactly one `send_gated` entry point ----------------------------------

#[test]
fn exactly_one_send_gated() {
    let definitions: usize = all_arm_production()
        .iter()
        .map(|(_file, body)| body.matches("fn send_gated").count())
        .sum();
    assert_eq!(definitions, 1, "there must be exactly one send_gated definition");
    // It lives in transport.rs.
    assert!(arm_production("transport.rs").contains("pub fn send_gated"));
}

// -- b3 / b4: the ONLY real egress is ProdBackend::execute, gated --------------

#[test]
fn reqwest_is_confined_to_gated_prodbackend() {
    for (file, body) in all_arm_production() {
        if file == "transport.rs" {
            continue;
        }
        assert!(!body.contains("reqwest"), "reqwest reachable outside transport.rs: {file}");
        assert!(!body.contains(".send()"), "network send outside transport.rs: {file}");
    }
    let transport = arm_production("transport.rs");
    // ProdBackend (the only reqwest holder) is gated to arm-live-egress + non-test.
    assert!(
        transport.contains(
            "#[cfg(all(feature = \"arm-live-egress\", not(test)))]\n#[derive(Debug)]\npub struct ProdBackend"
        ),
        "ProdBackend is not gated to arm-live-egress + not(test)"
    );
    // Every `reqwest` mention sits after the first live-egress cfg gate (i.e. only
    // inside the gated ProdBackend region).
    let gate = "#[cfg(all(feature = \"arm-live-egress\", not(test)))]";
    let first_gate = transport.find(gate).expect("live-egress gate present");
    let first_reqwest = transport.find("reqwest").expect("reqwest present in transport");
    assert!(first_reqwest > first_gate, "reqwest appears before the live-egress gate");
    // The raw egress permit is consumed only through `into_plan` (backend-only).
    assert!(transport.contains("fn into_plan"));
    // `RawBackend` is SEALED: it has a private-supertrait bound, so no external
    // crate can implement another egress backend.
    assert!(transport.contains("pub trait RawBackend: sealed::Sealed"), "RawBackend is not sealed");
    // The ONLY production `impl RawBackend for` blocks are the production simulator
    // and the feature-gated live backend. Keep counts so duplicate impls cannot hide.
    let mut backends: Vec<&str> = transport
        .match_indices("impl RawBackend for ")
        .map(|(index, _)| {
            let rest = &transport[index + "impl RawBackend for ".len()..];
            rest.split_whitespace().next().unwrap_or("")
        })
        .collect();
    backends.sort_unstable();
    assert_eq!(
        backends,
        ["ProdBackend", "SimBackend"],
        "unexpected RawBackend implementor(s): {backends:?}"
    );

    assert!(
        transport.contains("#[derive(Debug, Default)]\npub struct SimBackend;"),
        "production SimBackend missing or test-gated"
    );
    let sim_impl = transport
        .split_once("impl RawBackend for SimBackend")
        .and_then(|(_, rest)| rest.split_once("#[derive(Debug)]\npub struct RuntimeBackend<'a>"))
        .map(|(body, _)| body)
        .expect("SimBackend impl");
    for forbidden in [
        "reqwest",
        ".send()",
        "std::net",
        "std::fs",
        "std::env",
        "println!",
        "eprintln!",
        "SubmitOutcome::LiveComplete",
    ] {
        assert!(!sim_impl.contains(forbidden), "simulation backend contains `{forbidden}`");
    }
    assert!(sim_impl.contains("SubmitOutcome::Simulated(record)"));
    assert!(!transport.contains("\n    Complete,"), "ambiguous SubmitOutcome::Complete restored");
    assert!(transport.contains("\n    LiveComplete,"));
}

// -- C1: gate-widening / arbitrary-load seams are `#[cfg(test)]` ---------------

/// The RAW source (comments kept) of an arm file, WITHOUT the `#[cfg(test)] mod
/// tests` tail — so seam scans see the production/impl-level items (which carry the
/// `#[cfg(test)]` attribute we check) but never match test-fn NAMES like
/// `open_existing_missing_...`.
fn arm_raw(file: &str) -> String {
    let raw = std::fs::read_to_string(arm_dir().join(file)).expect("arm source");
    production_prefix(&raw, file).unwrap_or_else(|error| panic!("{error}")).to_string()
}

/// Assert every `fn <seam>` DEFINITION in `raw` is `#[cfg(test)]`-gated (the
/// attribute appears within the 200 chars preceding the definition).
fn assert_seam_cfg_test(raw: &str, seam: &str) {
    let needle = format!("fn {seam}");
    let mut found = 0;
    let mut from = 0;
    while let Some(rel) = raw[from..].find(&needle) {
        let index = from + rel;
        let start = index.saturating_sub(200);
        assert!(
            raw[start..index].contains("#[cfg(test)]"),
            "seam `{seam}` is not #[cfg(test)]-gated (real gate/custody bypass reachable in production)"
        );
        found += 1;
        from = index + needle.len();
    }
    assert!(found >= 1, "seam `{seam}` not found (rename? update the seal)");
}

#[test]
fn gate_and_custody_seams_are_test_only() {
    let witness = arm_raw("witness.rs");
    // Gate-widening seams must never be reachable by production/arm-wiring code.
    assert_seam_cfg_test(&witness, "issue_with_gate_for_test");
    assert_seam_cfg_test(&witness, "load_and_sign_with");
    assert_seam_cfg_test(&witness, "with_forced_gate");
    // The `force_gate_open` field is itself `#[cfg(test)]`.
    let start = witness.find("force_gate_open").expect("force_gate_open field");
    assert!(
        witness[start.saturating_sub(200)..start].contains("#[cfg(test)]"),
        "force_gate_open field is not #[cfg(test)]-gated"
    );
    let custody = arm_raw("custody.rs");
    // Arbitrary-path/address key + credential loaders must be test-only.
    assert_seam_cfg_test(&custody, "load_from");
}

#[test]
fn runtime_switch_and_funds_lock_sources_are_pinned() {
    let transport = arm_raw("transport.rs");
    assert!(transport.contains("#[derive(Debug)]\npub struct RuntimeBackend<'a>"));
    assert!(!transport.contains("pub struct RuntimeBackend<'a> {\n    pub"));
    assert!(transport.contains(
        "#[cfg(all(feature = \"arm-live-egress\", not(test)))]\n    pub fn from_explicit_flag"
    ));
    assert!(transport.contains("selection: LiveSelectionProof { private: () }"));
    let live_snapshot = transport
        .split_once("fn from_live_selection")
        .and_then(|(_, rest)| rest.split_once("fn evaluate_live_locks"))
        .map(|(body, _)| body)
        .expect("live snapshot constructor");
    assert!(live_snapshot.contains("explicit_live: true"));
    assert!(live_snapshot.contains("signed_receipt_fresh: freshness.signed_receipt_fresh()"));
    assert!(live_snapshot.contains("kill_clear: freshness.kill_clear()"));
    assert!(!live_snapshot.contains("signed_receipt_fresh: true"));
    assert!(!live_snapshot.contains("kill_clear: true"));

    let witness = arm_raw("witness.rs");
    assert!(witness.contains("pub struct FreshnessProof {\n    signed_receipt: SignedReceiptFresh,\n    kill: KillClear,\n}"));
    assert!(witness.contains(") -> Option<FreshnessProof> {"));
    assert_eq!(
        witness
            .matches("Some(FreshnessProof {\n            signed_receipt: SignedReceiptFresh")
            .count(),
        1
    );
    assert!(!witness.contains("impl Clone for FreshnessProof"));
    assert!(!witness.contains("impl Copy for FreshnessProof"));

    let send_gated = transport
        .split_once("pub fn send_gated")
        .and_then(|(_, rest)| rest.split_once("// -- pure response mapping"))
        .map(|(body, _)| body)
        .expect("send_gated body");
    assert_eq!(
        send_gated.matches("let Some(freshness) = fresh.revalidate").count(),
        2,
        "both initial and retry branches must mint freshness proof"
    );
    assert!(!send_gated.contains("live_requested"));
    assert_eq!(
        transport
            .matches("native_balance_at_latest_committed(super::custody::FUNDED_WALLET)")
            .count(),
        1
    );
    assert_eq!(transport.matches("fresh.armed.hot_wallet_cap_wei()").count(), 1);
    assert!(transport.contains(".ok_or(LiveLockClosed::FundedAccountAbsent)?"));
    assert!(transport.contains(".map_err(|_| LiveLockClosed::FundsUnavailable)?"));

    #[derive(Default)]
    struct LivePermitConstructions(usize);
    impl<'ast> Visit<'ast> for LivePermitConstructions {
        fn visit_expr_struct(&mut self, expression: &'ast syn::ExprStruct) {
            if expression
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "LiveEgressPermit")
            {
                self.0 += 1;
            }
            syn::visit::visit_expr_struct(self, expression);
        }
    }
    let parsed_transport = parse(&transport);
    let mut constructions = LivePermitConstructions::default();
    constructions.visit_file(&parsed_transport);
    assert_eq!(constructions.0, 1, "LiveEgressPermit must have one construction expression");
    assert!(
        transport.contains("#[derive(Debug)]\npub struct LiveEgressPermit {\n    private: (),\n}")
    );
    assert!(!transport.contains("impl Clone for LiveEgressPermit"));
    assert!(!transport.contains("impl Copy for LiveEgressPermit"));

    #[derive(Default)]
    struct SharedSequenceCalls(usize);
    impl<'ast> Visit<'ast> for SharedSequenceCalls {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = call.func.as_ref()
                && path
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "execute_live_sequence")
            {
                self.0 += 1;
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    let helper = parsed_transport
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "execute_live_sequence" => {
                Some(function)
            }
            _ => None,
        })
        .expect("shared live sequence");
    assert!(matches!(helper.vis, syn::Visibility::Inherited));
    assert!(helper.attrs.iter().all(|attribute| !attribute.path().is_ident("cfg")));
    let mut sequence_calls = SharedSequenceCalls::default();
    sequence_calls.visit_file(&parsed_transport);
    assert_eq!(sequence_calls.0, 1, "production must have one shared-sequence call");

    let raw_transport =
        std::fs::read_to_string(arm_dir().join("transport.rs")).expect("raw transport source");
    let parsed_full_transport = parse(&raw_transport);
    let mut all_sequence_calls = SharedSequenceCalls::default();
    all_sequence_calls.visit_file(&parsed_full_transport);
    assert_eq!(
        all_sequence_calls.0, 8,
        "one production plus seven test calls must share the sole sequence"
    );
    let production_backend = transport
        .split_once("impl RawBackend for ProdBackend")
        .and_then(|(_, rest)| rest.split_once("// -- shared live execution sequence"))
        .map(|(body, _)| body)
        .expect("ProdBackend execute body");
    assert_eq!(production_backend.matches("execute_live_sequence(").count(), 1);
    assert!(!production_backend.contains("match egress.into_plan()"));

    let proofs = arm_raw("proofs.rs");
    assert!(proofs.contains(
        "fn native_balance_at_latest_committed(\n        &self,\n        address: Address,\n    ) -> Result<Option<alloy_primitives::U256>, ProviderError>;"
    ));
    let providers = arm_raw("providers.rs");
    assert!(providers.contains(
        "fn native_balance_at_latest_committed(\n        &self,\n        address: Address,\n    ) -> Result<Option<alloy_primitives::U256>, ProviderError>;"
    ));

    let cli_dir = manifest_dir().join("../cli");
    let cli = std::fs::read_to_string(cli_dir.join("src/standard_node.rs")).expect("CLI source");
    let flag_index = cli.find("long = \"mev-live-egress\"").expect("live flag");
    assert_eq!(cli.matches("long = \"mev-live-egress\"").count(), 1);
    let flag_block = &cli[flag_index.saturating_sub(160)..(flag_index + 240).min(cli.len())];
    assert!(flag_block.contains("#[cfg(feature = \"arm-live-egress\")]"));
    assert!(flag_block.contains("default_value_t = false"));
    for forbidden in ["env =", "MEV_LIVE_EGRESS", "default_value_t = true"] {
        assert!(!flag_block.contains(forbidden), "live flag gained forbidden source `{forbidden}`");
    }
    let cli_manifest = std::fs::read_to_string(cli_dir.join("Cargo.toml")).expect("CLI manifest");
    assert!(
        cli_manifest
            .contains("arm-sim = [\n    \"t4e-handoff\",\n    \"mev-trader-submit/arm\",\n]")
    );
    assert!(cli_manifest.contains(
        "arm-live-egress = [\n    \"arm-sim\",\n    \"dep:mev-trader-submit\",\n    \"mev-trader-submit/arm-live-egress\",\n]"
    ));
    let node_manifest =
        std::fs::read_to_string(manifest_dir().join("../../../bin/node/Cargo.toml"))
            .expect("node manifest");
    assert!(node_manifest.contains("arm-sim = [ \"base-execution-cli/arm-sim\" ]"));
    assert!(
        node_manifest
            .contains("arm-live-egress = [ \"arm-sim\", \"base-execution-cli/arm-live-egress\" ]")
    );
}

#[test]
fn production_process_image_open_is_fixed_and_arbitrary_paths_are_test_only() {
    let providers = arm_raw("providers.rs");
    let parsed = parse(&providers);
    let mut inventory = ProductionCallInventory::default();
    syn::visit::visit_file(&mut inventory, &parsed);

    assert!(
        !inventory.function_names.iter().any(|name| name == "from_path"),
        "arbitrary-path process identity constructor reached production"
    );
    let file_opens: Vec<&CallRecord> =
        inventory.calls.iter().filter(|call| call.path.ends_with("File::open")).collect();
    assert_eq!(file_opens.len(), 1, "production must open exactly one process image");
    assert_eq!(
        file_opens[0].string_argument.as_deref(),
        Some("/proc/self/exe"),
        "production process-image open must use the fixed procfs path"
    );
    assert_eq!(
        file_opens[0].arguments,
        vec![None],
        "production process-image open must take a literal, never a caller expression"
    );

    assert_seam_cfg_test(&providers, "from_path");
    let process_methods = inherent_methods(&parsed, "ProcessBinaryIdentity");
    assert_eq!(
        process_methods.iter().map(|method| ident_name(&method.sig.ident)).collect::<Vec<_>>(),
        ["install", "from_open_file", "binary_digest"],
        "production ProcessBinaryIdentity methods must not gain an arbitrary-input seam"
    );
    let from_open_file = inherent_method(&parsed, "ProcessBinaryIdentity", "from_open_file");
    assert!(
        from_open_file.sig.inputs.len() == 1
            && matches!(
                from_open_file.sig.inputs.first(),
                Some(syn::FnArg::Typed(argument))
                    if matches!(
                        &*argument.ty,
                        syn::Type::Path(path)
                            if path.path.segments.last().is_some_and(|segment| segment.ident == "File")
                    )
            ),
        "the private hashing helper may accept only an already-open File"
    );
    let install = inherent_method(&parsed, "ProcessBinaryIdentity", "install");
    assert_eq!(
        fallible_local_name(install.block.stmts.first().expect("install open statement")),
        Some("file".to_owned()),
        "the fixed process image must be opened fallibly before hashing"
    );
}

#[test]
fn production_deployment_validation_precedes_arm_runtime_open_at_install_sink() {
    let providers = parse(&arm_raw("providers.rs"));
    let install = inherent_method(&providers, "ProductionB5Runtime", "install");

    let mut relevant = Vec::new();
    for (statement_index, statement) in install.block.stmts.iter().enumerate() {
        let mut inventory = ProductionCallInventory::default();
        syn::visit::visit_stmt(&mut inventory, statement);
        for call in inventory.calls {
            if call.path.ends_with("ProcessBinaryIdentity::install")
                || call.path.ends_with("ProductionDeploymentIdentitySource::install")
                || call.path.ends_with("ArmRuntime::open")
            {
                relevant.push((statement_index, call));
            }
        }
    }

    assert_eq!(relevant.len(), 3, "production install authority call inventory changed");
    assert!(
        relevant[0].1.path.ends_with("ProcessBinaryIdentity::install")
            && relevant[1].1.path.ends_with("ProductionDeploymentIdentitySource::install")
            && relevant[2].1.path.ends_with("ArmRuntime::open"),
        "process measurement and signed deployment/store validation must precede ArmRuntime::open"
    );
    assert_eq!(
        relevant[1].1.arguments,
        vec![
            Some("evidence".to_owned()),
            Some("process".to_owned()),
            Some("claim_store".to_owned()),
        ],
        "deployment validation must bind signed evidence, measured process, and the opened claim store"
    );
    assert_eq!(
        [
            fallible_local_name(&install.block.stmts[relevant[0].0]),
            fallible_local_name(&install.block.stmts[relevant[1].0]),
            fallible_local_name(&install.block.stmts[relevant[2].0]),
        ],
        [
            Some("process".to_owned()),
            Some("deployment_identity".to_owned()),
            Some("arm".to_owned()),
        ],
        "each authority must fail closed before execution can reach the next install statement"
    );
}

// -- M1: the assembler-only witness is not duplicable --------------------------

#[test]
fn validated_unsigned_atomic_tx_has_no_clone() {
    let source = manifest_dir().join("src");
    let assembler = std::fs::read_to_string(source.join("assembler.rs")).expect("assembler");
    let authority = std::fs::read_to_string(source.join("tx_authority.rs")).expect("authority");
    assert!(
        assembler.contains(
            "#[cfg(feature = \"arm\")]\n#[derive(Debug)]\npub struct LegacyValidatedUnsignedAtomicTx"
        ),
        "legacy arm witness must derive exactly Debug (no Clone/Copy)"
    );
    assert!(
        assembler.contains(
            "#[cfg(feature = \"arm\")]\npub type ValidatedUnsignedAtomicTx = LegacyValidatedUnsignedAtomicTx;"
        ),
        "arm compatibility alias must name the linear legacy witness"
    );
    for body in [&assembler, &authority] {
        assert!(!body.contains("impl Clone for ValidatedUnsignedAtomicTx"));
        assert!(!body.contains("impl Copy for ValidatedUnsignedAtomicTx"));
        assert!(!body.contains("impl Clone for LegacyValidatedUnsignedAtomicTx"));
        assert!(!body.contains("impl Copy for LegacyValidatedUnsignedAtomicTx"));
    }
    assert!(
        authority.contains("pub struct ValidatedUnsignedAtomicTx {")
            && authority.contains("impl Debug for ValidatedUnsignedAtomicTx"),
        "T4b witness must be linear with an explicit redacted Debug implementation"
    );
}

// -- b5: endpoints are compile-pinned in request.rs only -----------------------

#[test]
fn endpoints_are_pinned_in_request_only() {
    let request = arm_production("request.rs");
    assert!(request.contains("http://127.0.0.1:8545"), "base node RPC pin missing");
    assert!(
        request.contains("https://baseauction.blinklabs.xyz/v1/"),
        "blink auction host pin missing"
    );
    // No other arm file carries a URL scheme in real code.
    for (file, body) in all_arm_production() {
        if file == "request.rs" {
            continue;
        }
        assert!(!body.contains("://"), "URL literal outside request.rs: {file}");
    }
}

// -- b6: custody paths pinned + key never escapes ------------------------------

#[test]
fn custody_paths_pinned_and_key_confined() {
    let custody = arm_production("custody.rs");
    assert!(custody.contains("/home/ubuntu/.config/mev-trading-hotwallet"), "hot-wallet path pin");
    assert!(custody.contains("/home/ubuntu/.blink-searcher-key"), "blink credential path pin");
    assert!(
        custody.contains("98e1e2A84557D49496D1BFE31EA7b5a6C59FD0f9"),
        "funded wallet address pin"
    );
    // The signing key never leaves custody: no return/exposed-key shape.
    for escape in [
        "-> SigningKey",
        "-> &SigningKey",
        "Result<SigningKey",
        "pub signing_key",
        "fn signing_key",
    ] {
        assert!(!custody.contains(escape), "hot-wallet key escape shape present: {escape}");
    }
    // No logging surface anywhere in the arm production tree.
    for (file, body) in all_arm_production() {
        for forbidden in ["println!", "eprintln!", "dbg!", "print!", "eprint!"] {
            assert!(!body.contains(forbidden), "logging surface {forbidden} in {file}");
        }
    }
}

// -- arming self-load seal: submit's arm production may not obtain its own armed
//    criteria via `base_mev_trader::production_arming_criteria` --------------------

/// `production_arming_criteria` is a PUBLIC producer in `base_mev_trader` (a submit
/// dependency under `phase-b`), so Rust visibility alone cannot bar a call. The
/// design invariant is that B5 injects the verified value and submit NEVER self-loads
/// its own arming criteria. A token scan over the arm production sources rejects any
/// direct call OR `use …::production_arming_criteria [as alias]` import that would
/// have to name the source identifier here. (Comments and the `#[cfg(test)] mod
/// tests` tail are stripped, so the witness.rs doc reference does not trip this.) The
/// clean-4 alias-bypass path — an import/re-export living in lib/fee/assembler/signer
/// — is closed by the identical check in `capability_seal.rs`.
#[test]
fn arm_source_never_self_loads_production_arming_criteria() {
    for (file, body) in all_arm_production() {
        assert!(
            !body.contains("production_arming_criteria"),
            "arm production source {file} references production_arming_criteria \
             (submit must not self-load arming criteria; B5 injects a verified value)"
        );
    }
}

// -- b8: only reviewed workspace edges link the submit crate (re-run) ----------

fn workspace_metadata() -> serde_json::Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline"])
        .current_dir(manifest_dir().join("../../.."))
        .output()
        .expect("cargo metadata runs");
    assert!(output.status.success(), "cargo metadata failed");
    serde_json::from_slice(&output.stdout).expect("metadata json")
}

#[test]
fn only_reviewed_workspace_edges_link_the_submit_crate() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    let mut linkers = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("name");
        if name == "mev-trader-submit" {
            continue;
        }
        for dependency in package["dependencies"].as_array().expect("dependencies") {
            if dependency["name"] != "mev-trader-submit" {
                continue;
            }
            match name {
                "base-execution-cli" => {
                    assert_eq!(
                        dependency["optional"], true,
                        "CLI submit edge must remain optional"
                    );
                    assert_eq!(dependency["kind"], serde_json::Value::Null);
                    assert_eq!(dependency["features"], serde_json::json!([]));
                    assert_eq!(dependency["rename"], serde_json::Value::Null);
                    assert_eq!(dependency["target"], serde_json::Value::Null);
                }
                "base-suppression-provision-bin" => {
                    assert_eq!(dependency["optional"], true);
                    assert_eq!(dependency["uses_default_features"], true);
                    assert_eq!(dependency["kind"], serde_json::Value::Null);
                    assert_eq!(dependency["rename"], serde_json::Value::Null);
                    assert_eq!(dependency["target"], serde_json::Value::Null);
                    assert_eq!(dependency["features"], serde_json::json!(["arm-provisioning"]));
                }
                other => panic!("unreviewed submit linker: {other}"),
            }
            linkers.push(name);
        }
    }
    linkers.sort_unstable();
    assert_eq!(linkers, ["base-execution-cli", "base-suppression-provision-bin"]);

    let provisioner = packages
        .iter()
        .find(|package| package["name"] == "base-suppression-provision-bin")
        .expect("provisioning package");
    assert_eq!(provisioner["features"]["provision"], serde_json::json!(["dep:mev-trader-submit"]));
    let provisioning_targets = provisioner["targets"].as_array().expect("provisioning targets");
    for name in ["base-mev-suppression-provision", "base-mev-t4e-provision"] {
        let provisioning_bin = provisioning_targets
            .iter()
            .find(|target| target["name"] == name)
            .unwrap_or_else(|| panic!("missing provisioning binary target `{name}`"));
        assert_eq!(provisioning_bin["required-features"], serde_json::json!(["provision"]));
    }

    let cli =
        std::fs::read_to_string(manifest_dir().join("../cli/Cargo.toml")).expect("CLI manifest");
    let t4b = cli
        .split_once("t4b-shadow = [")
        .expect("T4b feature")
        .1
        .split_once(']')
        .expect("closed T4b feature")
        .0;
    assert!(t4b.contains("\"mev-trader-submit/tx-authority\""));
    for forbidden in ["phase-b", "arm", "arm-live-egress", "reqwest", "signer"] {
        assert!(!t4b.contains(forbidden), "T4b CLI edge enables {forbidden}");
    }
}

#[test]
fn workspace_build_keeps_submit_arm_features_disabled() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "-i", "mev-trader-submit", "-e", "features", "--offline"])
        .current_dir(manifest_dir().join("../../.."))
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).expect("cargo tree output");
    assert!(
        tree.contains("mev-trader-submit feature \"default\""),
        "workspace closure no longer contains the expected default-only submit node: {tree}"
    );
    for forbidden in ["arm", "arm-provisioning", "arm-live-egress"] {
        assert!(
            !tree.contains(&format!("mev-trader-submit feature \"{forbidden}\"")),
            "workspace closure activates forbidden submit feature `{forbidden}`:\n{tree}"
        );
    }
}
