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

mod input;
pub use input::{TdxQuotePolicy, TdxVerifierInput};

mod types;
pub use types::{
    ITDXVerifier, TDXTcbStatus, TDXVerificationResult, TDXVerifierJournal, TdxTcbStatusList,
    ZkCoProcessorConfig, ZkCoProcessorType,
};

mod quote;
pub use quote::{
    CERTIFICATION_DATA_HEADER_LEN, ECDSA_P256_ATTESTATION_KEY_TYPE, ECDSA_P256_PUBLIC_KEY_BODY_LEN,
    ECDSA_P256_SIGNATURE_LEN, ECDSA_SIG_AUX_DATA_CERTIFICATION_DATA_TYPE, MIN_SIGNATURE_DATA_LEN,
    MRSIGNERSEAM_OFFSET, MRTD_OFFSET, ParsedTdxQuote, QE_AUTHENTICATION_DATA_SIZE_LEN,
    QE_REPORT_ATTRIBUTES_LEN, QE_REPORT_ATTRIBUTES_OFFSET, QE_REPORT_DATA_HASH_LEN,
    QE_REPORT_DATA_OFFSET, QE_REPORT_ISV_PROD_ID_OFFSET, QE_REPORT_ISV_SVN_OFFSET, QE_REPORT_LEN,
    QE_REPORT_MISCSELECT_LEN, QE_REPORT_MISCSELECT_OFFSET, QE_REPORT_MRSIGNER_LEN,
    QE_REPORT_MRSIGNER_OFFSET, REPORT_DATA_OFFSET, RTMR_OFFSET, SEAM_ATTRIBUTES_OFFSET,
    TDX_MEASUREMENT_LEN, TDX_QUOTE_HEADER_LEN, TDX_REPORT_BODY_LEN, TDX_REPORT_DATA_LEN,
    TDX_SEAM_ATTRIBUTES_LEN, TDX_TEE_TCB_SVN_LEN, TDX_TEE_TYPE, TdxQuote, TdxQuoteHeader,
};

mod verify;
pub use verify::TdxVerifier;
