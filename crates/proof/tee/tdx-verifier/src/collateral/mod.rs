//! Explicit TDX collateral, signing chain, and revocation evidence inputs.

mod certificate;
pub use certificate::{AuthenticatedTdxCertificate, TdxCertificate};

mod platform;
pub use platform::{TdxPckTcb, TdxPlatformIdentity};

mod qe_identity;
pub use qe_identity::{
    TdxQeIdentityBody, TdxQeIdentityDocument, TdxQeIdentityLevel, TdxQeIdentityTcb,
};

mod revocation;
pub use revocation::{AuthenticatedTdxCrl, TdxRevocationEvidence};

mod signed;
pub use signed::{TdxCollateral, TdxSignedCollateral, TdxSignedCollateralBody};

mod status;
pub use status::IntelTcbStatus;

mod tcb_info;
pub use tcb_info::{
    TdxModule, TdxModuleIdentity, TdxModuleTcb, TdxModuleTcbLevel, TdxTcbComponent,
    TdxTcbComponents, TdxTcbInfoBody, TdxTcbInfoDocument, TdxTcbLevel,
};

mod verifier;
pub use verifier::CollateralVerifier;

/// Subject common name expected for Intel PCS TCB collateral signing certificates.
pub const INTEL_TCB_SIGNING_CERT_COMMON_NAME: &str = "Intel SGX TCB Signing";

/// Intel TCB info identifier expected for TDX collateral.
pub const TDX_TCB_INFO_ID: &str = "TDX";

/// Intel QE identity identifier expected for TDX quotes.
pub const TDX_QE_IDENTITY_ID: &str = "TD_QE";

/// Intel QE identity schema version expected for TDX quotes.
pub const TDX_QE_IDENTITY_VERSION: u16 = 2;
