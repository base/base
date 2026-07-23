//! Privacy seal proving no privileged consumer can reach the P1 helper.
#![cfg(feature = "b5-dormant-presign")]

use std::collections::BTreeSet;

const LIB_SOURCE: &str = include_str!("../src/lib.rs");
const MEV_TRADER_SOURCE: &str = include_str!("../src/mev_trader.rs");
const B5_DORMANT_SOURCE: &str = include_str!("../src/mev_trader/b5_dormant.rs");

const PRIVILEGED_SYMBOLS: [&str; 4] = [
    "VerifiedProvisioningManifestBinding",
    "CommitBReviewedProvisioningBinding",
    "B5ProvisioningBindingError",
    "verify_provisioning_bindings_against",
];

fn source_code(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut code = String::with_capacity(source.len());
    let mut index = 0;
    let mut block_depth = 0usize;
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut escaped = false;

    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if in_line_comment {
            if byte == b'\n' {
                in_line_comment = false;
                code.push('\n');
            } else {
                code.push(' ');
            }
        } else if block_depth != 0 {
            if byte == b'/' && next == Some(b'*') {
                block_depth += 1;
                code.push_str("  ");
                index += 1;
            } else if byte == b'*' && next == Some(b'/') {
                block_depth -= 1;
                code.push_str("  ");
                index += 1;
            } else if byte == b'\n' {
                code.push('\n');
            } else {
                code.push(' ');
            }
        } else if in_string {
            if byte == b'\n' {
                code.push('\n');
            } else {
                code.push(' ');
            }
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'/' && next == Some(b'/') {
            in_line_comment = true;
            code.push_str("  ");
            index += 1;
        } else if byte == b'/' && next == Some(b'*') {
            block_depth = 1;
            code.push_str("  ");
            index += 1;
        } else if byte == b'"' {
            in_string = true;
            code.push(' ');
        } else {
            code.push(char::from(byte));
        }
        index += 1;
    }

    assert_eq!(block_depth, 0, "unterminated block comment in inspected source");
    assert!(!in_string, "unterminated string in inspected source");
    code
}

fn production_code(source: &str) -> String {
    let code = source_code(source);
    code.split("#[cfg(test)]").next().expect("source prefix").to_owned()
}

