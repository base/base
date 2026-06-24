use alloc::vec::Vec;

use alloy_consensus::{
    Sealed, SignableTransaction, Signed, TxEip1559, TxEip4844, TypedTransaction,
};
use alloy_eips::eip7702::SignedAuthorization;
#[cfg(feature = "reth")]
use alloy_network::TransactionBuilder;
use alloy_network_primitives::TransactionBuilder7702;
use alloy_primitives::{Address, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use base_common_consensus::{
    AccountChange, BaseTxEnvelope, BaseTypedTransaction, Call, Eip8130Constants, Eip8130Contracts,
    TxDeposit,
};
use serde::{Deserialize, Serialize};

use crate::Transaction;

/// Authentication scheme an EIP-8130 gas estimate should price.
///
/// Estimation never verifies a signature: the scheme only selects which
/// enshrined authenticator the intrinsic-gas schedule charges (the
/// authenticator's execution gas plus the calldata cost of its authentication
/// payload). Absent a scheme, the estimate prices the default-EOA secp256k1
/// (bare-signature) path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Eip8130AuthScheme {
    /// secp256k1 — the default-EOA / k1 authenticator.
    Secp256k1,
    /// P-256 (secp256r1) authenticator.
    P256,
    /// `WebAuthn` authenticator (P-256 over `authenticatorData || clientDataJSON`).
    WebAuthn,
}

impl Eip8130AuthScheme {
    /// The enshrined authenticator address whose schedule entry prices this
    /// scheme.
    #[must_use]
    pub const fn authenticator(self) -> Address {
        match self {
            Self::Secp256k1 => Eip8130Constants::K1_AUTHENTICATOR,
            Self::P256 => Eip8130Contracts::P256_AUTHENTICATOR,
            Self::WebAuthn => Eip8130Contracts::WEBAUTHN_AUTHENTICATOR,
        }
    }

    /// Representative byte length of the authentication `data` (the bytes after
    /// the 20-byte authenticator selector) for a real signature of this scheme.
    /// Used to price calldata gas when the request omits an explicit size; the
    /// variable-length `WebAuthn` payload should be sized via `*_auth_size`.
    #[must_use]
    pub const fn default_data_len(self) -> usize {
        match self {
            Self::Secp256k1 => 65,
            Self::P256 => 128,
            Self::WebAuthn => 256,
        }
    }
}

/// EIP-8130 account-abstraction fields layered onto a standard
/// [`TransactionRequest`] for the `eth_call` / `eth_estimateGas` AA path.
///
/// All fields are optional and absent for a plain (non-8130) request; their
/// presence (via [`Eip8130RequestFields::is_some`]) marks a request as an
/// EIP-8130 simulation. Raw signatures (`sender_auth`, `payer_auth`) are never
/// passed: estimation runs without a signature. Instead a caller declares the
/// authentication *scheme* it intends to use (and, for sponsored transactions,
/// the `payer`), and the estimate prices that scheme's authentication gas by
/// synthesizing a correctly-shaped stub blob — so a caller estimates the cost
/// of any key type without first signing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Eip8130RequestFields {
    /// The 2D nonce channel key. Absent or zero is the protocol-nonce channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,
    /// Account-configuration changes applied before the calls (create,
    /// authorize/revoke actor, set delegation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_changes: Option<Vec<AccountChange>>,
    /// The phased call batches dispatched by the sender account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<Vec<Vec<Call>>>,
    /// Optional expiring-nonce expiry (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
    /// Opaque, non-executed transaction metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Bytes>,
    /// Authentication scheme the sender will use, priced into the estimate.
    /// Absent (or [`Eip8130AuthScheme::Secp256k1`]) prices the default-EOA
    /// bare-signature path; [`Eip8130AuthScheme::P256`] / `WebAuthn` price the
    /// configured-account authenticator path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_auth_scheme: Option<Eip8130AuthScheme>,
    /// Byte length of the sender's authentication payload, overriding the
    /// scheme default — set this to size a variable-length `WebAuthn` signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_auth_size: Option<u32>,
    /// Sponsoring payer account. When set, the estimate includes payer
    /// authentication gas (metered on top of the gas limit, as in execution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<Address>,
    /// Authentication scheme the payer will use (defaults to secp256k1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer_auth_scheme: Option<Eip8130AuthScheme>,
    /// Byte length of the payer's authentication payload, overriding the scheme
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer_auth_size: Option<u32>,
}

