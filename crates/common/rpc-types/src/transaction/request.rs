use alloc::{string::String, vec::Vec};
use core::{fmt, marker::PhantomData};

use alloy_consensus::{
    Sealed, SignableTransaction, Signed, TxEip1559, TxEip4844, TypedTransaction,
};
use alloy_eips::eip7702::SignedAuthorization;
#[cfg(feature = "network")]
use alloy_network::TransactionBuilder;
use alloy_network_primitives::TransactionBuilder7702;
use alloy_primitives::{Address, Bytes, ChainId, Signature, TxKind, U256};
use alloy_rpc_types_eth::{AccessList, TransactionInput, TransactionRequest};
use base_common_consensus::{
    AccountChange, BASE_TX_TYPE_ID, BaseTxEnvelope, BaseTypedTransaction, Call, TxBaseConstants,
    TxBasePayerSerde, TxDeposit,
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, IgnoredAny, SeqAccess, Visitor},
};
use serde_json::{Map, Value};

use crate::Transaction;

/// Base transaction account-abstraction fields layered onto a standard
/// [`TransactionRequest`] for the `eth_call` / `eth_estimateGas` AA path.
///
/// All fields are optional and absent for a plain (non-Base) request; their
/// presence (via [`TxBaseRequestFields::is_some`]) marks a request as a
/// Base transaction simulation. Authentication is never verified: the caller may pass
/// the authentication blob it intends to sign (`K1_AUTHENTICATOR(20) || sig`
/// for a named account, or a bare secp256k1 signature for the default EOA),
/// and the estimate charges the fixed k1 authentication cost.
///
/// The sender account is [`sender`](Self::sender) or the standard `from`, which
/// must agree when both are present; the `sender_auth` blob's form then selects
/// the EOA vs named-account path (see
/// [`crate::BaseTransactionRequest::to_base_tx_simulation_tx`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxBaseRequestFields {
    /// The 2D nonce channel key. Absent or zero is the protocol-nonce channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,
    /// Account changes applied before the calls: at most one delegation entry,
    /// which simulation installs as inclusion would.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "TxBaseRequestFields::deserialize_account_changes"
    )]
    pub account_changes: Option<Vec<AccountChange>>,
    /// The phased call batches dispatched by the sender account.
    ///
    /// Alternatively, a single call can be given as the standard top-level
    /// `to` / `value` / `data`. Setting both `calls` and any of those is
    /// rejected as ambiguous.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "TxBaseRequestFields::deserialize_calls"
    )]
    pub calls: Option<Vec<Vec<Call>>>,
    /// Optional lower bound of the validity window, in Unix seconds or
    /// milliseconds (the unit is detected from the magnitude; `0` or absent
    /// means no lower bound). Checked as `block.timestamp * 1000 >=` the
    /// bound in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub valid_after: Option<u64>,
    /// Optional upper bound of the validity window, in Unix seconds or
    /// milliseconds (the unit is detected from the magnitude; `0` or absent
    /// means no expiry). Required (non-zero) for nonce-free transactions.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "alloy_serde::quantity::opt")]
    pub valid_before: Option<u64>,
    /// Opaque, non-executed transaction metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Bytes>,
    /// The Base transaction sender account the batch dispatches from — the wire-level
    /// `sender` identity. Interchangeable with the standard `from`: the estimate
    /// resolves the account as `sender` or `from`, and rejects a request where
    /// both are present but disagree, or where neither is set, as
    /// `INVALID_PARAMS`. A declared `sender` also selects the named-account
    /// default when `sender_auth` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<Address>,
    /// Raw sender authentication blob, never verified. Its form selects the
    /// authentication path, mirroring the on-wire transaction:
    ///
    /// - A bare 65-byte secp256k1 signature prices the default-EOA path: the
    ///   account authenticates with a k1 key, exactly as for a 1559 transaction.
    /// - `K1_AUTHENTICATOR(20) || sig` prices the named-account path.
    ///
    /// An absent blob defaults by intent: a declared `sender` synthesizes a
    /// k1-prefixed authorization; a `from`-only request synthesizes a bare
    /// secp256k1 signature. You need not sign first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_auth: Option<Bytes>,
    /// Sponsoring payer account, or `"0x00"` for open payer mode. When set, the
    /// estimate includes payer authentication gas (metered on top of the gas
    /// limit, as in execution).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "TxBasePayerSerde::serialize",
        deserialize_with = "TxBasePayerSerde::deserialize"
    )]
    pub payer: Option<Address>,
    /// Raw payer authentication blob, used when a `payer` is declared.
    ///
    /// For a named payer this is `K1_AUTHENTICATOR(20) || sig`, and absent
    /// defaults to a representative secp256k1 payer authorization. A supplied
    /// blob that does not start with the k1 authenticator is rejected as
    /// `INVALID_PARAMS`.
    ///
    /// In open payer mode this is the payer's raw 65-byte signature, from which
    /// the payer is recovered. It is optional: before any payer has signed, an
    /// absent (or unrecoverable) signature is priced as 65 bytes and the payer
    /// is simulated as the placeholder address
    /// `0x0000000000000000000000000000000000008130`, never as the sender.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer_auth: Option<Bytes>,
}

