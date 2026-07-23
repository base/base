//! B5-1a `presign` public-API visibility seal.
//!
//! Plan verification lane: `cargo test -p mev-trader-submit --no-default-features
//! --features presign --test b5_public_api_seal --locked --frozen`.
//!
//! Machine-checks that the dormant tier exposes exactly the reviewed value/digest
//! surface and nothing else:
//!
//! * the pure builder keeps the exact eight-scalar signature — argument types AND
//!   order are sealed by coercing to a fixed fn-pointer shape and by feeding a
//!   distinct bit pattern into every position and reading each back through its
//!   own accessor;
//! * the three domain constants are the exact reviewed literals, pairwise
//!   distinct, and satisfy the framing precondition (non-empty ASCII, no NUL);
//! * both error enums stay closed (the wildcard-free matches below fail
//!   compilation on any variant addition or removal) and their Display/Debug/
//!   source output is redacted to the failure class — never a value, path,
//!   digest, byte, or OS message;
//! * every exported type is a plain `Copy + Eq` value type — forgeable and
//!   non-authorizing by construction;
//! * at source level the `dormant` module stays private and `presign`-gated, the
//!   crate root re-exports exactly the seven reviewed names, and the module
//!   imports nothing beyond the exact `{alloy-primitives, sha2}` direct
//!   dependency allowlist — no std, no file/env/embedded-bytes capability.
#![cfg(feature = "presign")]

use core::error::Error;
use std::{collections::BTreeSet, fs, path::Path};

use alloy_primitives::B256;
use mev_trader_submit::{
    AuthenticatedProvisioningSnapshot, B5_DEPLOYMENT_REVIEW_DOMAIN, B5_DORMANT_PROVISIONING_DOMAIN,
    B5_PROVISIONING_VALUE_SET_DOMAIN, DigestFramingError, DomainSeparatedSha256,
    ProvisioningSnapshotError,
};
use syn::{GenericArgument, ImplItem, Item, PathArguments, Type, Visibility, visit::Visit};

/// Sealed shape of the pure eight-scalar builder. The argument order in this
/// alias is normative: chain id, N10 manifest, N8 value set, N4 evidence,
/// N0 source commit, N2 release artifact, N6 review file, N7 review digest.
type EightScalarBuilder =
    fn(
        u64,
        B256,
        B256,
        B256,
        [u8; 20],
        B256,
        B256,
        B256,
    ) -> Result<AuthenticatedProvisioningSnapshot, ProvisioningSnapshotError>;

/// The sealed builder entry point; any signature/type/order drift fails this
/// coercion at compile time.
const EIGHT_SCALAR_BUILDER: EightScalarBuilder =
    AuthenticatedProvisioningSnapshot::from_authenticated_bindings;

/// Sealed shape of the sole framed-digest entry point.
type FramedDigest = fn(&str, &[u8]) -> Result<B256, DigestFramingError>;

/// The sealed digest entry point.
const FRAMED_DIGEST: FramedDigest = DomainSeparatedSha256::digest;

/// Sealed `B256` accessor set, in builder-argument order (manifest, value set,
/// evidence, release, review file, review digest). Exactly one accessor per
/// retained 32-byte scalar; each returns `B256`, never a merged or re-encoded
/// value.
const B256_SCALAR_ACCESSORS: [fn(&AuthenticatedProvisioningSnapshot) -> B256; 6] = [
    AuthenticatedProvisioningSnapshot::manifest_sha256,
    AuthenticatedProvisioningSnapshot::value_set_sha256,
    AuthenticatedProvisioningSnapshot::deployment_evidence_sha256,
    AuthenticatedProvisioningSnapshot::release_artifact_sha256,
    AuthenticatedProvisioningSnapshot::deployment_review_file_sha256,
    AuthenticatedProvisioningSnapshot::deployment_review_digest,
];

/// Sealed chain-id accessor.
const CHAIN_ID_ACCESSOR: fn(&AuthenticatedProvisioningSnapshot) -> u64 =
    AuthenticatedProvisioningSnapshot::chain_id;

/// Sealed source-commit accessor: N0 stays a raw 20-byte commit, retained
/// separately — never widened, hex-wrapped, or combined with N2.
const SOURCE_COMMIT_ACCESSOR: fn(&AuthenticatedProvisioningSnapshot) -> [u8; 20] =
    AuthenticatedProvisioningSnapshot::source_commit;

