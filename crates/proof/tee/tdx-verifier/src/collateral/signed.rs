//! Signed Intel collateral documents and bundle types.

use alloy_primitives::{B256, Bytes, keccak256};
use serde_json::Value;

use crate::{Result, TdxVerifierError};

use super::{
    CollateralVerifier, IntelTcbStatus, TdxCertificate, TdxQeIdentityDocument, TdxTcbInfoDocument,
};

/// Signed collateral document with its signing chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxSignedCollateral {
    /// Raw collateral bytes consumed by the verifier.
    pub raw: Bytes,
    /// Root-to-leaf signing certificate chain for this collateral.
    pub signing_chain: Vec<TdxCertificate>,
    /// P-256 ECDSA signature over the selected signed JSON body.
    pub signature: Bytes,
    /// Collateral issue time in seconds since Unix epoch.
    pub issue_time: u64,
    /// Collateral expiration time in seconds since Unix epoch.
    pub next_update: u64,
}

/// JSON body kind covered by an Intel PCS collateral signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TdxSignedCollateralBody {
    /// Signed TCB info body stored under `tcbInfo`.
    TcbInfo,
    /// Signed QE identity body stored under `enclaveIdentity`.
    QeIdentity,
}

impl TdxSignedCollateralBody {
    /// Returns the signed JSON field name for this collateral body.
    pub const fn json_key(self) -> &'static str {
        match self {
            Self::TcbInfo => "tcbInfo",
            Self::QeIdentity => "enclaveIdentity",
        }
    }

    /// Returns the verifier error for this collateral body.
    pub fn invalid(self, message: String) -> TdxVerifierError {
        match self {
            Self::TcbInfo => TdxVerifierError::TcbInfoInvalid(message),
            Self::QeIdentity => TdxVerifierError::QeIdentityInvalid(message),
        }
    }
}

impl TdxSignedCollateral {
    /// Returns the contract-compatible hash of the raw collateral bytes.
    pub fn hash(&self) -> B256 {
        keccak256(&self.raw)
    }

    /// Parses this signed collateral as an Intel TCB info JSON document.
    pub fn tcb_info_document(&self) -> Result<TdxTcbInfoDocument> {
        serde_json::from_slice(&self.raw).map_err(|e| {
            TdxVerifierError::TcbInfoInvalid(format!("TCB info JSON parse failed: {e}"))
        })
    }

    /// Parses this signed collateral as an Intel QE identity JSON document.
    pub fn qe_identity_document(&self) -> Result<TdxQeIdentityDocument> {
        serde_json::from_slice(&self.raw).map_err(|e| {
            TdxVerifierError::QeIdentityInvalid(format!("QE identity JSON parse failed: {e}"))
        })
    }

    /// Extracts issue and next-update times from the signed collateral JSON body.
    pub fn signed_validity(&self, body_kind: TdxSignedCollateralBody) -> Result<(u64, u64)> {
        let document: Value =
            serde_json::from_slice(&self.raw).map_err(|e| body_kind.invalid(format!("{e}")))?;
        let body = Self::signed_body_value(&document, body_kind)?;
        let signed_time_field = |field: &str| -> Result<u64> {
            match body.get(field) {
                Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
                    body_kind.invalid(format!("{field} is not an unsigned timestamp"))
                }),
                Some(Value::String(value)) => CollateralVerifier::parse_rfc3339_seconds(value)
                    .map_err(|message| body_kind.invalid(format!("{field} is invalid: {message}"))),
                Some(_) => Err(body_kind.invalid(format!("{field} has unsupported type"))),
                None => Err(body_kind.invalid(format!("{field} is missing"))),
            }
        };
        let issue_time = signed_time_field("issueDate")?;
        let next_update = signed_time_field("nextUpdate")?;
        Ok((issue_time, next_update))
    }

    /// Serializes the JSON value covered by the PCS collateral signature.
    pub fn signed_body_bytes(&self, body_kind: TdxSignedCollateralBody) -> Result<Vec<u8>> {
        let document: Value =
            serde_json::from_slice(&self.raw).map_err(|e| body_kind.invalid(format!("{e}")))?;
        let body = Self::signed_body_value(&document, body_kind)?;
        serde_json::to_vec(body).map_err(|e| {
            body_kind.invalid(format!("collateral signed body serialization failed: {e}"))
        })
    }

    /// Returns the JSON value covered by the PCS collateral signature.
    pub fn signed_body_value(
        document: &Value,
        body_kind: TdxSignedCollateralBody,
    ) -> Result<&Value> {
        if document.get(TdxSignedCollateralBody::TcbInfo.json_key()).is_some()
            && document.get(TdxSignedCollateralBody::QeIdentity.json_key()).is_some()
        {
            return Err(body_kind.invalid("collateral JSON contains multiple signed bodies".into()));
        }

        document
            .get(body_kind.json_key())
            .ok_or_else(|| body_kind.invalid(format!("{} body is missing", body_kind.json_key())))
    }
}

/// TCB info and QE identity collateral bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxCollateral {
    /// TCB info collateral and signing chain.
    pub tcb_info: TdxSignedCollateral,
    /// QE identity collateral and signing chain.
    pub qe_identity: TdxSignedCollateral,
    /// Intel TCB status selected from the TCB info levels.
    pub tcb_status: IntelTcbStatus,
}