impl TxBaseRequestFields {
    /// Whether any field that defines a Base transaction request is present.
    pub const fn is_some(&self) -> bool {
        self.nonce_key.is_some()
            || self.account_changes.is_some()
            || self.calls.is_some()
            || self.valid_after.is_some()
            || self.valid_before.is_some()
            || self.metadata.is_some()
            || self.sender.is_some()
            || self.sender_auth.is_some()
            || self.payer.is_some()
            || self.payer_auth.is_some()
    }

    /// Deserializes `accountChanges`, rejecting more than
    /// [`TxBaseConstants::MAX_ACCOUNT_CHANGES_PER_TX`] entries while decoding.
    pub fn deserialize_account_changes<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<AccountChange>>, D::Error> {
        Self::deserialize_bounded(
            deserializer,
            TxBaseConstants::MAX_ACCOUNT_CHANGES_PER_TX,
            "accountChanges",
        )
    }

    /// Deserializes `calls`, rejecting more than
    /// [`TxBaseConstants::MAX_CALL_PHASES_PER_TX`] phases while decoding.
    pub fn deserialize_calls<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<Vec<Call>>>, D::Error> {
        Self::deserialize_bounded(deserializer, TxBaseConstants::MAX_CALL_PHASES_PER_TX, "calls")
    }

    /// Deserializes an optional list of at most `max` entries. The entry past
    /// `max` is skipped without being built, so an oversized list fails before
    /// it allocates more than `max` entries.
    pub fn deserialize_bounded<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
        deserializer: D,
        max: usize,
        field: &'static str,
    ) -> Result<Option<Vec<T>>, D::Error> {
        struct BoundedList<T> {
            max: usize,
            field: &'static str,
            entry: PhantomData<T>,
        }

        impl<'de, T: Deserialize<'de>> Visitor<'de> for BoundedList<T> {
            type Value = Option<Vec<T>>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "`{}` with at most {} entries", self.field, self.max)
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<Self::Value, D::Error> {
                deserializer.deserialize_seq(self)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.max));
                while entries.len() < self.max {
                    match seq.next_element()? {
                        Some(entry) => entries.push(entry),
                        None => return Ok(Some(entries)),
                    }
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(self.max + 1, &self));
                }
                Ok(Some(entries))
            }
        }

        deserializer.deserialize_option(BoundedList { max, field, entry: PhantomData })
    }
}

/// Builder for [`BaseTypedTransaction`], with optional Base transaction simulation
/// fields ([`TxBaseRequestFields`]) layered onto a standard
/// [`TransactionRequest`]. A request with no Base transaction fields serializes and behaves
/// exactly like the underlying [`TransactionRequest`].
///
/// Deserialization is a single pass over the JSON object rather than
/// `#[serde(flatten)]`, which would buffer the whole object before decoding
/// either half. The Base transaction fields decode straight into their bounded types,
/// keys neither half defines are skipped without being buffered, and only
/// [`TransactionRequest`]'s own keys are collected for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct BaseTransactionRequest {
    #[serde(flatten)]
    inner: TransactionRequest,
    #[serde(flatten)]
    base_tx: TxBaseRequestFields,
}

