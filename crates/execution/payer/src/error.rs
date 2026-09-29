//! ERC-8168 `PAYER_REJECTED` errors.

use alloy_primitives::{Address, Bytes, U256};
use jsonrpsee::types::ErrorObjectOwned;
use serde::{Deserialize, Serialize};

/// Condition a payer reports in `PAYER_REJECTED` error data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PayerErrorCode {
    /// The phase-0 transfer fails when simulated.
    ExecutionReverted,
    /// The transaction's gas exceeds the payer's per-transaction ceiling.
    GasExceedsLimit,
    /// The payer cannot evaluate the transaction right now.
    TemporarilyUnavailable,
    /// The transaction is malformed or does not follow the payer's terms.
    InvalidTransaction,
    /// The payment token is not accepted.
    UnsupportedToken,
    /// The phase-0 credit is below the required amount at the current rate.
    PaymentInsufficient,
    /// `valid_before` is missing or beyond the offer's `maxExpiry`.
    ExpiryOutOfBounds,
    /// The sender holds less of the payment token than phase 0 transfers.
    SenderBalanceInsufficient,
}

/// Corrected phase-0 credit for a [`PayerErrorCode::PaymentInsufficient`] rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Requote {
    /// Payment token.
    pub token: Address,
    /// Minimum credit accepted now for the transaction's gas fields.
    pub payment_amount: U256,
    /// Current rate, in token atomic units per 10^18 wei.
    pub rate: U256,
    /// Seconds the corrected amount holds.
    pub ttl: u64,
}

/// Sender shortfall for a [`PayerErrorCode::SenderBalanceInsufficient`] rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shortfall {
    /// Payment token.
    pub token: Address,
    /// Amount phase 0 transfers.
    pub required: U256,
    /// Sender's balance.
    pub available: U256,
}

/// Failed call for a [`PayerErrorCode::ExecutionReverted`] rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Revert {
    /// Call phase that failed; 0 is the payment.
    pub phase: u64,
    /// Raw revert data, when the call reverted rather than halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Bytes>,
}

/// Cost diagnostic for a [`PayerErrorCode::GasExceedsLimit`] rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GasDiagnostic {
    /// `gas_limit × max_fee_per_gas` of the transaction, in wei.
    pub estimated_cost: U256,
    /// Payer's per-transaction ceiling, in wei.
    pub max_cost: U256,
}

/// Data of a `-32000 PAYER_REJECTED` JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayerRejection {
    /// Rejected condition.
    pub code: PayerErrorCode,
    /// Human-readable detail.
    pub reason: String,
    /// Corrected credit, for [`PayerErrorCode::PaymentInsufficient`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requote: Option<Box<Requote>>,
    /// Sender shortfall, for [`PayerErrorCode::SenderBalanceInsufficient`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shortfall: Option<Box<Shortfall>>,
    /// Cost diagnostic, for [`PayerErrorCode::GasExceedsLimit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas: Option<Box<GasDiagnostic>>,
    /// Failed call, for [`PayerErrorCode::ExecutionReverted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<Box<Revert>>,
}

impl PayerRejection {
    /// JSON-RPC error code of every payer rejection.
    pub const RPC_CODE: i32 = -32000;

    /// JSON-RPC error message of every payer rejection.
    pub const RPC_MESSAGE: &str = "PAYER_REJECTED";

    /// Creates a rejection without actionable detail.
    pub fn new(code: PayerErrorCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            requote: None,
            shortfall: None,
            gas: None,
            revert: None,
        }
    }
}

impl From<PayerRejection> for ErrorObjectOwned {
    fn from(rejection: PayerRejection) -> Self {
        Self::owned(PayerRejection::RPC_CODE, PayerRejection::RPC_MESSAGE, Some(rejection))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejection_uses_payer_rejected_envelope() {
        let token = Address::repeat_byte(0x83);
        let rejection = PayerRejection {
            requote: Some(Box::new(Requote {
                token,
                payment_amount: U256::from(0x34bc0u64),
                rate: U256::from(0x7735_9400u64),
                ttl: 15,
            })),
            ..PayerRejection::new(PayerErrorCode::PaymentInsufficient, "credit too low")
        };

        let error = ErrorObjectOwned::from(rejection);

        assert_eq!(error.code(), -32000);
        assert_eq!(error.message(), "PAYER_REJECTED");
        let data: serde_json::Value = serde_json::from_str(error.data().unwrap().get()).unwrap();
        assert_eq!(
            data,
            json!({
                "code": "PAYMENT_INSUFFICIENT",
                "reason": "credit too low",
                "requote": {
                    "token": token,
                    "paymentAmount": "0x34bc0",
                    "rate": "0x77359400",
                    "ttl": 15
                }
            })
        );
    }
}
