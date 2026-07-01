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
pub use quote::{
    CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_ATTESTATION_KEY_TYPE, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
    ECDSA_P256_SIGNATURE_LEN, ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE, MIN_AUX_DATA_LEN,
    MIN_SIGNATURE_DATA_LEN, MRTD_OFFSET, ParsedTdxQuote, QE_AUTHENTICATION_DATA_SIZE_LEN,
    QE_REPORT_LEN, REPORT_DATA_OFFSET, RTMR_OFFSET, SIGNATURE_DATA_LEN_PREFIX_LEN,
    TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN, TDX_QUOTE_VERSION, TDX_REPORT_BODY_LEN,
    TDX_REPORT_DATA_LEN, TDX_TEE_TYPE, TdxQuote,
};

mod verify;
pub use verify::{TdxVerifier, TdxVerifierInput};