impl Eip8130RequestFields {
    /// Whether any EIP-8130 field is present, marking the request as an
    /// EIP-8130 simulation rather than a plain transaction request.
    pub const fn is_some(&self) -> bool {
        self.nonce_key.is_some()
            || self.account_changes.is_some()
            || self.calls.is_some()
            || self.expiry.is_some()
            || self.metadata.is_some()
            || self.sender_auth_scheme.is_some()
            || self.sender_auth_size.is_some()
            || self.payer.is_some()
            || self.payer_auth_scheme.is_some()
            || self.payer_auth_size.is_some()
    }
}

/// Builder for [`BaseTypedTransaction`], with optional EIP-8130 simulation
/// fields ([`Eip8130RequestFields`]) layered onto a standard
/// [`TransactionRequest`]. A request with no 8130 fields serializes and behaves
/// exactly like the underlying [`TransactionRequest`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseTransactionRequest {
    #[serde(flatten)]
    inner: TransactionRequest,
    #[serde(flatten)]
    eip8130: Eip8130RequestFields,
}

impl BaseTransactionRequest {
    /// The EIP-8130 simulation fields layered onto this request, if any are
    /// present. Returns `None` for a plain (non-8130) transaction request.
    pub const fn as_eip8130(&self) -> Option<&Eip8130RequestFields> {
        if self.eip8130.is_some() { Some(&self.eip8130) } else { None }
    }
}

impl AsRef<TransactionRequest> for BaseTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        &self.inner
    }
}

impl AsMut<TransactionRequest> for BaseTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        &mut self.inner
    }
}

impl From<TransactionRequest> for BaseTransactionRequest {
    fn from(inner: TransactionRequest) -> Self {
        Self { inner, eip8130: Eip8130RequestFields::default() }
    }
}

impl BaseTransactionRequest {
    /// Sets the `from` field in the call to the provided address
    #[inline]
    pub const fn from(mut self, from: Address) -> Self {
        self.inner.from = Some(from);
        self
    }

    /// Sets the transactions type for the transactions.
    #[doc(alias = "tx_type")]
    pub const fn transaction_type(mut self, transaction_type: u8) -> Self {
        self.inner.transaction_type = Some(transaction_type);
        self
    }

    /// Sets the gas limit for the transaction.
    pub const fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.inner.gas = Some(gas_limit);
        self
    }

    /// Sets the nonce for the transaction.
    pub const fn nonce(mut self, nonce: u64) -> Self {
        self.inner.nonce = Some(nonce);
        self
    }

    /// Sets the maximum fee per gas for the transaction.
    pub const fn max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.inner.max_fee_per_gas = Some(max_fee_per_gas);
        self
    }

    /// Sets the maximum priority fee per gas for the transaction.
    pub const fn max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.inner.max_priority_fee_per_gas = Some(max_priority_fee_per_gas);
        self
    }

    /// Sets the recipient address for the transaction.
    #[inline]
    pub const fn to(mut self, to: Address) -> Self {
        self.inner.to = Some(TxKind::Call(to));
        self
    }

    /// Sets the value (amount) for the transaction.
    pub const fn value(mut self, value: U256) -> Self {
        self.inner.value = Some(value);
        self
    }

    /// Sets the chain ID for the transaction.
    pub const fn chain_id(mut self, chain_id: ChainId) -> Self {
        self.inner.chain_id = Some(chain_id);
        self
    }

    /// Sets the input data as deploy (CREATE) bytecode.
    pub fn deploy_code(mut self, code: impl Into<Bytes>) -> Self {
        self.inner.to = Some(TxKind::Create);
        self.inner.input.input = Some(code.into());
        self
    }

    /// Sets the access list for the transaction.
    pub fn access_list(mut self, access_list: AccessList) -> Self {
        self.inner.access_list = Some(access_list);
        self
    }

    /// Sets the input data for the transaction.
    pub fn input(mut self, input: TransactionInput) -> Self {
        self.inner.input = input;
        self
    }

    /// Builds [`BaseTypedTransaction`] from this builder. See [`TransactionRequest::build_typed_tx`]
    /// for more info.
    ///
    /// Note that EIP-4844 transactions are not supported on Base chains and will be converted into
    /// EIP-1559 transactions.
    #[allow(clippy::result_large_err)]
    pub fn build_typed_tx(self) -> Result<BaseTypedTransaction, Self> {
        let Self { inner, eip8130 } = self;
        let tx = match inner.build_typed_tx() {
            Ok(tx) => tx,
            Err(inner) => return Err(Self { inner, eip8130 }),
        };
        match tx {
            TypedTransaction::Legacy(tx) => Ok(BaseTypedTransaction::Legacy(tx)),
            TypedTransaction::Eip1559(tx) => Ok(BaseTypedTransaction::Eip1559(tx)),
            TypedTransaction::Eip2930(tx) => Ok(BaseTypedTransaction::Eip2930(tx)),
            TypedTransaction::Eip4844(tx) => {
                let tx: TxEip4844 = tx.into();
                Ok(BaseTypedTransaction::Eip1559(TxEip1559 {
                    chain_id: tx.chain_id,
                    nonce: tx.nonce,
                    gas_limit: tx.gas_limit,
                    max_priority_fee_per_gas: tx.max_priority_fee_per_gas,
                    max_fee_per_gas: tx.max_fee_per_gas,
                    to: TxKind::Call(tx.to),
                    value: tx.value,
                    access_list: tx.access_list,
                    input: tx.input,
                }))
            }
            TypedTransaction::Eip7702(tx) => Ok(BaseTypedTransaction::Eip7702(tx)),
        }
    }
}