/// Sealed recomputed-N11 accessor.
const SNAPSHOT_DIGEST_ACCESSOR: fn(&AuthenticatedProvisioningSnapshot) -> B256 =
    AuthenticatedProvisioningSnapshot::snapshot_digest;

/// The exact reviewed re-export set of the private `dormant` module.
const EXPECTED_EXPORTS: [&str; 7] = [
    "AuthenticatedProvisioningSnapshot",
    "B5_DEPLOYMENT_REVIEW_DOMAIN",
    "B5_DORMANT_PROVISIONING_DOMAIN",
    "B5_PROVISIONING_VALUE_SET_DOMAIN",
    "DigestFramingError",
    "DomainSeparatedSha256",
    "ProvisioningSnapshotError",
];

/// The exact import allowlist of the dormant module source: the two direct
/// `presign` dependencies plus the test-module self-import. Any other `use`
/// line (std, fs, env, net, …) breaks the no-I/O seal.
const ALLOWED_DORMANT_IMPORTS: [&str; 3] =
    ["use alloy_primitives::{B256, hex};", "use sha2::{Digest, Sha256};", "use super::*;"];

/// Proof that a sealed export is a plain, freely copyable, comparable value
/// type (forgeable, carrying no authority or resource).
fn assert_plain_value<T: Copy + Eq + core::fmt::Debug>(value: T) {
    let duplicate = value;
    assert_eq!(duplicate, value);
}

/// Wildcard-free Display oracle for the closed framing-error surface; adding
/// or removing a variant fails compilation here.
const fn digest_framing_display(error: DigestFramingError) -> &'static str {
    match error {
        DigestFramingError::InvalidDomain => "digest domain must be non-empty ASCII without NUL",
        DigestFramingError::PayloadLengthOverflow => {
            "digest payload length exceeds the u32 framing limit"
        }
    }
}

/// Wildcard-free Display oracle for the closed builder-error surface.
const fn provisioning_display(error: ProvisioningSnapshotError) -> &'static str {
    match error {
        ProvisioningSnapshotError::UnsupportedChainId => {
            "provisioning snapshot chain id is not the supported chain"
        }
        ProvisioningSnapshotError::PayloadFraming(_) => {
            "provisioning snapshot payload framing failed"
        }
    }
}

/// A redacted diagnostic exposes only the failure class: no hex prefix, no
/// path separator, no `key: value` structure.
fn assert_redacted(rendered: &str) {
    assert!(!rendered.contains("0x"), "diagnostic leaked hex material: {rendered}");
    assert!(!rendered.contains('/'), "diagnostic leaked a path separator: {rendered}");
    assert!(!rendered.contains(':'), "diagnostic leaked structured data: {rendered}");
}

#[test]
fn builder_maps_each_sealed_argument_position_to_its_own_accessor() {
    let snapshot = EIGHT_SCALAR_BUILDER(
        8453,
        B256::repeat_byte(0xa1),
        B256::repeat_byte(0xa2),
        B256::repeat_byte(0xa3),
        [0xa4; 20],
        B256::repeat_byte(0xa5),
        B256::repeat_byte(0xa6),
        B256::repeat_byte(0xa7),
    )
    .expect("chain 8453 with structurally admissible scalars must build");

    assert_eq!(CHAIN_ID_ACCESSOR(&snapshot), 8453);
    assert_eq!(SOURCE_COMMIT_ACCESSOR(&snapshot), [0xa4; 20]);
    // The distinct per-position patterns must come back out of exactly the
    // accessor sealed to that position: a swapped argument or accessor is a
    // mismatch here, not a silent re-mapping.
    let expected = [0xa1, 0xa2, 0xa3, 0xa5, 0xa6, 0xa7].map(B256::repeat_byte);
    for (accessor, pattern) in B256_SCALAR_ACCESSORS.iter().zip(expected) {
        assert_eq!(accessor(&snapshot), pattern);
    }
    assert_ne!(SNAPSHOT_DIGEST_ACCESSOR(&snapshot), B256::ZERO);
}

#[test]
fn framed_digest_entry_point_keeps_its_sealed_signature() {
    let digest = FRAMED_DIGEST(B5_DEPLOYMENT_REVIEW_DOMAIN, b"seal")
        .expect("a valid domain and payload must hash");
    assert_ne!(digest, B256::ZERO);
}

