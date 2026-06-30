//! Intel QE identity collateral parsing and QE report matching.

use serde::Deserialize;

use crate::{
    ParsedTdxQuote, QE_REPORT_ATTRIBUTES_LEN, QE_REPORT_ATTRIBUTES_OFFSET,
    QE_REPORT_ISV_PROD_ID_OFFSET, QE_REPORT_ISV_SVN_OFFSET, QE_REPORT_MISCSELECT_LEN,
    QE_REPORT_MISCSELECT_OFFSET, QE_REPORT_MRSIGNER_LEN, QE_REPORT_MRSIGNER_OFFSET, Result,
    TdxVerifierError,
};

use super::{CollateralVerifier, IntelTcbStatus, TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION};

/// Signed Intel QE identity JSON document.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxQeIdentityDocument {
    /// QE identity payload.
    #[serde(rename = "enclaveIdentity")]
    pub enclave_identity: TdxQeIdentityBody,
}

/// Intel QE identity fields used to authenticate the quote's QE report.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxQeIdentityBody {
    /// Intel collateral class identifier.
    pub id: String,
    /// Intel collateral schema version.
    pub version: u16,
    /// Collateral issue date authenticated inside signed JSON.
    #[serde(rename = "issueDate")]
    pub issue_date: String,
    /// Collateral expiration authenticated inside signed JSON.
    #[serde(rename = "nextUpdate")]
    pub next_update: String,
    /// Expected QE `MISCSELECT` as hex text.
    pub miscselect: String,
    /// QE `MISCSELECT` mask as hex text.
    #[serde(rename = "miscselectMask")]
    pub miscselect_mask: String,
    /// Expected QE attributes as hex text.
    pub attributes: String,
    /// QE attributes mask as hex text.
    #[serde(rename = "attributesMask")]
    pub attributes_mask: String,
    /// Expected QE signer measurement as hex text.
    pub mrsigner: String,
    /// Expected QE product ID.
    pub isvprodid: u16,
    /// Ordered QE identity TCB levels.
    #[serde(rename = "tcbLevels")]
    pub tcb_levels: Vec<TdxQeIdentityLevel>,
}

impl TdxQeIdentityBody {
    /// Verifies this signed QE identity against the PCK-signed QE report.
    pub fn verify_qe_report(&self, quote: &ParsedTdxQuote) -> Result<()> {
        self.verify_tdx_identity()?;

        let miscselect = quote
            .qe_report
            .get(
                QE_REPORT_MISCSELECT_OFFSET..QE_REPORT_MISCSELECT_OFFSET + QE_REPORT_MISCSELECT_LEN,
            )
            .ok_or_else(|| {
                TdxVerifierError::InvalidQuote("QE report miscselect read out of bounds".into())
            })?;
        let attributes = quote
            .qe_report
            .get(
                QE_REPORT_ATTRIBUTES_OFFSET..QE_REPORT_ATTRIBUTES_OFFSET + QE_REPORT_ATTRIBUTES_LEN,
            )
            .ok_or_else(|| {
                TdxVerifierError::InvalidQuote("QE report attributes read out of bounds".into())
            })?;
        let mrsigner = quote
            .qe_report
            .get(QE_REPORT_MRSIGNER_OFFSET..QE_REPORT_MRSIGNER_OFFSET + QE_REPORT_MRSIGNER_LEN)
            .ok_or_else(|| {
                TdxVerifierError::InvalidQuote("QE report mrsigner read out of bounds".into())
            })?;
        let isvprodid =
            CollateralVerifier::read_u16_le_bytes(&quote.qe_report, QE_REPORT_ISV_PROD_ID_OFFSET)
                .map_err(TdxVerifierError::InvalidQuote)?;
        let isvsvn =
            CollateralVerifier::read_u16_le_bytes(&quote.qe_report, QE_REPORT_ISV_SVN_OFFSET)
                .map_err(TdxVerifierError::InvalidQuote)?;

        Self::verify_masked_field(
            miscselect,
            &self.miscselect,
            &self.miscselect_mask,
            "miscselect",
        )?;
        Self::verify_masked_field(
            attributes,
            &self.attributes,
            &self.attributes_mask,
            "attributes",
        )?;
        if mrsigner
            != CollateralVerifier::decode_hex_exact(&self.mrsigner, QE_REPORT_MRSIGNER_LEN)
                .map_err(TdxVerifierError::QeIdentityInvalid)?
                .as_ref()
        {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE report signer does not match QE identity".into(),
            ));
        }
        if isvprodid != self.isvprodid {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE report ISV product ID does not match QE identity".into(),
            ));
        }

        let status = self
            .tcb_levels
            .iter()
            .find(|level| level.tcb.isvsvn <= isvsvn)
            .map(|level| level.tcb_status)
            .ok_or_else(|| {
                TdxVerifierError::QeIdentityInvalid(
                    "no QE identity TCB level matches QE report".into(),
                )
            })?;
        if !status.is_accepted_qe_identity_status() {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE identity TCB status is not accepted".into(),
            ));
        }

        Ok(())
    }

    /// Verifies a QE report field matches the signed QE identity under a hex mask.
    pub fn verify_masked_field(
        actual: &[u8],
        expected_hex: &str,
        mask_hex: &str,
        field_name: &str,
    ) -> Result<()> {
        let matches = CollateralVerifier::masked_bytes_match(actual, expected_hex, mask_hex)
            .map_err(TdxVerifierError::QeIdentityInvalid)?;
        if !matches {
            return Err(TdxVerifierError::QeIdentityInvalid(format!(
                "QE report {field_name} does not match QE identity"
            )));
        }
        Ok(())
    }

    /// Verifies the signed QE identity is the TDX identity type and schema version.
    pub fn verify_tdx_identity(&self) -> Result<()> {
        if self.id != TDX_QE_IDENTITY_ID || self.version != TDX_QE_IDENTITY_VERSION {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE identity is not TDX TD_QE v2 collateral".into(),
            ));
        }
        Ok(())
    }
}

/// One QE identity TCB level.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxQeIdentityLevel {
    /// QE identity TCB threshold.
    pub tcb: TdxQeIdentityTcb,
    /// Intel status for this QE identity level.
    #[serde(rename = "tcbStatus")]
    pub tcb_status: IntelTcbStatus,
}

/// QE identity TCB SVN threshold.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxQeIdentityTcb {
    /// Minimum QE ISV SVN for this level.
    pub isvsvn: u16,
}