impl From<BaseTransactionRequest> for TransactionRequest {
    fn from(value: BaseTransactionRequest) -> Self {
        value.inner
    }
}

impl From<TxDeposit> for BaseTransactionRequest {
    fn from(tx: TxDeposit) -> Self {
        let TxDeposit {
            source_hash: _,
            from,
            to,
            mint: _,
            value,
            gas_limit,
            is_system_transaction: _,
            input,
        } = tx;

        TransactionRequest {
            from: Some(from),
            to: Some(to),
            value: Some(value),
            gas: Some(gas_limit),
            input: input.into(),
            ..Default::default()
        }
        .into()
    }
}

impl From<Sealed<TxDeposit>> for BaseTransactionRequest {
    fn from(value: Sealed<TxDeposit>) -> Self {
        value.into_inner().into()
    }
}

impl<T> From<Signed<T, Signature>> for BaseTransactionRequest
where
    T: SignableTransaction<Signature> + Into<TransactionRequest>,
{
    fn from(value: Signed<T, Signature>) -> Self {
        #[cfg(feature = "k256")]
        let from = value.recover_signer().ok();
        #[cfg(not(feature = "k256"))]
        let from = None;

        let mut inner: TransactionRequest = value.strip_signature().into();
        inner.from = from;

        inner.into()
    }
}

impl From<BaseTypedTransaction> for BaseTransactionRequest {
    fn from(tx: BaseTypedTransaction) -> Self {
        match tx {
            BaseTypedTransaction::Legacy(tx) => Into::<TransactionRequest>::into(tx).into(),
            BaseTypedTransaction::Eip2930(tx) => Into::<TransactionRequest>::into(tx).into(),
            BaseTypedTransaction::Eip1559(tx) => Into::<TransactionRequest>::into(tx).into(),
            BaseTypedTransaction::Eip7702(tx) => Into::<TransactionRequest>::into(tx).into(),
            BaseTypedTransaction::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::Eip8130 cannot be projected onto BaseTransactionRequest; AA transactions have no single sender/recipient/value"
            ),
            BaseTypedTransaction::Deposit(tx) => tx.into(),
        }
    }
}

impl From<BaseTxEnvelope> for BaseTransactionRequest {
    fn from(value: BaseTxEnvelope) -> Self {
        match value {
            BaseTxEnvelope::Legacy(tx) => tx.into(),
            BaseTxEnvelope::Eip2930(tx) => tx.into(),
            BaseTxEnvelope::Eip1559(tx) => tx.into(),
            BaseTxEnvelope::Eip7702(tx) => tx.into(),
            BaseTxEnvelope::Eip8130(_) => unimplemented!(
                "BaseTxEnvelope::Eip8130 cannot be projected onto BaseTransactionRequest; AA transactions have no single sender/recipient/value"
            ),
            BaseTxEnvelope::Deposit(tx) => tx.into(),
        }
    }
}

