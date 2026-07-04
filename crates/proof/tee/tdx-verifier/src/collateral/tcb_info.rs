//! Intel TCB info collateral parsing and quote TCB matching.

use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

use super::{CollateralVerifier, IntelTcbStatus, TDX_TCB_INFO_ID, TdxPckTcb, TdxPlatformIdentity};
use crate::{ParsedTdxQuote, Result, TdxVerifierError, quote::TDX_TEE_TYPE};

/// Signed Intel TCB info JSON document body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxTcbInfoDocument {
    /// TCB info payload.
    pub tcb_info: TdxTcbInfoBody,
}

/// Intel TCB info payload fields used by this verifier.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxTcbInfoBody {
    /// Intel collateral class identifier.
    pub id: String,
    /// Intel TEE type for TDX, when supplied by the PCS response.
    #[serde(default, deserialize_with = "TdxTcbInfoBody::deserialize_tee_type")]
    pub tee_type: Option<u32>,
    /// Platform FMSPC as Intel hex text.
    pub fmspc: String,
    /// Platform PCE ID as Intel hex text.
    #[serde(alias = "pceid")]
    pub pce_id: String,
    /// Default TDX module identity authenticated in this TCB info document.
    pub tdx_module: TdxModule,
    /// Versioned TDX module identities authenticated in this TCB info document.
    pub tdx_module_identities: Vec<TdxModuleIdentity>,
    /// Ordered TCB levels from the signed TCB info document.
    pub tcb_levels: Vec<TdxTcbLevel>,
}

impl TdxTcbInfoBody {
    /// Parses an optional Intel TEE type value from numeric or hexadecimal JSON.
    pub fn deserialize_tee_type<'de, D>(
        deserializer: D,
    ) -> std::result::Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Null => Ok(None),
            Value::Number(number) => {
                let value = number
                    .as_u64()
                    .ok_or_else(|| de::Error::custom("teeType is not an unsigned integer"))?;
                u32::try_from(value).map(Some).map_err(|_| de::Error::custom("teeType exceeds u32"))
            }
            Value::String(value) => {
                let value =
                    value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(&value);
                u32::from_str_radix(value, 16)
                    .map(Some)
                    .map_err(|e| de::Error::custom(format!("teeType parse failed: {e}")))
            }
            _ => Err(de::Error::custom("teeType has unsupported type")),
        }
    }

    /// Verifies that this signed TCB info applies to the PCK certificate platform.
    pub fn verify_platform(&self, pck_platform: &TdxPlatformIdentity) -> Result<()> {
        let fmspc = CollateralVerifier::decode_hex(&self.fmspc)
            .map_err(TdxVerifierError::TcbInfoInvalid)?;
        let pce_id = CollateralVerifier::decode_hex(&self.pce_id)
            .map_err(TdxVerifierError::TcbInfoInvalid)?;
        if fmspc != pck_platform.fmspc || pce_id != pck_platform.pce_id {
            return Err(TdxVerifierError::TcbInfoInvalid(
                "TCB info FMSPC/PCE ID does not match PCK certificate".into(),
            ));
        }
        Ok(())
    }

    /// Selects the first TCB level matching the PCK SGX/PCE TCB and quote TDX module identity.
    pub fn tcb_status_for_quote(
        &self,
        quote: &ParsedTdxQuote,
        pck_tcb: &TdxPckTcb,
    ) -> Result<IntelTcbStatus> {
        if self.id != TDX_TCB_INFO_ID
            || self.tee_type.is_some_and(|tee_type| tee_type != TDX_TEE_TYPE)
        {
            return Err(TdxVerifierError::TcbInfoInvalid("TCB info is not TDX collateral".into()));
        }
        let platform_status = self
            .tcb_levels
            .iter()
            .find(|level| {
                level.tcb.matches_pck_tcb(pck_tcb) && level.tcb.matches_quote_tdx_tcb(quote)
            })
            .map(|level| level.tcb_status)
            .ok_or_else(|| {
                TdxVerifierError::TcbInfoInvalid("no TCB info level matches quote TCB".into())
            })?;
        let module_status = self.tdx_module_status_for_quote(quote)?;
        Ok(platform_status.converge_with_tdx_module_status(module_status))
    }

    /// Returns the TDX module identity TCB status for the loaded module in the quote.
    pub fn tdx_module_status_for_quote(&self, quote: &ParsedTdxQuote) -> Result<IntelTcbStatus> {
        let module_version = quote.tee_tcb_svn[1];
        if module_version == 0 {
            CollateralVerifier::verify_module_identity_fields(
                quote,
                &self.tdx_module.mrsigner,
                &self.tdx_module.attributes,
                &self.tdx_module.attributes_mask,
            )?;
            return Ok(IntelTcbStatus::UpToDate);
        }

        let expected_id = format!("TDX_{module_version:02X}");
        let module = self
            .tdx_module_identities
            .iter()
            .find(|identity| identity.id.eq_ignore_ascii_case(&expected_id))
            .ok_or_else(|| {
                TdxVerifierError::TcbInfoInvalid(format!(
                    "no TDX module identity matches quote module version {module_version}"
                ))
            })?;
        CollateralVerifier::verify_module_identity_fields(
            quote,
            &module.mrsigner,
            &module.attributes,
            &module.attributes_mask,
        )?;

        let module_isvsvn = u32::from(quote.tee_tcb_svn[0]);
        module
            .tcb_levels
            .iter()
            .find(|level| level.tcb.isvsvn <= module_isvsvn)
            .map(|level| level.tcb_status)
            .ok_or_else(|| {
                TdxVerifierError::TcbInfoInvalid(
                    "no TDX module identity TCB level matches quote".into(),
                )
            })
    }
}