#[test]
fn domain_constants_are_the_exact_reviewed_literals() {
    assert_eq!(B5_DORMANT_PROVISIONING_DOMAIN, "base-mev:b5-dormant:provisioning:v1");
    assert_eq!(B5_PROVISIONING_VALUE_SET_DOMAIN, "base-mev:b5-provisioning:value-set:v1");
    assert_eq!(B5_DEPLOYMENT_REVIEW_DOMAIN, "base-mev:b5-deployment-review:v1");

    let domains = [
        B5_DORMANT_PROVISIONING_DOMAIN,
        B5_PROVISIONING_VALUE_SET_DOMAIN,
        B5_DEPLOYMENT_REVIEW_DOMAIN,
    ];
    for (index, domain) in domains.iter().enumerate() {
        assert!(
            domain.is_ascii() && !domain.contains('\0'),
            "domain must satisfy the framing precondition: {domain}"
        );
        for other in &domains[index + 1..] {
            assert_ne!(domain, other, "digest domains must be pairwise distinct");
        }
    }
}

#[test]
fn exported_types_are_plain_forgeable_value_types() {
    let snapshot = EIGHT_SCALAR_BUILDER(
        8453,
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
        [0; 20],
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
    )
    .expect("the all-zero pattern is structurally admissible");
    assert_plain_value(snapshot);
    // The digest type is a stateless unit struct: constructible bare, holding
    // no configuration, key, or resource.
    assert_plain_value(DomainSeparatedSha256);
    assert_plain_value(DigestFramingError::InvalidDomain);
    assert_plain_value(ProvisioningSnapshotError::UnsupportedChainId);
}

#[test]
fn framing_errors_are_closed_and_redacted() {
    let observed = FRAMED_DIGEST("", b"payload").expect_err("an empty domain must fail closed");
    assert_eq!(observed, DigestFramingError::InvalidDomain);

    for error in [DigestFramingError::InvalidDomain, DigestFramingError::PayloadLengthOverflow] {
        assert_eq!(error.to_string(), digest_framing_display(error));
        assert!(error.source().is_none(), "a framing failure class has no deeper source");
        assert_redacted(&error.to_string());
        assert_redacted(&format!("{error:?}"));
    }
}

#[test]
fn snapshot_errors_are_closed_convertible_and_redacted() {
    let unsupported = EIGHT_SCALAR_BUILDER(
        8454,
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
        [0; 20],
        B256::ZERO,
        B256::ZERO,
        B256::ZERO,
    )
    .expect_err("a non-8453 chain id must fail closed before hashing");
    assert_eq!(unsupported, ProvisioningSnapshotError::UnsupportedChainId);
    assert!(unsupported.source().is_none(), "the chain-id class has no deeper source");

    let converted = ProvisioningSnapshotError::from(DigestFramingError::PayloadLengthOverflow);
    assert_eq!(
        converted,
        ProvisioningSnapshotError::PayloadFraming(DigestFramingError::PayloadLengthOverflow)
    );
    let source = converted.source().expect("payload framing keeps its failure class as source");
    assert_eq!(
        source.to_string(),
        digest_framing_display(DigestFramingError::PayloadLengthOverflow)
    );

    for error in [unsupported, converted] {
        assert_eq!(error.to_string(), provisioning_display(error));
        assert_redacted(&error.to_string());
        assert_redacted(&format!("{error:?}"));
    }
}

#[test]
fn dormant_module_is_private_with_minimum_exports_and_exact_import_allowlist() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lib_source =
        fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("crate root must be readable");

    assert!(
        lib_source.contains("#![forbid(unsafe_code)]"),
        "the crate-wide unsafe forbid seal must stay"
    );
    assert_eq!(
        lib_source.matches("mod dormant;").count(),
        1,
        "exactly one dormant module declaration"
    );
    assert!(
        lib_source.contains("#[cfg(feature = \"presign\")]\nmod dormant;"),
        "the dormant module must stay private and presign-gated"
    );
    assert!(!lib_source.contains("pub mod dormant"), "the dormant module must never become public");

    assert_eq!(
        lib_source.matches("pub use dormant::").count(),
        1,
        "exactly one dormant re-export list"
    );
    let export_block = lib_source
        .split_once("pub use dormant::{")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("};"))
        .map(|(block, _)| block)
        .expect("the dormant re-export list must be a single braced group");
    let exported: BTreeSet<&str> =
        export_block.split(',').map(str::trim).filter(|name| !name.is_empty()).collect();
    let expected: BTreeSet<&str> = EXPECTED_EXPORTS.into_iter().collect();
    assert_eq!(exported, expected, "crate root must re-export exactly the reviewed dormant names");

    let dormant_source = fs::read_to_string(manifest_dir.join("src/dormant.rs"))
        .expect("dormant module source must be readable");
    for line in dormant_source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("use ") {
            assert!(
                ALLOWED_DORMANT_IMPORTS.contains(&trimmed),
                "import outside the presign dependency allowlist: {trimmed}"
            );
        }
    }
    assert!(!dormant_source.contains("pub(crate)"), "no pub(crate) items in the dormant module");
    assert!(!dormant_source.contains("pub mod"), "no nested public modules in the dormant module");
    for capability in ["std::", "include_bytes!(", "include_str!(", "env!("] {
        assert!(
            !dormant_source.contains(capability),
            "dormant module must stay free of the {capability} capability"
        );
    }
}