impl From<Transaction> for BaseTransactionRequest {
    fn from(value: Transaction) -> Self {
        let (tx, signer) = value.inner.inner.into_parts();
        let mut request: Self = tx.into();
        request.as_mut().from = Some(signer);
        request
    }
}

impl TransactionBuilder7702 for BaseTransactionRequest {
    fn authorization_list(&self) -> Option<&Vec<SignedAuthorization>> {
        self.as_ref().authorization_list()
    }

    fn set_authorization_list(&mut self, authorization_list: Vec<SignedAuthorization>) {
        self.as_mut().set_authorization_list(authorization_list);
    }
}

#[cfg(feature = "reth")]
impl TransactionBuilder for BaseTransactionRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.as_ref().chain_id()
    }

    fn set_chain_id(&mut self, chain_id: ChainId) {
        self.as_mut().set_chain_id(chain_id);
    }

    fn nonce(&self) -> Option<u64> {
        self.as_ref().nonce()
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.as_mut().set_nonce(nonce);
    }

    fn take_nonce(&mut self) -> Option<u64> {
        self.as_mut().nonce.take()
    }

    fn input(&self) -> Option<&Bytes> {
        self.as_ref().input()
    }

    fn set_input<T: Into<Bytes>>(&mut self, input: T) {
        self.as_mut().set_input(input);
    }

    fn from(&self) -> Option<Address> {
        self.as_ref().from()
    }

    fn set_from(&mut self, from: Address) {
        self.as_mut().set_from(from);
    }

    fn kind(&self) -> Option<TxKind> {
        self.as_ref().kind()
    }

    fn clear_kind(&mut self) {
        self.as_mut().clear_kind();
    }

    fn set_kind(&mut self, kind: TxKind) {
        self.as_mut().set_kind(kind);
    }

    fn value(&self) -> Option<U256> {
        self.as_ref().value()
    }

    fn set_value(&mut self, value: U256) {
        self.as_mut().set_value(value);
    }

    fn gas_price(&self) -> Option<u128> {
        self.as_ref().gas_price()
    }

    fn set_gas_price(&mut self, gas_price: u128) {
        self.as_mut().set_gas_price(gas_price);
    }

    fn max_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_fee_per_gas()
    }

    fn set_max_fee_per_gas(&mut self, max_fee_per_gas: u128) {
        self.as_mut().set_max_fee_per_gas(max_fee_per_gas);
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.as_ref().max_priority_fee_per_gas()
    }

    fn set_max_priority_fee_per_gas(&mut self, max_priority_fee_per_gas: u128) {
        self.as_mut().set_max_priority_fee_per_gas(max_priority_fee_per_gas);
    }

    fn gas_limit(&self) -> Option<u64> {
        self.as_ref().gas_limit()
    }

    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.as_mut().set_gas_limit(gas_limit);
    }

    fn access_list(&self) -> Option<&AccessList> {
        self.as_ref().access_list()
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.as_mut().set_access_list(access_list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_request_has_no_eip8130_fields() {
        let json = r#"{"from":"0x0000000000000000000000000000000000000001","to":"0x0000000000000000000000000000000000000002","value":"0x1"}"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        assert!(req.as_eip8130().is_none(), "a plain request is not an 8130 request");
        assert_eq!(req.as_ref().value, Some(U256::from(1u64)));

        // A plain request must not serialize any 8130 keys.
        let out = serde_json::to_string(&req).unwrap();
        assert!(!out.contains("nonceKey"));
        assert!(!out.contains("calls"));
        assert!(!out.contains("accountChanges"));
    }

    #[test]
    fn eip8130_fields_parse_alongside_the_base_request() {
        let json = r#"{
            "from":"0x0000000000000000000000000000000000000001",
            "maxFeePerGas":"0x5",
            "nonceKey":"0x2a",
            "accountChanges":[],
            "calls":[[{"to":"0x0000000000000000000000000000000000000003","data":"0x"}]]
        }"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        let aa = req.as_eip8130().expect("request carries 8130 fields");
        assert_eq!(aa.nonce_key, Some(U256::from(0x2au64)));
        assert_eq!(aa.account_changes.as_deref(), Some(&[][..]));
        let calls = aa.calls.as_ref().expect("calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        // The base fields still deserialize into the inner request.
        assert_eq!(req.as_ref().max_fee_per_gas, Some(5));
    }
}
