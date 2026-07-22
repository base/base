//! The `payer_*` JSON-RPC interface and its response DTOs.

use alloy_primitives::{Address, B256, Bytes, U256};
use base_execution_payer::{PriceSnapshot, TokenPrice};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Deserialize, Serialize};

/// ERC-8168 payer-service methods served by the Base builder-operated payer.
#[rpc(server, namespace = "payer")]
pub trait PayerApi {
    /// Returns the currently-quotable payer terms: the co-signing payer
    /// account, whether the service is live, and each accepted token's
    /// `feeRecipient`, exchange rate and payer margin. Served from a per-block
    /// price snapshot with no oracle round-trip.
    #[method(name = "getTerms")]
    async fn get_terms(&self) -> RpcResult<PayerTermsResponse>;

    /// Co-signs and submits a partially-signed EIP-8130 transaction.
    ///
    /// `transaction` is the EIP-2718 encoding of a transaction whose
    /// `sender_auth` is present, whose `payer` designates the builder's payer
    /// account, and whose `payer_auth` is still empty. The builder fills the
    /// `payer_auth` and submits the fully-authorized transaction to its
    /// mempool, returning the transaction hash.
    #[method(name = "sendTransaction")]
    async fn send_transaction(&self, transaction: Bytes) -> RpcResult<B256>;
}

/// The payer service's currently-quotable terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayerTermsResponse {
    /// Admin EOA that co-signs (`payer` / `payer_auth`) and receives payment.
    pub payer: Address,
    /// Whether the payer service is currently accepting transactions.
    pub enabled: bool,
    /// The accepted tokens currently quotable and their terms.
    pub tokens: Vec<TokenTermsDto>,
}

impl From<PriceSnapshot> for PayerTermsResponse {
    fn from(snapshot: PriceSnapshot) -> Self {
        Self {
            payer: snapshot.payer,
            enabled: snapshot.enabled,
            tokens: snapshot.prices.into_iter().map(TokenTermsDto::from).collect(),
        }
    }
}

/// Per-token terms: where to pay, the exchange rate, and the payer margin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTermsDto {
    /// ERC-20 token accepted for gas payment.
    pub token: Address,
    /// Phase-0 transfer destination (ERC-8168 `feeRecipient`).
    pub fee_recipient: Address,
    /// Token-atomic-units-per-native-wei exchange rate.
    pub rate: RateDto,
    /// Payer margin in basis points, folded into the quoted amount.
    pub margin_bps: u16,
}

impl From<TokenPrice> for TokenTermsDto {
    fn from(price: TokenPrice) -> Self {
        Self {
            token: price.token,
            fee_recipient: price.fee_recipient,
            rate: RateDto {
                numerator: price.rate.numerator,
                denominator: price.rate.denominator,
            },
            margin_bps: price.margin_bps,
        }
    }
}

/// An exact exchange rate as ERC-8168's `{ numerator, denominator }` rational:
/// `numerator` token atomic units per `denominator` native wei.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateDto {
    /// Token atomic units.
    pub numerator: U256,
    /// Native wei that [`Self::numerator`] token atomic units are worth.
    pub denominator: U256,
}