#[derive(Default)]
struct CapabilityVisitor {
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for CapabilityVisitor {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.segments.first().is_some_and(|segment| segment.ident == "std") {
            self.violations.push("std path".to_string());
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        self.violations.push(format!("extern crate {}", item.ident));
    }

    fn visit_macro(&mut self, item: &'ast syn::Macro) {
        if item.path.segments.last().is_some_and(|segment| {
            matches!(
                segment.ident.to_string().as_str(),
                "env" | "option_env" | "include_bytes" | "include_str"
            )
        }) {
            self.violations
                .push(format!("capability macro {}", item.path.segments.last().unwrap().ident));
        }
        syn::visit::visit_macro(self, item);
    }
}

fn public_ident(visibility: &Visibility, ident: &syn::Ident) -> Option<String> {
    matches!(visibility, Visibility::Public(_)).then(|| ident.to_string())
}

fn type_name(item: &Type) -> Option<String> {
    let Type::Path(path) = item else {
        return None;
    };
    path_segment_name(path.path.segments.last()?)
}

fn path_segment_name(segment: &syn::PathSegment) -> Option<String> {
    let mut name = segment.ident.to_string();
    match &segment.arguments {
        PathArguments::None => {}
        PathArguments::AngleBracketed(arguments) => {
            let type_arguments: Option<Vec<_>> = arguments
                .args
                .iter()
                .map(|argument| match argument {
                    GenericArgument::Type(argument) => type_name(argument),
                    _ => None,
                })
                .collect();
            name.push('<');
            name.push_str(&type_arguments?.join(","));
            name.push('>');
        }
        PathArguments::Parenthesized(_) => return None,
    }
    Some(name)
}

fn inherent_impl_name(item: &syn::ItemImpl) -> Option<String> {
    item.trait_.is_none().then(|| type_name(item.self_ty.as_ref())).flatten()
}

fn trait_impl_name(item: &syn::ItemImpl) -> Option<String> {
    let (_, trait_path, _) = item.trait_.as_ref()?;
    let trait_name = path_segment_name(trait_path.segments.last()?)?;
    Some(format!("{trait_name} for {}", type_name(item.self_ty.as_ref())?))
}

#[test]
fn dormant_ast_has_exact_public_surface_and_no_hidden_capability_path() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(manifest_dir.join("src/dormant.rs"))
        .expect("dormant module source must be readable");
    let syntax = syn::parse_file(&source).expect("dormant module must parse as Rust");

    let mut public_items = BTreeSet::new();
    let mut public_methods = BTreeSet::new();
    let mut enum_variants = std::collections::BTreeMap::<String, BTreeSet<String>>::new();
    let mut trait_impls = BTreeSet::new();

