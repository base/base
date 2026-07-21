//! Mode-B red-line capability seal for the B3-arm tier (§1.3 b1–b8). Author ≠
//! reviewer: these are machine-checks a reviewer re-runs. Compiles only under
//! `--features arm` (the acceptance lane); absent otherwise.
#![cfg(feature = "arm")]

use std::{collections::BTreeSet, path::PathBuf, process::Command};

/// The exact production files under `src/arm/`. A NEW arm file must be added here
/// (and re-reviewed) before it can ship (b7, fail-closed).
const ARM_FILES: [&str; 8] = [
    "claim.rs",
    "custody.rs",
    "mod.rs",
    "proofs.rs",
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
/// things. Also drops any `#[cfg(test)] mod tests { .. }` tail so test scaffolding
/// (which legitimately signs, loads keys, and drives fakes) is not scanned as
/// production capability.
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

/// Production code of one arm file: comments stripped and the `#[cfg(test)] mod
/// tests` tail removed.
fn arm_production(file: &str) -> String {
    let raw = std::fs::read_to_string(arm_dir().join(file)).expect("arm source");
    let stripped = strip_comments(&raw);
    match stripped.find("#[cfg(test)]\nmod tests") {
        Some(index) => stripped[..index].to_string(),
        None => stripped,
    }
}

/// The testkit lives inside `mod.rs` under `#[cfg(test)]`; drop it too for scans.
fn arm_production_mod() -> String {
    let raw = std::fs::read_to_string(arm_dir().join("mod.rs")).expect("arm mod");
    let stripped = strip_comments(&raw);
    match stripped.find("#[cfg(test)]\npub(crate) mod testkit") {
        Some(index) => stripped[..index].to_string(),
        None => stripped,
    }
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

/// The complete curated forward-B5 public API re-exported from `lib.rs`. Any change
/// to the exported surface must update this allowlist (and be re-reviewed). No
/// low-level injection API (arbitrary suppression paths, fixture source impls,
/// request/custody internals) may appear here.
const PUBLIC_API_ALLOWLIST: [&str; 36] = [
    "ArmError",
    "ArmRuntime",
    "ArmedFailSink",
    "AttributionRetryToken",
    "AuthorizedCandidate",
    "AuthorizedSignedSubmission",
    "CHAIN_ID_BASE",
    "Channel",
    "CheckedCandidate",
    "CodeHashProvider",
    "DeploymentEvidence",
    "DeploymentIdentity",
    "DeploymentIdentitySource",
    "DeploymentPayload",
    "DrawdownSource",
    "EgressPlan",
    "FreshnessSources",
    "G7Attestation",
    "G7Payload",
    "LiveRunAttestation",
    "LiveRunPayload",
    "PairedSubmission",
    "ProdBackend",
    "ProofBindings",
    "ProviderError",
    "provision_suppression_anchor",
    "RawBackend",
    "RawEgress",
    "RequestSpec",
    "SubmissionAttempt",
    "SubmitOutcome",
    "SubmitSuppressionClear",
    "SuppressionRollbackError",
    "ValidatedExecutionIdentity",
    "send_gated",
    "try_claim_arm",
];

fn lib_source() -> String {
    std::fs::read_to_string(manifest_dir().join("src").join("lib.rs")).expect("lib.rs")
}

// -- AST-based seal primitives (syn) — robust against the evasion vectors -------

/// Parse Rust source (syn keeps `#[cfg(test)]` items with their attributes and
/// does not evaluate `cfg`, so seams stay visible for inspection).
fn parse(src: &str) -> syn::File {
    syn::parse_file(src).expect("parse rust source")
}

/// Vector 1: the named `mod` item is present AND declared with NO visibility
/// modifier (plain `mod X;`). Returns `false` for a missing module OR any of
/// `pub mod` / `pub(crate) mod` / `pub(...) mod` (only `Visibility::Inherited`
/// passes).
fn mod_is_private(src: &str, name: &str) -> bool {
    parse(src).items.iter().any(|item| {
        matches!(item, syn::Item::Mod(module)
            if module.ident == name && matches!(module.vis, syn::Visibility::Inherited))
    })
}

/// A fully-flattened `use` leaf: the full SOURCE path segments (before crate/self
/// normalization) plus either the PUBLIC exported name (the `as` alias if present,
/// else the leaf) or a glob marker.
#[derive(Debug)]
enum UseLeaf {
    Item { source_path: Vec<String>, public_name: String },
    Glob { source_path: Vec<String> },
}

/// Vectors 1+2: fully flatten a `use` tree into leaves, recursing through `Group`
/// at ANY position (leading, nested, trailing), `Path`, `Name`, `Rename`, `Glob`.
fn flatten_use(tree: &syn::UseTree, prefix: &[String], out: &mut Vec<UseLeaf>) {
    match tree {
        syn::UseTree::Name(name) => {
            let mut path = prefix.to_vec();
            path.push(name.ident.to_string());
            out.push(UseLeaf::Item { source_path: path, public_name: name.ident.to_string() });
        }
        syn::UseTree::Rename(rename) => {
            let mut path = prefix.to_vec();
            path.push(rename.ident.to_string());
            // The EXPORTED name is the `as` alias.
            out.push(UseLeaf::Item { source_path: path, public_name: rename.rename.to_string() });
        }
        syn::UseTree::Path(inner) => {
            let mut path = prefix.to_vec();
            path.push(inner.ident.to_string());
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

/// Strip leading `crate` / `self` / `super` path segments (normalization).
fn strip_crate_self(path: &[String]) -> &[String] {
    let mut start = 0;
    while start < path.len() && matches!(path[start].as_str(), "crate" | "self" | "super") {
        start += 1;
    }
    &path[start..]
}

/// Vector 2: collect the arm re-export surface across EVERY `pub use` leaf. Each
/// arm-rooted leaf must be a DIRECT `arm::<name>` (rejecting `arm::<sub>::…` deeper
/// paths and globs). Returns `(source_names, public_names)` — the source item AND
/// the exported (alias-aware) name — or `Err` on a glob / non-direct path.
fn arm_reexports(src: &str) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let file = parse(src);
    let mut source = BTreeSet::new();
    let mut public = BTreeSet::new();
    for item in &file.items {
        let syn::Item::Use(use_item) = item else {
            continue;
        };
        if !matches!(use_item.vis, syn::Visibility::Public(_)) {
            continue;
        }
        let mut leaves = Vec::new();
        flatten_use(&use_item.tree, &[], &mut leaves);
        for leaf in leaves {
            match leaf {
                UseLeaf::Glob { source_path } => {
                    if strip_crate_self(&source_path).first().map(String::as_str) == Some("arm") {
                        return Err(format!("glob re-export into arm: {source_path:?}"));
                    }
                }
                UseLeaf::Item { source_path, public_name } => {
                    let norm = strip_crate_self(&source_path);
                    if norm.first().map(String::as_str) != Some("arm") {
                        continue; // not an arm re-export
                    }
                    // Must be EXACTLY `arm::<name>` — never `arm::<submodule>::…`.
                    if norm.len() != 2 {
                        return Err(format!("non-direct arm re-export path: {norm:?}"));
                    }
                    source.insert(norm[1].clone());
                    public.insert(public_name);
                }
            }
        }
    }
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

// -- b9: facade — private module + private sub-modules -------------------------

#[test]
fn arm_module_is_private() {
    // Rejects `pub mod arm;` AND `pub(crate) mod arm;` AND `pub(...) mod arm;`.
    assert!(
        mod_is_private(&lib_source(), "arm"),
        "the `arm` module must be a plain `mod arm;` (no visibility modifier)"
    );
}

#[test]
fn arm_submodules_are_private() {
    let modrs = std::fs::read_to_string(arm_dir().join("mod.rs")).expect("mod.rs");
    // Rejects `pub mod <sub>;` AND `pub(crate) mod <sub>;` for every real sub-module.
    // (The `#[cfg(test)] pub(crate) mod testkit` is a test utility and is NOT scanned.)
    for sub in ["claim", "custody", "proofs", "request", "suppression", "transport", "witness"] {
        assert!(
            mod_is_private(&modrs, sub),
            "arm sub-module `{sub}` must be a plain `mod {sub};` (no visibility modifier)"
        );
    }
}

// -- b10: exported surface == curated allowlist (normalized, glob-rejecting) ----

#[test]
fn public_api_surface_is_exactly_the_curated_allowlist() {
    let (source, public) = arm_reexports(&lib_source()).expect("no glob/deep arm re-export");
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
}

// -- b11: injection-critical types' FULL non-test method surface is allowlisted --

/// The reviewed, exact non-`#[cfg(test)]` public/`pub(crate)` inherent-method
/// surface of the injection-critical types. Adding ANY non-test method (constructor,
/// mutator, or path/source setter) to these types must update this allowlist.
fn arm_runtime_methods() -> BTreeSet<String> {
    ["open", "sink", "suppression_clear", "freshness"].iter().map(|s| (*s).to_string()).collect()
}
fn freshness_sources_methods() -> BTreeSet<String> {
    ["revalidate"].iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn constructor_surface_is_sealed() {
    let witness = std::fs::read_to_string(arm_dir().join("witness.rs")).expect("witness.rs");
    // `ArmRuntime`: only `open` (pins stores internally), `freshness` (takes ONLY
    // keyless node providers), `sink`, `suppression_clear` are non-test surface. A
    // new `from_path`/`with_store`/`set_path` mutator would appear here and FAIL.
    assert_eq!(
        inherent_method_surface(&witness, "ArmRuntime"),
        arm_runtime_methods(),
        "ArmRuntime non-#[cfg(test)] method surface changed — review before allowlisting"
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
    let definitions: usize =
        all_arm_production().iter().map(|(_file, body)| body.matches("fn send_gated").count()).sum();
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
    assert!(
        transport.contains("pub trait RawBackend: sealed::Sealed"),
        "RawBackend is not sealed"
    );
    // The ONLY `impl RawBackend for` blocks are ProdBackend (gated) and FakeBackend
    // (test) — no third egress backend.
    let backends: BTreeSet<&str> = transport
        .match_indices("impl RawBackend for ")
        .map(|(index, _)| {
            let rest = &transport[index + "impl RawBackend for ".len()..];
            rest.split_whitespace().next().unwrap_or("")
        })
        .collect();
    assert_eq!(
        backends,
        BTreeSet::from(["ProdBackend", "FakeBackend"]),
        "unexpected RawBackend implementor(s): {backends:?}"
    );
}

// -- C1: gate-widening / arbitrary-load seams are `#[cfg(test)]` ---------------

/// The RAW source (comments kept) of an arm file, WITHOUT the `#[cfg(test)] mod
/// tests` tail — so seam scans see the production/impl-level items (which carry the
/// `#[cfg(test)]` attribute we check) but never match test-fn NAMES like
/// `open_existing_missing_...`.
fn arm_raw(file: &str) -> String {
    let raw = std::fs::read_to_string(arm_dir().join(file)).expect("arm source");
    match raw.find("#[cfg(test)]\nmod tests") {
        Some(index) => raw[..index].to_string(),
        None => raw,
    }
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
    assert_seam_cfg_test(&witness, "issue_checked");
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

// -- M1: the assembler-only witness is not duplicable --------------------------

#[test]
fn validated_unsigned_atomic_tx_has_no_clone() {
    let assembler =
        std::fs::read_to_string(manifest_dir().join("src").join("assembler.rs")).expect("assembler");
    // The struct derives EXACTLY `Debug` (no Clone/Copy): duplicating the linear
    // witness would let one validated tx bind to two candidates.
    assert!(
        assembler.contains(
            "#[cfg(feature = \"arm\")]\n#[derive(Debug)]\npub struct ValidatedUnsignedAtomicTx"
        ),
        "ValidatedUnsignedAtomicTx must derive exactly Debug (no Clone/Copy)"
    );
    assert!(
        !assembler.contains("impl Clone for ValidatedUnsignedAtomicTx"),
        "ValidatedUnsignedAtomicTx Clone impl present"
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
    for escape in ["-> SigningKey", "-> &SigningKey", "Result<SigningKey", "pub signing_key", "fn signing_key"] {
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

// -- b8: no workspace crate links the submit crate (re-run) --------------------

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
fn no_workspace_crate_links_the_submit_crate() {
    let metadata = workspace_metadata();
    let packages = metadata["packages"].as_array().expect("packages");
    for package in packages {
        let name = package["name"].as_str().expect("name");
        if name == "mev-trader-submit" {
            continue;
        }
        for dependency in package["dependencies"].as_array().expect("dependencies") {
            assert_ne!(
                dependency["name"], "mev-trader-submit",
                "{name} depends on mev-trader-submit — it could reach the node binary"
            );
        }
    }
}
