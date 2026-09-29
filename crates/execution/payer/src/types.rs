//! ERC-8168 `payer_*` request and response types served by the payer.

use alloy_primitives::{Address, B256, Bytes, U64, U128, U256};
use serde::{Deserialize, Serialize};

/// `payer_getTerms` parameters.
///
/// The payer quotes one rate per token regardless of the intent's sender,
/// calls, or authentication, so the remaining ERC-8168 fields are accepted and
/// ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTermsParams {
    /// Chain the intent executes on.
    pub chain_id: U64,
    /// Gas for the intent's calls, excluding the phase-0 payment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<U64>,
    /// Tokens to list first, in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_tokens: Vec<Address>,
}

/// `payer_getTerms` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTermsResult {
    /// Advisory gas values every offer's `paymentAmount` is priced at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas_estimate: Option<GasEstimate>,
    /// Offers, best first; empty when the payer has nothing to offer.
    pub options: Vec<PaymentOption>,
}

/// Advisory gas values shared by every offer in a [`GetTermsResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GasEstimate {
    /// Gas limit, phase-0 payment included.
    pub gas_limit: U64,
    /// Maximum fee per gas, in wei.
    pub max_fee_per_gas: U128,
    /// Maximum priority fee per gas, in wei.
    pub max_priority_fee_per_gas: U128,
}

/// A selectable ERC-8168 payment option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaymentOption {
    /// Payment in one of several tokens under shared terms.
    Token(TokenPaymentOffer),
}

/// Offer to pay gas in exchange for a phase-0 token transfer.
///
/// `methods` is omitted, so the offer supports only `payer_sendTransaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPaymentOffer {
    /// Account the wallet names as the transaction's payer and pays in phase 0.
    pub payer: Address,
    /// Seconds the offer, rates included, may be cached.
    pub ttl: u64,
    /// Binding constraints on the co-signed transaction.
    pub conditions: OfferConditions,
    /// Accepted tokens.
    pub tokens: Vec<TokenChoice>,
}

/// Binding constraints of an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferConditions {
    /// Maximum seconds between co-signing and the transaction's `valid_before`.
    pub max_expiry: u64,
    /// Maximum `gas_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gas_limit: Option<U64>,
    /// Maximum `gas_limit × max_fee_per_gas`, in wei.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<U256>,
}

/// One accepted token in a [`TokenPaymentOffer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenChoice {
    /// Token contract.
    pub token: Address,
    /// Display symbol.
    pub symbol: String,
    /// Token decimals.
    pub decimals: u8,
    /// Token atomic units per 10^18 wei.
    pub rate: U256,
    /// Required phase-0 amount at the response's `gasEstimate`.
    pub payment_amount: U256,
    /// Gas the phase-0 transfer adds to `gas_limit`.
    pub payment_gas: U64,
}

/// `payer_sendTransaction` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTransactionParams {
    /// Sender-signed EIP-8130 transaction naming the payer, with empty `payer_auth`.
    pub signed_transaction: Bytes,
}

/// `payer_sendTransaction` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTransactionResult {
    /// Hash of the co-signed transaction.
    pub transaction_hash: B256,
    /// Phase-0 credit the transaction makes.
    pub token_charged: TokenCharged,
}

/// Phase-0 credit reported by [`SendTransactionResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCharged {
    /// Token paid.
    pub token: Address,
    /// Amount credited to the payer.
    pub amount: U256,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn get_terms_params_ignore_intent_fields() {
        let params: GetTermsParams = serde_json::from_value(json!({
            "chainId": "0x2105",
            "from": "0xAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAaAa",
            "calls": [{ "to": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", "data": "0x" }],
            "gasLimit": "0xC350",
            "context": { "policyId": "abc" }
        }))
        .unwrap();

        assert_eq!(
            params,
            GetTermsParams {
                chain_id: U64::from(8453),
                gas_limit: Some(U64::from(50_000)),
                preferred_tokens: Vec::new(),
            }
        );
    }

    #[test]
    fn token_offer_serializes_with_kind_tag() {
        let option = PaymentOption::Token(TokenPaymentOffer {
            payer: Address::repeat_byte(0xcc),
            ttl: 15,
            conditions: OfferConditions { max_expiry: 10, max_gas_limit: None, max_cost: None },
            tokens: vec![TokenChoice {
                token: Address::repeat_byte(0x83),
                symbol: "USDC".to_owned(),
                decimals: 6,
                rate: U256::from(0x7735_9400u64),
                payment_amount: U256::from(0x33450u64),
                payment_gas: U64::from(0x4e20),
            }],
        });

        let value = serde_json::to_value(option).unwrap();

        assert_eq!(value["kind"], "token");
        assert_eq!(value["conditions"], json!({ "maxExpiry": 10 }));
        assert_eq!(value["tokens"][0]["rate"], "0x77359400");
        assert_eq!(value["tokens"][0]["paymentAmount"], "0x33450");
        assert_eq!(value["tokens"][0]["paymentGas"], "0x4e20");
    }
}