fn occurrences(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn normalized(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn struct_field_visibilities<'a>(
    source: &'a str,
    struct_name: &str,
) -> BTreeSet<(&'a str, &'a str)> {
    let declaration = format!("struct {struct_name}");
    let declarations: Vec<_> = source.match_indices(&declaration).collect();
    assert_eq!(declarations.len(), 1, "expected exactly one declaration for `{struct_name}`");
    let declaration_start = declarations[0].0;
    let body_start = source[declaration_start..]
        .find('{')
        .map(|offset| declaration_start + offset)
        .unwrap_or_else(|| panic!("missing body for `{struct_name}`"));
    let mut depth = 0usize;
    let mut body_end = None;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1).expect("unbalanced struct body");
                if depth == 0 {
                    body_end = Some(body_start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let body_end = body_end.unwrap_or_else(|| panic!("unclosed body for `{struct_name}`"));
    let mut fields = BTreeSet::new();
    for declaration in source[body_start + 1..body_end]
        .split(',')
        .map(str::trim)
        .filter(|declaration| !declaration.is_empty())
    {
        let (prefix, _) = declaration
            .split_once(':')
            .unwrap_or_else(|| panic!("malformed field in `{struct_name}`: `{declaration}`"));
        let mut parts = prefix.split_whitespace();
        let visibility =
            parts.next().unwrap_or_else(|| panic!("missing field visibility in `{struct_name}`"));
        let field_name =
            parts.next().unwrap_or_else(|| panic!("missing field name in `{struct_name}`"));
        assert!(
            parts.next().is_none(),
            "unexpected field declaration in `{struct_name}`: `{declaration}`"
        );
        assert!(
            fields.insert((visibility, field_name)),
            "duplicate field `{field_name}` in `{struct_name}`"
        );
    }
    fields
}

#[test]
fn child_module_has_no_external_visibility_or_reexport() {
    let mev_lines: Vec<_> = MEV_TRADER_SOURCE
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect();
    let module_index = mev_lines
        .iter()
        .position(|line| *line == "mod b5_dormant;")
        .expect("private dormant module declaration");

    assert_eq!(mev_lines[module_index - 1], "#[cfg(feature = \"b5-dormant-presign\")]");
    assert_eq!(occurrences(&production_code(MEV_TRADER_SOURCE), "b5_dormant"), 1);
    assert_eq!(occurrences(&source_code(LIB_SOURCE), "b5_dormant"), 0);

    for symbol in PRIVILEGED_SYMBOLS {
        assert_eq!(occurrences(&production_code(MEV_TRADER_SOURCE), symbol), 0);
        assert_eq!(occurrences(&source_code(LIB_SOURCE), symbol), 0);
    }
}

#[test]
fn binding_surface_is_at_most_parent_module_visible() {
    let dormant = production_code(B5_DORMANT_SOURCE);
    let compact = normalized(&dormant);

    for declaration in [
        "pub(super) struct VerifiedProvisioningManifestBinding",
        "pub(super) struct CommitBReviewedProvisioningBinding",
        "pub(super) enum B5ProvisioningBindingError",
    ] {
        assert_eq!(occurrences(&compact, declaration), 1, "visibility changed for `{declaration}`");
    }
    assert_eq!(occurrences(&compact, "fn verify_provisioning_bindings_against("), 1);
    assert_eq!(occurrences(&compact, "pub(super) fn verify_provisioning_bindings_against"), 0);
    let without_parent_visibility = compact.replace("pub(super)", "");
    assert_eq!(
        occurrences(&without_parent_visibility, "pub "),
        0,
        "a child item or field became publicly visible"
    );

    for widened in [
        "pub struct ",
        "pub enum ",
        "pub type ",
        "pub fn ",
        "pub const ",
        "pub static ",
        "pub use ",
        "pub(crate)",
        "pub(self)",
        "pub(in ",
    ] {
        assert_eq!(occurrences(&compact, widened), 0, "widened child surface: `{widened}`");
    }
}

#[test]
fn every_binding_struct_has_exactly_the_parent_visible_field_set() {
    let dormant = production_code(B5_DORMANT_SOURCE);
    let expected_manifest_fields: BTreeSet<_> = [
        "chain_id",
        "observed_manifest_sha256",
        "decoded_value_set_sha256",
        "recomputed_value_set_sha256",
        "deployment_evidence_sha256",
        "source_commit",
        "release_artifact_sha256",
        "deployment_review_file_sha256",
    ]
    .into_iter()
    .map(|field| ("pub(super)", field))
    .collect();
    let expected_reviewed_fields: BTreeSet<_> = [
        "chain_id",
        "manifest_sha256",
        "value_set_sha256",
        "deployment_evidence_sha256",
        "source_commit",
        "release_artifact_sha256",
        "deployment_review_file_sha256",
        "deployment_review_digest",
    ]
    .into_iter()
    .map(|field| ("pub(super)", field))
    .collect();

    assert_eq!(
        struct_field_visibilities(&dormant, "VerifiedProvisioningManifestBinding"),
        expected_manifest_fields,
        "manifest binding fields or visibilities changed"
    );
    assert_eq!(
        struct_field_visibilities(&dormant, "CommitBReviewedProvisioningBinding"),
        expected_reviewed_fields,
        "reviewed binding fields or visibilities changed"
    );
}

#[test]
fn production_has_no_privileged_consumer_path() {
    let dormant = production_code(B5_DORMANT_SOURCE);
    let helper = "verify_provisioning_bindings_against";

    assert_eq!(occurrences(&dormant, helper), 2, "unexpected helper reference inventory");
    assert_eq!(
        occurrences(&dormant, &format!("{helper}(")),
        1,
        "a production helper call appeared"
    );
    assert_eq!(occurrences(&dormant, "pub use"), 0);
    assert_eq!(occurrences(&dormant, "use crate::"), 0);
}