impl<'de> Deserialize<'de> for BaseTransactionRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Every key of both halves, so unknown keys are skipped rather than
        /// buffered. The [`TransactionRequest`] keys (including those of its
        /// flattened `input` and blob `sidecar`) are kept as raw JSON values and
        /// decoded by [`TransactionRequest`] itself.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            from: Option<Value>,
            to: Option<Value>,
            gas_price: Option<Value>,
            max_fee_per_gas: Option<Value>,
            max_priority_fee_per_gas: Option<Value>,
            max_fee_per_blob_gas: Option<Value>,
            gas: Option<Value>,
            value: Option<Value>,
            input: Option<Value>,
            data: Option<Value>,
            nonce: Option<Value>,
            chain_id: Option<Value>,
            access_list: Option<Value>,
            #[serde(rename = "type")]
            transaction_type: Option<Value>,
            blob_versioned_hashes: Option<Value>,
            blobs: Option<Value>,
            commitments: Option<Value>,
            proofs: Option<Value>,
            cell_proofs: Option<Value>,
            authorization_list: Option<Value>,
            #[serde(default)]
            nonce_key: Option<U256>,
            #[serde(
                default,
                deserialize_with = "TxBaseRequestFields::deserialize_account_changes"
            )]
            account_changes: Option<Vec<AccountChange>>,
            #[serde(default, deserialize_with = "TxBaseRequestFields::deserialize_calls")]
            calls: Option<Vec<Vec<Call>>>,
            #[serde(default, with = "alloy_serde::quantity::opt")]
            valid_after: Option<u64>,
            #[serde(default, with = "alloy_serde::quantity::opt")]
            valid_before: Option<u64>,
            #[serde(default)]
            metadata: Option<Bytes>,
            #[serde(default)]
            sender: Option<Address>,
            #[serde(default)]
            sender_auth: Option<Bytes>,
            #[serde(default, deserialize_with = "TxBasePayerSerde::deserialize")]
            payer: Option<Address>,
            #[serde(default)]
            payer_auth: Option<Bytes>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let inner: Map<String, Value> = [
            ("from", wire.from),
            ("to", wire.to),
            ("gasPrice", wire.gas_price),
            ("maxFeePerGas", wire.max_fee_per_gas),
            ("maxPriorityFeePerGas", wire.max_priority_fee_per_gas),
            ("maxFeePerBlobGas", wire.max_fee_per_blob_gas),
            ("gas", wire.gas),
            ("value", wire.value),
            ("input", wire.input),
            ("data", wire.data),
            ("nonce", wire.nonce),
            ("chainId", wire.chain_id),
            ("accessList", wire.access_list),
            ("type", wire.transaction_type),
            ("blobVersionedHashes", wire.blob_versioned_hashes),
            ("blobs", wire.blobs),
            ("commitments", wire.commitments),
            ("proofs", wire.proofs),
            ("cellProofs", wire.cell_proofs),
            ("authorizationList", wire.authorization_list),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (String::from(key), value)))
        .collect();
        let inner =
            TransactionRequest::deserialize(Value::Object(inner)).map_err(de::Error::custom)?;
        let base_tx = TxBaseRequestFields {
            nonce_key: wire.nonce_key,
            account_changes: wire.account_changes,
            calls: wire.calls,
            valid_after: wire.valid_after,
            valid_before: wire.valid_before,
            metadata: wire.metadata,
            sender: wire.sender,
            sender_auth: wire.sender_auth,
            payer: wire.payer,
            payer_auth: wire.payer_auth,
        };
        Ok(Self { inner, base_tx })
    }
}

