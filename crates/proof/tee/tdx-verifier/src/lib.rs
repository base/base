#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod attestation;
pub use attestation::{TdxSignerAttestation, TdxSignerAttestationDecodeError};

mod collateral;
pub use collateral::{
    AuthenticatedTdxCertificate, AuthenticatedTdxCrl, CollateralVerifier,
    INTEL_TCB_SIGNING_CERT_COMMON_NAME, IntelTcbStatus, TDX_QE_IDENTITY_ID,
    TDX_QE_IDENTITY_VERSION, TDX_TCB_INFO_ID, TdxCertificate, TdxCollateral, TdxModule,
    TdxModuleIdentity, TdxModuleTcb, TdxModuleTcbLevel, TdxPckTcb, TdxPlatformIdentity,
    TdxQeIdentityBody, TdxQeIdentityDocument, TdxQeIdentityLevel, TdxQeIdentityTcb,
    TdxRevocationEvidence, TdxSignedCollateral, TdxSignedCollateralBody, TdxTcbComponent,
    TdxTcbComponents, TdxTcbInfoBody, TdxTcbInfoDocument, TdxTcbLevel,
};

mod error;
pub use error::{Result, TdxVerifierError};

mod types;
pub use types::{
    ITDXVerifier, TDXTcbStatus, TDXVerificationResult, TDXVerifierJournal, ZkCoProcessorConfig,
    ZkCoProcessorType,
};

mod quote;
pub use quote::{ParsedTdxQuote, TDX_MEASUREMENT_LEN, TDX_REPORT_DATA_LEN, TdxQuote};

mod verify;
pub use verify::{TdxVerifier, TdxVerifierInput};
