//! Closed diagnostic-surface seal for public and CLI-private B5 errors.
#![cfg(feature = "b5-dormant-presign")]

use std::error::Error;

use mev_trader_submit::{DigestFramingError, ProvisioningSnapshotError};

const PRIVATE_CHILD_SOURCE: &str = include_str!("../src/mev_trader/b5_dormant.rs");
const PRIVATE_ERROR_CLASSES: [&str; 9] = [
    "ChainIdMismatch",
    "ManifestBindingMismatch",
    "ValueSetBindingMismatch",
    "EvidenceBindingMismatch",
    "SourceCommitBindingMismatch",
    "ReleaseArtifactBindingMismatch",
    "ReviewFileBindingMismatch",
    "ReviewSemanticBindingMismatch",
    "BuilderRejected",
];

#[test]
fn public_framing_errors_render_only_closed_class_text() {
    let cases = [
        (
            DigestFramingError::InvalidDomain,
            "InvalidDomain",
            "digest domain must be non-empty ASCII without NUL",
        ),
        (
            DigestFramingError::PayloadLengthOverflow,
            "PayloadLengthOverflow",
            "digest payload length exceeds the u32 framing limit",
        ),
    ];

    for (error, debug_class, display_class) in cases {
        assert_eq!(format!("{error:?}"), debug_class);
        assert_eq!(error.to_string(), display_class);
        assert!(error.source().is_none());
    }
}

#[test]
fn public_snapshot_errors_expose_only_allowed_nested_framing_class() {
    let unsupported = ProvisioningSnapshotError::UnsupportedChainId;
    assert_eq!(format!("{unsupported:?}"), "UnsupportedChainId");
    assert_eq!(
        unsupported.to_string(),
        "provisioning snapshot chain id is not the supported chain",
    );
    assert!(unsupported.source().is_none());

    for framing in [DigestFramingError::InvalidDomain, DigestFramingError::PayloadLengthOverflow] {
        let expected_nested_class = format!("{framing:?}");
        let error = ProvisioningSnapshotError::PayloadFraming(framing);
        assert_eq!(format!("{error:?}"), format!("PayloadFraming({expected_nested_class})"),);
        assert_eq!(error.to_string(), "provisioning snapshot payload framing failed",);
        let source = error.source().expect("payload framing exposes its closed framing class");
        assert_eq!(format!("{source:?}"), expected_nested_class);
        assert_eq!(source.to_string(), framing.to_string());
        assert!(source.source().is_none());
    }
}

#[test]
fn private_cli_error_is_manual_unit_only_and_exactly_closed() {
    let enum_marker = "pub(super) enum B5ProvisioningBindingError {";
    let marker_offset =
        PRIVATE_CHILD_SOURCE.find(enum_marker).expect("private error enum must remain present");
    let nearby_prefix =
        PRIVATE_CHILD_SOURCE[..marker_offset].lines().rev().take(12).collect::<Vec<_>>().join("\n");
    assert!(
        !nearby_prefix.contains("derive(Debug"),
        "private error Debug must stay manually redacted",
    );

    let enum_body = PRIVATE_CHILD_SOURCE[marker_offset + enum_marker.len()..]
        .split_once("\n}")
        .expect("private error enum must have a closing brace")
        .0;
    let declarations: Vec<&str> = enum_body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("///"))
        .collect();
    let expected_declarations: Vec<String> =
        PRIVATE_ERROR_CLASSES.iter().map(|class| format!("{class},")).collect();
    assert_eq!(declarations, expected_declarations);
    for class in PRIVATE_ERROR_CLASSES {
        assert!(
            PRIVATE_CHILD_SOURCE.contains(&format!(r#"Self::{class} => "{class}""#)),
            "private class mapping must be closed and identity-preserving for {class}",
        );
    }

    assert!(PRIVATE_CHILD_SOURCE.contains("impl fmt::Debug for B5ProvisioningBindingError {"));
    assert!(PRIVATE_CHILD_SOURCE.contains("impl fmt::Display for B5ProvisioningBindingError {"));
    assert!(PRIVATE_CHILD_SOURCE.contains("impl Error for B5ProvisioningBindingError {}"));
    assert!(PRIVATE_CHILD_SOURCE.contains("f.write_str(self.class())"));
    assert!(
        !PRIVATE_CHILD_SOURCE.contains("derive(Debug, Error)"),
        "private error must not derive value-bearing renderings",
    );
}
