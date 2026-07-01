//! Intel QE identity collateral parsing and QE report matching.

use serde::Deserialize;

use crate::{
    ParsedTdxQuote, Result, TdxQuote, TdxVerifierError,
    quote::{
        QE_REPORT_ATTRIBUTES_LEN, QE_REPORT_ATTRIBUTES_OFFSET, QE_REPORT_ISV_PROD_ID_OFFSET,
        QE_REPORT_ISV_SVN_OFFSET, QE_REPORT_MISCSELECT_LEN, QE_REPORT_MISCSELECT_OFFSET,
        QE_REPORT_MRSIGNER_LEN, QE_REPORT_MRSIGNER_OFFSET,
    },
};

use super::{CollateralVerifier, IntelTcbStatus, TDX_QE_IDENTITY_ID, TDX_QE_IDENTITY_VERSION};

/// Signed Intel QE identity JSON document.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TdxQeIdentityDocument {
    /// QE identity payload.
    pub enclave_identity: TdxQeIdentityBody,
}

/// Intel QE identity fields used to authenticate the quote's QE report.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TdxQeIdentityBody {
    /// Intel collateral class identifier.
    pub id: String,
    /// Intel collateral schema version.
    pub version: u16,
    /// Expected QE `MISCSELECT` as hex text.
    pub miscselect: String,
    /// QE `MISCSELECT` mask as hex text.
    pub miscselect_mask: String,
    /// Expected QE attributes as hex text.
    pub attributes: String,
    /// QE attributes mask as hex text.
    pub attributes_mask: String,
    /// Expected QE signer measurement as hex text.
    pub mrsigner: String,
    /// Expected QE product ID.
    pub isvprodid: u16,
    /// Ordered QE identity TCB levels.
    pub tcb_levels: Vec<TdxQeIdentityLevel>,
}

impl TdxQeIdentityBody {
    /// Verifies this signed QE identity against the PCK-signed QE report.
    pub fn verify_qe_report(&self, quote: &ParsedTdxQuote) -> Result<()> {
        if self.id != TDX_QE_IDENTITY_ID || self.version != TDX_QE_IDENTITY_VERSION {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE identity is not TDX TD_QE v2 collateral".into(),
            ));
        }

        let miscselect = TdxQuote::read_array::<QE_REPORT_MISCSELECT_LEN>(
            &quote.qe_report,
            QE_REPORT_MISCSELECT_OFFSET,
        )?;
        let attributes = TdxQuote::read_array::<QE_REPORT_ATTRIBUTES_LEN>(
            &quote.qe_report,
            QE_REPORT_ATTRIBUTES_OFFSET,
        )?;
        let mrsigner = TdxQuote::read_array::<QE_REPORT_MRSIGNER_LEN>(
            &quote.qe_report,
            QE_REPORT_MRSIGNER_OFFSET,
        )?;
        let isvprodid = u16::from_le_bytes(TdxQuote::read_array::<2>(
            &quote.qe_report,
            QE_REPORT_ISV_PROD_ID_OFFSET,
        )?);
        let isvsvn = u16::from_le_bytes(TdxQuote::read_array::<2>(
            &quote.qe_report,
            QE_REPORT_ISV_SVN_OFFSET,
        )?);

        for (actual, expected_hex, mask_hex, field_name) in [
            (miscselect.as_slice(), &self.miscselect, &self.miscselect_mask, "miscselect"),
            (attributes.as_slice(), &self.attributes, &self.attributes_mask, "attributes"),
        ] {
            let matches = CollateralVerifier::masked_bytes_match(actual, expected_hex, mask_hex)
                .map_err(TdxVerifierError::QeIdentityInvalid)?;
            if !matches {
                return Err(TdxVerifierError::QeIdentityInvalid(format!(
                    "QE report {field_name} does not match QE identity"
                )));
            }
        }
        if mrsigner.as_slice()
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
        if status != IntelTcbStatus::UpToDate {
            return Err(TdxVerifierError::QeIdentityInvalid(
                "QE identity TCB status is not accepted".into(),
            ));
        }

        Ok(())
    }
}

/// One QE identity TCB level.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TdxQeIdentityLevel {
    /// QE identity TCB threshold.
    pub tcb: TdxQeIdentityTcb,
    /// Intel status for this QE identity level.
    pub tcb_status: IntelTcbStatus,
}

/// QE identity TCB SVN threshold.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxQeIdentityTcb {
    /// Minimum QE ISV SVN for this level.
    pub isvsvn: u16,
}