    for item in &syntax.items {
        match item {
            Item::Const(item) => {
                if let Some(name) = public_ident(&item.vis, &item.ident) {
                    public_items.insert(name);
                }
            }
            Item::Struct(item) => {
                if let Some(name) = public_ident(&item.vis, &item.ident) {
                    assert!(
                        item.fields.iter().all(|field| !matches!(field.vis, Visibility::Public(_))),
                        "public fields would widen the forgeable value surface"
                    );
                    public_items.insert(name);
                }
            }
            Item::Enum(item) => {
                if let Some(name) = public_ident(&item.vis, &item.ident) {
                    enum_variants.insert(
                        name.clone(),
                        item.variants.iter().map(|variant| variant.ident.to_string()).collect(),
                    );
                    public_items.insert(name);
                }
            }
            Item::Impl(item) => {
                if let Some(type_name) = inherent_impl_name(item) {
                    for member in &item.items {
                        match member {
                            ImplItem::Fn(method) if matches!(method.vis, Visibility::Public(_)) => {
                                public_methods.insert(format!("{type_name}::{}", method.sig.ident));
                            }
                            ImplItem::Const(item) if matches!(item.vis, Visibility::Public(_)) => {
                                panic!(
                                    "public associated const widens the sealed API: \
                                     {type_name}::{}",
                                    item.ident
                                );
                            }
                            ImplItem::Type(item) if matches!(item.vis, Visibility::Public(_)) => {
                                panic!(
                                    "public associated type widens the sealed API: \
                                     {type_name}::{}",
                                    item.ident
                                );
                            }
                            _ => {}
                        }
                    }
                } else if let Some(name) = trait_impl_name(item) {
                    trait_impls.insert(name);
                } else {
                    panic!("unclassifiable impl widens the sealed API");
                }
            }
            Item::Fn(item) if matches!(item.vis, Visibility::Public(_)) => {
                panic!("bare public function widens the sealed API: {}", item.sig.ident);
            }
            Item::Mod(item) if item.ident == "tests" => {}
            Item::ExternCrate(item) => panic!("extern crate is forbidden: {}", item.ident),
            _ => {}
        }
    }

    let expected_items: BTreeSet<String> = [
        "AuthenticatedProvisioningSnapshot",
        "B5_DEPLOYMENT_REVIEW_DOMAIN",
        "B5_DORMANT_PROVISIONING_DOMAIN",
        "B5_PROVISIONING_VALUE_SET_DOMAIN",
        "DigestFramingError",
        "DomainSeparatedSha256",
        "ProvisioningSnapshotError",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(public_items, expected_items);

    let expected_methods: BTreeSet<String> = [
        "AuthenticatedProvisioningSnapshot::chain_id",
        "AuthenticatedProvisioningSnapshot::deployment_evidence_sha256",
        "AuthenticatedProvisioningSnapshot::deployment_review_digest",
        "AuthenticatedProvisioningSnapshot::deployment_review_file_sha256",
        "AuthenticatedProvisioningSnapshot::from_authenticated_bindings",
        "AuthenticatedProvisioningSnapshot::manifest_sha256",
        "AuthenticatedProvisioningSnapshot::release_artifact_sha256",
        "AuthenticatedProvisioningSnapshot::snapshot_digest",
        "AuthenticatedProvisioningSnapshot::source_commit",
        "AuthenticatedProvisioningSnapshot::value_set_sha256",
        "DomainSeparatedSha256::digest",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(public_methods, expected_methods);
    let expected_trait_impls: BTreeSet<String> = [
        "Display for DigestFramingError",
        "Error for DigestFramingError",
        "Display for ProvisioningSnapshotError",
        "Error for ProvisioningSnapshotError",
        "From<DigestFramingError> for ProvisioningSnapshotError",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(trait_impls, expected_trait_impls);

    assert_eq!(
        enum_variants["DigestFramingError"],
        ["InvalidDomain", "PayloadLengthOverflow"].into_iter().map(str::to_string).collect()
    );
    assert_eq!(
        enum_variants["ProvisioningSnapshotError"],
        ["PayloadFraming", "UnsupportedChainId"].into_iter().map(str::to_string).collect()
    );

    let mut visitor = CapabilityVisitor::default();
    visitor.visit_file(&syntax);
    assert!(visitor.violations.is_empty(), "hidden capability paths: {:?}", visitor.violations);

    let lib_source =
        fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("crate root must be readable");
    let lib_syntax = syn::parse_file(&lib_source).expect("crate root must parse as Rust");
    let dormant_reexports: Vec<&syn::ItemUse> = lib_syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Use(item)
                if matches!(item.vis, Visibility::Public(_))
                    && matches!(&item.tree, syn::UseTree::Path(path) if path.ident == "dormant") =>
            {
                Some(item)
            }
            _ => None,
        })
        .collect();
    assert_eq!(dormant_reexports.len(), 1, "exactly one public dormant re-export is allowed");
    let syn::UseTree::Path(path) = &dormant_reexports[0].tree else {
        unreachable!();
    };
    let syn::UseTree::Group(group) = path.tree.as_ref() else {
        panic!("dormant re-export must be one closed braced group");
    };
    let ast_exports: BTreeSet<String> = group
        .items
        .iter()
        .map(|item| match item {
            syn::UseTree::Name(name) => name.ident.to_string(),
            _ => panic!("aliases, globs, and nested export paths are forbidden"),
        })
        .collect();
    assert_eq!(ast_exports, expected_items);
}
