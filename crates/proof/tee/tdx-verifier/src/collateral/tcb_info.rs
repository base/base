//! Intel TCB info collateral parsing and quote TCB matching.

use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

use crate::{ParsedTdxQuote, Result, TDX_TEE_TYPE, TdxVerifierError};

use super::{CollateralVerifier, IntelTcbStatus, TDX_TCB_INFO_ID, TdxPckTcb, TdxPlatformIdentity};

/// Signed Intel TCB info JSON document body.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxTcbInfoDocument {
    /// TCB info payload.
    #[serde(rename = "tcbInfo")]
    pub tcb_info: TdxTcbInfoBody,
}

/// Intel TCB info payload fields used by this verifier.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxTcbInfoBody {
    /// Intel collateral class identifier.
    pub id: String,
    /// Intel TEE type for TDX, when supplied by the PCS response.
    #[serde(default, rename = "teeType")]
    pub tee_type: Option<TdxTeeType>,
    /// Collateral issue date authenticated inside signed JSON.
    #[serde(rename = "issueDate")]
    pub issue_date: String,
    /// Collateral expiration authenticated inside signed JSON.
    #[serde(rename = "nextUpdate")]
    pub next_update: String,
    /// Platform FMSPC as Intel hex text.
    pub fmspc: String,
    /// Platform PCE ID as Intel hex text.
    #[serde(rename = "pceId", alias = "pceid")]
    pub pce_id: String,
    /// Default TDX module identity authenticated in this TCB info document.
    #[serde(rename = "tdxModule")]
    pub tdx_module: TdxModule,
    /// Versioned TDX module identities authenticated in this TCB info document.
    #[serde(rename = "tdxModuleIdentities")]
    pub tdx_module_identities: Vec<TdxModuleIdentity>,
    /// Ordered TCB levels from the signed TCB info document.
    #[serde(rename = "tcbLevels")]
    pub tcb_levels: Vec<TdxTcbLevel>,
}

impl TdxTcbInfoBody {
    /// Verifies that this signed TCB info document is TDX collateral.
    pub fn verify_tdx_collateral(&self) -> Result<()> {
        if self.id != TDX_TCB_INFO_ID
            || self.tee_type.is_some_and(|tee_type| tee_type.value != TDX_TEE_TYPE)
        {
            return Err(TdxVerifierError::TcbInfoInvalid("TCB info is not TDX collateral".into()));
        }
        Ok(())
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
        self.verify_tdx_collateral()?;
        let platform_status = self
            .tcb_levels
            .iter()
            .find(|level| level.tcb.matches_quote_and_pck(quote, pck_tcb))
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
            self.tdx_module.verify_quote(quote)?;
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
        module.verify_quote(quote)?;

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

/// Intel TEE type parsed from a signed TCB info document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TdxTeeType {
    /// Numeric TEE type value.
    pub value: u32,
}

impl TdxTeeType {
    /// Parses Intel TEE type text as hexadecimal, accepting an optional `0x` prefix.
    pub fn parse_hex(value: &str) -> std::result::Result<Self, String> {
        let value = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
        u32::from_str_radix(value, 16)
            .map(|value| Self { value })
            .map_err(|e| format!("teeType parse failed: {e}"))
    }
}

impl<'de> Deserialize<'de> for TdxTeeType {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::Number(number) => {
                let value = number
                    .as_u64()
                    .ok_or_else(|| de::Error::custom("teeType is not an unsigned integer"))?;
                let value =
                    u32::try_from(value).map_err(|_| de::Error::custom("teeType exceeds u32"))?;
                Ok(Self { value })
            }
            Value::String(value) => Self::parse_hex(&value).map_err(de::Error::custom),
            _ => Err(de::Error::custom("teeType has unsupported type")),
        }
    }
}

/// One TCB level from the signed TCB info document.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxTcbLevel {
    /// Component SVN requirements for this level.
    pub tcb: TdxTcbComponents,
    /// Intel status for this level.
    #[serde(rename = "tcbStatus")]
    pub tcb_status: IntelTcbStatus,
}

/// Component SVN requirements from one TCB level.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxTcbComponents {
    /// Minimum PCE SVN for this level.
    pub pcesvn: u16,
    /// TDX TCB component SVNs for this level.
    #[serde(default, rename = "tdxtcbcomponents", alias = "tdxTcbComponents")]
    pub tdxtcbcomponents: Vec<TdxTcbComponent>,
    /// SGX TCB component SVNs used by some collateral encodings.
    #[serde(default, rename = "sgxtcbcomponents", alias = "sgxTcbComponents")]
    pub sgxtcbcomponents: Vec<TdxTcbComponent>,
}

impl TdxTcbComponents {
    /// Returns true when this TCB level applies to the PCK certificate and quote.
    pub fn matches_quote_and_pck(&self, quote: &ParsedTdxQuote, pck_tcb: &TdxPckTcb) -> bool {
        self.matches_pck_tcb(pck_tcb) && self.matches_quote_tdx_tcb(quote)
    }

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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxTcbComponent {
    /// Security version number for this component.
    pub svn: u16,
}

/// Signed default TDX module identity fields from TCB info collateral.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxModule {
    /// Expected TDX module signer measurement as hex text.
    pub mrsigner: String,
    /// Expected TDX module SEAM attributes as hex text.
    pub attributes: String,
    /// Mask applied when comparing TDX module SEAM attributes.
    #[serde(rename = "attributesMask")]
    pub attributes_mask: String,
}

impl TdxModule {
    /// Verifies this module identity against the quote report body.
    pub fn verify_quote(&self, quote: &ParsedTdxQuote) -> Result<()> {
        CollateralVerifier::verify_module_identity_fields(
            quote,
            &self.mrsigner,
            &self.attributes,
            &self.attributes_mask,
        )
    }
}

/// Signed versioned TDX module identity from TCB info collateral.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxModuleIdentity {
    /// Versioned module identity ID, such as `TDX_03`.
    pub id: String,
    /// Expected TDX module signer measurement as hex text.
    pub mrsigner: String,
    /// Expected TDX module SEAM attributes as hex text.
    pub attributes: String,
    /// Mask applied when comparing TDX module SEAM attributes.
    #[serde(rename = "attributesMask")]
    pub attributes_mask: String,
    /// Ordered TCB levels for this module identity.
    #[serde(rename = "tcbLevels")]
    pub tcb_levels: Vec<TdxModuleTcbLevel>,
}

impl TdxModuleIdentity {
    /// Verifies this module identity against the quote report body.
    pub fn verify_quote(&self, quote: &ParsedTdxQuote) -> Result<()> {
        CollateralVerifier::verify_module_identity_fields(
            quote,
            &self.mrsigner,
            &self.attributes,
            &self.attributes_mask,
        )
    }
}

/// One TDX module identity TCB level.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxModuleTcbLevel {
    /// Module identity TCB requirement.
    pub tcb: TdxModuleTcb,
    /// Intel status for this module identity level.
    #[serde(rename = "tcbStatus")]
    pub tcb_status: IntelTcbStatus,
}

/// TDX module identity TCB requirement.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TdxModuleTcb {
    /// Minimum module ISV SVN for this level.
    pub isvsvn: u32,
}