/// One TCB level from the signed TCB info document.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxTcbLevel {
    /// Component SVN requirements for this level.
    pub tcb: TdxTcbComponents,
    /// Intel status for this level.
    pub tcb_status: IntelTcbStatus,
}

/// Component SVN requirements from one TCB level.
#[derive(Debug, Deserialize)]
pub struct TdxTcbComponents {
    /// Minimum PCE SVN for this level.
    pub pcesvn: u16,
    /// TDX TCB component SVNs for this level.
    #[serde(default, alias = "tdxTcbComponents")]
    pub tdxtcbcomponents: Vec<TdxTcbComponent>,
    /// SGX TCB component SVNs used by some collateral encodings.
    #[serde(default, alias = "sgxTcbComponents")]
    pub sgxtcbcomponents: Vec<TdxTcbComponent>,
}

impl TdxTcbComponents {
    /// Returns true when this level's SGX/PCE requirements match the PCK certificate.
    pub fn matches_pck_tcb(&self, pck_tcb: &TdxPckTcb) -> bool {
        self.pcesvn <= pck_tcb.pce_svn
            && self.sgxtcbcomponents.len() == pck_tcb.sgx_tcb_svn.len()
            && self
                .sgxtcbcomponents
                .iter()
                .zip(pck_tcb.sgx_tcb_svn)
                .all(|(component, pck_svn)| component.svn <= u16::from(pck_svn))
    }

    /// Returns true when this level's TDX requirements match the quote report body.
    pub fn matches_quote_tdx_tcb(&self, quote: &ParsedTdxQuote) -> bool {
        let component_start = if quote.tee_tcb_svn[1] > 0 { 2 } else { 0 };
        self.tdxtcbcomponents.len() == quote.tee_tcb_svn.len()
            && self
                .tdxtcbcomponents
                .iter()
                .skip(component_start)
                .zip(quote.tee_tcb_svn.iter().skip(component_start))
                .all(|(component, quote_svn)| component.svn <= u16::from(*quote_svn))
    }
}

/// One TCB component SVN.
#[derive(Debug, Deserialize)]
pub struct TdxTcbComponent {
    /// Security version number for this component.
    pub svn: u16,
}

/// Signed default TDX module identity fields from TCB info collateral.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxModule {
    /// Expected TDX module signer measurement as hex text.
    pub mrsigner: String,
    /// Expected TDX module SEAM attributes as hex text.
    pub attributes: String,
    /// Mask applied when comparing TDX module SEAM attributes.
    pub attributes_mask: String,
}

/// Signed versioned TDX module identity from TCB info collateral.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxModuleIdentity {
    /// Versioned module identity ID, such as `TDX_03`.
    pub id: String,
    /// Expected TDX module signer measurement as hex text.
    pub mrsigner: String,
    /// Expected TDX module SEAM attributes as hex text.
    pub attributes: String,
    /// Mask applied when comparing TDX module SEAM attributes.
    pub attributes_mask: String,
    /// Ordered TCB levels for this module identity.
    pub tcb_levels: Vec<TdxModuleTcbLevel>,
}

/// One TDX module identity TCB level.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TdxModuleTcbLevel {
    /// Module identity TCB requirement.
    pub tcb: TdxModuleTcb,
    /// Intel status for this module identity level.
    pub tcb_status: IntelTcbStatus,
}

/// TDX module identity TCB requirement.
#[derive(Debug, Deserialize)]
pub struct TdxModuleTcb {
    /// Minimum module ISV SVN for this level.
    pub isvsvn: u32,
}