impl BaseTransactionRequest {
    /// The Base transaction simulation fields layered onto this request, if any are
    /// present or the request declares `type: 0x79`. Returns `None` for a plain
    /// (non-Base) transaction request.
    pub fn as_base_tx(&self) -> Option<&TxBaseRequestFields> {
        let declared = self.inner.transaction_type == Some(BASE_TX_TYPE_ID);
        (declared || self.base_tx.is_some()).then_some(&self.base_tx)
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
        Self { inner, base_tx: TxBaseRequestFields::default() }
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
    /// EIP-1559 transactions. A Base transaction request is returned as an error: a typed
    /// transaction cannot carry its fields.
    #[allow(clippy::result_large_err)]
    pub fn build_typed_tx(self) -> Result<BaseTypedTransaction, Self> {
        if self.as_base_tx().is_some() {
            return Err(self);
        }
        let Self { inner, base_tx } = self;
        let tx = match inner.build_typed_tx() {
            Ok(tx) => tx,
            Err(inner) => return Err(Self { inner, base_tx }),
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
            BaseTypedTransaction::Base(_) => unimplemented!(
                "BaseTypedTransaction::Base cannot be projected onto BaseTransactionRequest; AA transactions have no single sender/recipient/value"
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
            BaseTxEnvelope::Base(_) => unimplemented!(
                "BaseTxEnvelope::Base cannot be projected onto BaseTransactionRequest; AA transactions have no single sender/recipient/value"
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

#[cfg(feature = "network")]
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
    use alloy_eips::{
        eip2930::AccessListItem,
        eip4844::{BlobTransactionSidecar, Bytes48},
        eip7594::{BlobTransactionSidecarEip7594, BlobTransactionSidecarVariant},
    };
    use alloy_primitives::{B256, address};
    use base_common_consensus::Delegation;
    use serde_json::json;

    use super::*;

    const SENDER: Address = address!("0x00000000000000000000000000000000000000a1");

    /// A request with every [`TransactionRequest`] field set (an exhaustive
    /// literal, so a field added upstream fails to compile here) and every
    /// Base transaction field set.
    fn full_request(sidecar: BlobTransactionSidecarVariant) -> BaseTransactionRequest {
        let inner = TransactionRequest {
            from: Some(SENDER),
            to: Some(TxKind::Call(Address::repeat_byte(0x02))),
            gas_price: Some(1),
            max_fee_per_gas: Some(2),
            max_priority_fee_per_gas: Some(3),
            max_fee_per_blob_gas: Some(4),
            gas: Some(21_000),
            value: Some(U256::from(5)),
            input: TransactionInput::both(Bytes::from_static(&[0xab])),
            nonce: Some(6),
            chain_id: Some(8453),
            access_list: Some(AccessList(vec![AccessListItem {
                address: Address::repeat_byte(0x03),
                storage_keys: vec![B256::repeat_byte(0x04)],
            }])),
            transaction_type: Some(BASE_TX_TYPE_ID),
            blob_versioned_hashes: Some(vec![B256::repeat_byte(0x05)]),
            sidecar: Some(sidecar),
            authorization_list: Some(Vec::new()),
        };
        let base_tx = TxBaseRequestFields {
            nonce_key: Some(U256::from(7)),
            account_changes: Some(vec![AccountChange::Delegation(Delegation {
                target: Address::repeat_byte(0x06),
            })]),
            calls: Some(vec![vec![Call {
                to: Address::repeat_byte(0x07),
                value: U256::from(8),
                data: Bytes::from_static(&[0xcd]),
            }]]),
            valid_after: Some(9),
            valid_before: Some(10),
            metadata: Some(Bytes::from_static(&[0xef])),
            sender: Some(SENDER),
            sender_auth: Some(Bytes::from_static(&[0x01])),
            payer: Some(Address::repeat_byte(0x08)),
            payer_auth: Some(Bytes::from_static(&[0x02])),
        };
        BaseTransactionRequest { inner, base_tx }
    }

    #[test]
    fn every_field_survives_a_json_round_trip() {
        let eip4844 = BlobTransactionSidecarVariant::Eip4844(BlobTransactionSidecar {
            blobs: Vec::new(),
            commitments: vec![Bytes48::repeat_byte(0x0a)],
            proofs: vec![Bytes48::repeat_byte(0x0b)],
        });
        let eip7594 = BlobTransactionSidecarVariant::Eip7594(BlobTransactionSidecarEip7594 {
            blobs: Vec::new(),
            commitments: vec![Bytes48::repeat_byte(0x0a)],
            cell_proofs: vec![Bytes48::repeat_byte(0x0c)],
        });
        for sidecar in [eip4844, eip7594] {
            let request = full_request(sidecar);
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<BaseTransactionRequest>(&json).unwrap(), request);
        }
    }

    /// A request built in code skips decoding, so the structural check run by
    /// simulation is what bounds it.
    #[cfg(feature = "base-tx")]
    #[test]
    fn oversized_calls_built_in_code_fail_the_structural_check() {
        let mut request = BaseTransactionRequest::default();
        request.base_tx.sender = Some(SENDER);
        request.base_tx.calls = Some(vec![Vec::new(); TxBaseConstants::MAX_CALL_PHASES_PER_TX + 1]);
        assert_eq!(
            request.to_base_tx_simulation_tx(8453, 30_000_000).err(),
            Some(crate::TxBaseSimulationRequestError::Structural(
                base_common_consensus::TxBaseStructuralError::TooManyCallPhases
            ))
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let request: BaseTransactionRequest = serde_json::from_value(json!({
            "sender": SENDER,
            "unknown": [[], [], {"nested": [1, 2, 3]}],
        }))
        .unwrap();
        assert_eq!(
            request,
            serde_json::from_value::<BaseTransactionRequest>(json!({ "sender": SENDER })).unwrap()
        );
    }

    #[test]
    fn base_tx_lists_are_bounded_while_decoding() {
        let decode = |calls: usize, account_changes: usize| {
            let delegation = json!({ "type": "0x1", "target": Address::repeat_byte(0x06) });
            serde_json::from_str::<BaseTransactionRequest>(
                &json!({
                    "sender": SENDER,
                    "calls": vec![Vec::<Call>::new(); calls],
                    "accountChanges": vec![delegation; account_changes],
                })
                .to_string(),
            )
        };
        let max_calls = TxBaseConstants::MAX_CALL_PHASES_PER_TX;
        let max_changes = TxBaseConstants::MAX_ACCOUNT_CHANGES_PER_TX;

        let request = decode(max_calls, max_changes).unwrap();
        let fields = request.as_base_tx().unwrap();
        assert_eq!(fields.calls.as_ref().unwrap().len(), max_calls);
        assert_eq!(fields.account_changes.as_ref().unwrap().len(), max_changes);

        let error = decode(max_calls + 1, max_changes).unwrap_err().to_string();
        assert!(error.contains("`calls` with at most 1024 entries"), "{error}");
        let error = decode(max_calls, max_changes + 1).unwrap_err().to_string();
        assert!(error.contains("`accountChanges` with at most 1 entries"), "{error}");
    }

    #[test]
    fn plain_request_has_no_base_tx_fields() {
        let json = r#"{"from":"0x0000000000000000000000000000000000000001","to":"0x0000000000000000000000000000000000000002","value":"0x1"}"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        assert!(req.as_base_tx().is_none(), "a plain request is not a Base transaction request");
        assert_eq!(req.as_ref().value, Some(U256::from(1u64)));

        let out = serde_json::to_string(&req).unwrap();
        assert!(!out.contains("nonceKey"));
        assert!(!out.contains("calls"));
        assert!(!out.contains("accountChanges"));
    }

    #[test]
    fn base_tx_fields_parse_alongside_the_base_request() {
        let json = r#"{
            "from":"0x0000000000000000000000000000000000000001",
            "maxFeePerGas":"0x5",
            "nonceKey":"0x2a",
            "accountChanges":[],
            "calls":[[{"to":"0x0000000000000000000000000000000000000003","data":"0x"}]]
        }"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        let aa = req.as_base_tx().expect("request carries Base transaction fields");
        assert_eq!(aa.nonce_key, Some(U256::from(0x2au64)));
        assert_eq!(aa.account_changes.as_deref(), Some(&[][..]));
        let calls = aa.calls.as_ref().expect("calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].len(), 1);
        assert_eq!(req.as_ref().max_fee_per_gas, Some(5));
    }

    #[test]
    fn base_tx_type_alone_marks_request_as_base_tx() {
        let json = r#"{"from":"0x0000000000000000000000000000000000000001","type":"0x79"}"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        assert!(req.as_base_tx().is_some(), "type 0x79 must route the request onto the AA path");
    }

    #[test]
    fn base_tx_request_does_not_build_a_typed_tx() {
        let json = r#"{"from":"0x0000000000000000000000000000000000000001","nonce":"0x0","gas":"0x5208","maxFeePerGas":"0x1","maxPriorityFeePerGas":"0x1","type":"0x79"}"#;
        let req: BaseTransactionRequest = serde_json::from_str(json).unwrap();
        assert!(
            req.build_typed_tx().is_err(),
            "a Base transaction request must not become a plain tx"
        );
    }
}
