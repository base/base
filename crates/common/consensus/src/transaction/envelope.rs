use alloy_consensus::{
    Extended, InMemorySize, Sealable, Sealed, SignableTransaction, Signed, TransactionEnvelope,
    TxEip1559, TxEip2930, TxEip7702, TxEnvelope, TxLegacy,
    error::ValueError,
    transaction::{TransactionInfo, TxHashRef},
};
use alloy_eips::eip2718::Encodable2718;
#[cfg(feature = "evm")]
use alloy_evm::{FromRecoveredTx, FromTxWithEncoded};
#[cfg(feature = "alloy-compat")]
use alloy_network::{AnyRpcTransaction, AnyTxEnvelope};
use alloy_primitives::{B256, Bytes, Signature, TxHash};
#[cfg(feature = "alloy-compat")]
use alloy_rpc_types_eth::{ConversionError, Transaction as AlloyRpcTransaction};
#[cfg(feature = "alloy-compat")]
use alloy_serde::WithOtherFields;
#[cfg(feature = "evm")]
use revm::context::TxEnv;

use crate::{
    BasePooledTransaction, TxDeposit,
    transaction::{BaseTransactionInfo, DepositInfo, Eip8130Signed, TxEip8130},
};

/// The Ethereum [EIP-2718] Transaction Envelope, modified for Base.
///
/// # Note:
///
/// This enum distinguishes between tagged and untagged legacy transactions, as
/// the in-protocol merkle tree may commit to EITHER 0-prefixed or raw.
/// Therefore we must ensure that encoding returns the precise byte-array that
/// was decoded, preserving the presence or absence of the `TransactionType`
/// flag.
///
/// [EIP-2718]: https://eips.ethereum.org/EIPS/eip-2718
#[derive(Debug, Clone, TransactionEnvelope)]
#[envelope(tx_type_name = OpTxType, typed = BaseTypedTransaction, serde_cfg(feature = "serde"))]
pub enum BaseTxEnvelope {
    /// An untagged [`TxLegacy`].
    #[envelope(ty = 0)]
    Legacy(Signed<TxLegacy>),
    /// A [`TxEip2930`] tagged with type 1.
    #[envelope(ty = 1)]
    Eip2930(Signed<TxEip2930>),
    /// A [`TxEip1559`] tagged with type 2.
    #[envelope(ty = 2)]
    Eip1559(Signed<TxEip1559>),
    /// A [`TxEip7702`] tagged with type 4.
    #[envelope(ty = 4)]
    Eip7702(Signed<TxEip7702>),
    /// A [`TxDeposit`] tagged with type 0x7E.
    #[envelope(ty = 126)]
    #[serde(serialize_with = "crate::serde_deposit_tx_rpc")]
    Deposit(Sealed<TxDeposit>),
    /// An [EIP-8130] Account Abstraction transaction tagged with type 0x7D.
    ///
    /// [EIP-8130]: https://eips.ethereum.org/EIPS/eip-8130
    #[envelope(ty = 125, typed = TxEip8130)]
    Eip8130(Eip8130Signed),
}

/// Represents a transaction envelope for Base chains.
///
/// Compared to Ethereum it can tell whether the transaction is a deposit.
pub trait BaseTransaction {
    /// Returns `true` if the transaction is a deposit.
    fn is_deposit(&self) -> bool;

    /// Returns `Some` if the transaction is a deposit.
    fn as_deposit(&self) -> Option<&Sealed<TxDeposit>>;
}

impl BaseTransaction for BaseTxEnvelope {
    fn is_deposit(&self) -> bool {
        self.is_deposit()
    }

    fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        self.as_deposit()
    }
}

impl<B, T> BaseTransaction for Extended<B, T>
where
    B: BaseTransaction,
    T: BaseTransaction,
{
    fn is_deposit(&self) -> bool {
        match self {
            Self::BuiltIn(b) => b.is_deposit(),
            Self::Other(t) => t.is_deposit(),
        }
    }

    fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        match self {
            Self::BuiltIn(b) => b.as_deposit(),
            Self::Other(t) => t.as_deposit(),
        }
    }
}

impl AsRef<Self> for BaseTxEnvelope {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl From<Signed<TxLegacy>> for BaseTxEnvelope {
    fn from(v: Signed<TxLegacy>) -> Self {
        Self::Legacy(v)
    }
}

impl From<Signed<TxEip2930>> for BaseTxEnvelope {
    fn from(v: Signed<TxEip2930>) -> Self {
        Self::Eip2930(v)
    }
}

impl From<Signed<TxEip1559>> for BaseTxEnvelope {
    fn from(v: Signed<TxEip1559>) -> Self {
        Self::Eip1559(v)
    }
}

impl From<Signed<TxEip7702>> for BaseTxEnvelope {
    fn from(v: Signed<TxEip7702>) -> Self {
        Self::Eip7702(v)
    }
}

impl From<TxDeposit> for BaseTxEnvelope {
    fn from(v: TxDeposit) -> Self {
        v.seal_slow().into()
    }
}

impl From<Signed<BaseTypedTransaction>> for BaseTxEnvelope {
    fn from(value: Signed<BaseTypedTransaction>) -> Self {
        let (tx, sig, hash) = value.into_parts();
        match tx {
            BaseTypedTransaction::Legacy(tx_legacy) => {
                let tx = Signed::new_unchecked(tx_legacy, sig, hash);
                Self::Legacy(tx)
            }
            BaseTypedTransaction::Eip2930(tx_eip2930) => {
                let tx = Signed::new_unchecked(tx_eip2930, sig, hash);
                Self::Eip2930(tx)
            }
            BaseTypedTransaction::Eip1559(tx_eip1559) => {
                let tx = Signed::new_unchecked(tx_eip1559, sig, hash);
                Self::Eip1559(tx)
            }
            BaseTypedTransaction::Eip7702(tx_eip7702) => {
                let tx = Signed::new_unchecked(tx_eip7702, sig, hash);
                Self::Eip7702(tx)
            }
            BaseTypedTransaction::Eip8130(tx) => {
                debug_assert!(
                    tx.sender.is_none(),
                    "configured-owner EIP-8130 transactions must not be wrapped through the ECDSA Signed<BaseTypedTransaction> path; route them via BaseTxEnvelope::Eip8130 directly with the appropriate sender_auth",
                );
                debug_assert!(
                    tx.payer.is_none(),
                    "sponsored EIP-8130 transactions must not be wrapped through the ECDSA Signed<BaseTypedTransaction> path; the payer_auth would be silently dropped",
                );
                Self::Eip8130(Eip8130Signed::new(tx, sig.as_bytes().into(), Bytes::new()))
            }
            BaseTypedTransaction::Deposit(tx) => Self::Deposit(Sealed::new_unchecked(tx, hash)),
        }
    }
}

impl From<(BaseTypedTransaction, Signature)> for BaseTxEnvelope {
    fn from(value: (BaseTypedTransaction, Signature)) -> Self {
        Self::new_unhashed(value.0, value.1)
    }
}

impl From<Sealed<TxDeposit>> for BaseTxEnvelope {
    fn from(v: Sealed<TxDeposit>) -> Self {
        Self::Deposit(v)
    }
}

impl<Tx> From<BaseTxEnvelope> for Extended<BaseTxEnvelope, Tx> {
    fn from(value: BaseTxEnvelope) -> Self {
        Self::BuiltIn(value)
    }
}

impl TryFrom<TxEnvelope> for BaseTxEnvelope {
    type Error = TxEnvelope;

    fn try_from(value: TxEnvelope) -> Result<Self, Self::Error> {
        Self::try_from_eth_envelope(value)
    }
}

impl TryFrom<BaseTxEnvelope> for TxEnvelope {
    type Error = ValueError<BaseTxEnvelope>;

    fn try_from(value: BaseTxEnvelope) -> Result<Self, Self::Error> {
        value.try_into_eth_envelope()
    }
}

#[cfg(feature = "evm")]
impl FromRecoveredTx<BaseTxEnvelope> for TxEnv {
    fn from_recovered_tx(tx: &BaseTxEnvelope, caller: alloy_primitives::Address) -> Self {
        match tx {
            BaseTxEnvelope::Legacy(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip1559(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip2930(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip7702(tx) => Self::from_recovered_tx(tx.tx(), caller),
            BaseTxEnvelope::Eip8130(_) => {
                unimplemented!("EIP-8130 AA transactions cannot be converted to TxEnv yet")
            }
            BaseTxEnvelope::Deposit(tx) => Self::from_recovered_tx(tx.inner(), caller),
        }
    }
}

#[cfg(feature = "evm")]
impl FromTxWithEncoded<BaseTxEnvelope> for TxEnv {
    fn from_encoded_tx(
        tx: &BaseTxEnvelope,
        caller: alloy_primitives::Address,
        _encoded: alloy_primitives::Bytes,
    ) -> Self {
        Self::from_recovered_tx(tx, caller)
    }
}

#[cfg(feature = "alloy-compat")]
impl From<BaseTxEnvelope> for alloy_rpc_types_eth::TransactionRequest {
    fn from(value: BaseTxEnvelope) -> Self {
        match value {
            BaseTxEnvelope::Eip2930(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip1559(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip7702(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Legacy(tx) => tx.into_parts().0.into(),
            BaseTxEnvelope::Eip8130(_) => unimplemented!(
                "BaseTxEnvelope::Eip8130 cannot be converted to an alloy TransactionRequest; AA transactions have no single sender/recipient/value to project into the legacy request shape"
            ),
            BaseTxEnvelope::Deposit(tx) => tx.into_inner().into(),
        }
    }
}

#[cfg(feature = "alloy-compat")]
impl TryFrom<AnyTxEnvelope> for BaseTxEnvelope {
    type Error = AnyTxEnvelope;

    fn try_from(value: AnyTxEnvelope) -> Result<Self, Self::Error> {
        Self::try_from_any_envelope(value)
    }
}

#[cfg(feature = "alloy-compat")]
impl TryFrom<AnyRpcTransaction> for BaseTxEnvelope {
    type Error = ConversionError;

    fn try_from(tx: AnyRpcTransaction) -> Result<Self, Self::Error> {
        let WithOtherFields { inner: AlloyRpcTransaction { inner, .. }, other: _ } = tx.0;

        let from = inner.signer();
        match inner.into_inner() {
            AnyTxEnvelope::Ethereum(tx) => Self::try_from_eth_envelope(tx).map_err(|_| {
                ConversionError::Custom("unable to convert from ethereum type".to_string())
            }),
            AnyTxEnvelope::Unknown(mut tx) => {
                // Re-insert `from` field which was consumed by outer `Transaction`.
                // Ref hack in op-alloy <https://github.com/alloy-rs/op-alloy/blob/7d50b698631dd73f8d20f9f60ee78cd0597dc278/crates/rpc-types/src/transaction.rs#L236-L237>
                tx.inner
                    .fields
                    .insert_value("from".to_string(), from)
                    .map_err(|err| ConversionError::Custom(err.to_string()))?;
                Ok(Self::Deposit(Sealed::new(tx.try_into()?)))
            }
        }
    }
}

impl BaseTxEnvelope {
    /// Creates a new enveloped transaction from the given transaction, signature and hash.
    ///
    /// Caution: This assumes the given hash is the correct transaction hash.
    pub fn new_unchecked(
        transaction: BaseTypedTransaction,
        signature: Signature,
        hash: B256,
    ) -> Self {
        Signed::new_unchecked(transaction, signature, hash).into()
    }

    /// Creates a new signed transaction from the given typed transaction and signature without the
    /// hash.
    ///
    /// Note: this only calculates the hash on the first [`BaseTxEnvelope::hash`] call.
    pub fn new_unhashed(transaction: BaseTypedTransaction, signature: Signature) -> Self {
        transaction.into_signed(signature).into()
    }

    /// Returns true if the transaction is a legacy transaction.
    #[inline]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy(_))
    }

    /// Returns true if the transaction is an EIP-2930 transaction.
    #[inline]
    pub const fn is_eip2930(&self) -> bool {
        matches!(self, Self::Eip2930(_))
    }

    /// Returns true if the transaction is an EIP-1559 transaction.
    #[inline]
    pub const fn is_eip1559(&self) -> bool {
        matches!(self, Self::Eip1559(_))
    }

    /// Returns true if the transaction is a system transaction.
    #[inline]
    pub const fn is_system_transaction(&self) -> bool {
        match self {
            Self::Deposit(tx) => tx.inner().is_system_transaction,
            _ => false,
        }
    }

    /// Attempts to convert the envelope into the pooled variant.
    ///
    /// Returns an error if the envelope's variant is incompatible with the pooled format:
    /// [`TxDeposit`].
    pub fn try_into_pooled(self) -> Result<BasePooledTransaction, ValueError<Self>> {
        match self {
            Self::Legacy(tx) => Ok(tx.into()),
            Self::Eip2930(tx) => Ok(tx.into()),
            Self::Eip1559(tx) => Ok(tx.into()),
            Self::Eip7702(tx) => Ok(tx.into()),
            Self::Eip8130(tx) => Ok(tx.into()),
            Self::Deposit(tx) => {
                Err(ValueError::new(tx.into(), "Deposit transactions cannot be pooled"))
            }
        }
    }

    /// Attempts to convert the envelope into the ethereum pooled variant.
    ///
    /// Returns an error if the envelope's variant is incompatible with the ethereum pooled
    /// format: [`TxDeposit`] (not pooled at all) or [`Eip8130Signed`] (pooled, but has no
    /// ethereum-format representation since the alloy `PooledTransaction` enum has no
    /// EIP-8130 variant). Rejecting [`Eip8130Signed`] here prevents
    /// `From<BasePooledTransaction> for alloy_consensus::PooledTransaction` from panicking.
    pub fn try_into_eth_pooled(
        self,
    ) -> Result<alloy_consensus::transaction::PooledTransaction, ValueError<Self>> {
        match self {
            tx @ Self::Eip8130(_) => Err(ValueError::new(
                tx,
                "EIP-8130 transactions cannot be converted to ethereum PooledTransaction",
            )),
            other => other.try_into_pooled().map(Into::into),
        }
    }

    /// Attempts to convert the L2 variant into an ethereum [`TxEnvelope`].
    ///
    /// Returns the envelope as error if it is a variant unsupported on ethereum: [`TxDeposit`]
    pub fn try_into_eth_envelope(self) -> Result<TxEnvelope, ValueError<Self>> {
        match self {
            Self::Legacy(tx) => Ok(tx.into()),
            Self::Eip2930(tx) => Ok(tx.into()),
            Self::Eip1559(tx) => Ok(tx.into()),
            Self::Eip7702(tx) => Ok(tx.into()),
            tx @ Self::Eip8130(_) => Err(ValueError::new(
                tx,
                "EIP-8130 transactions cannot be converted to ethereum transaction",
            )),
            tx @ Self::Deposit(_) => Err(ValueError::new(
                tx,
                "Deposit transactions cannot be converted to ethereum transaction",
            )),
        }
    }

    /// Helper that creates [`BaseTransactionInfo`] by adding [`DepositInfo`] obtained from the
    /// given closure if this transaction is a deposit and return the [`BaseTransactionInfo`].
    pub fn try_to_tx_info<F, E>(
        &self,
        tx_info: TransactionInfo,
        f: F,
    ) -> Result<BaseTransactionInfo, E>
    where
        F: FnOnce(TxHash) -> Result<Option<DepositInfo>, E>,
    {
        let deposit_meta =
            if self.is_deposit() { f(self.tx_hash())? } else { None }.unwrap_or_default();

        Ok(BaseTransactionInfo::new(tx_info, deposit_meta))
    }

    /// Attempts to convert an ethereum [`TxEnvelope`] into the L2 variant.
    ///
    /// Returns the given envelope as error if [`BaseTxEnvelope`] doesn't support the variant
    /// (EIP-4844)
    #[allow(clippy::result_large_err)]
    pub fn try_from_eth_envelope(tx: TxEnvelope) -> Result<Self, TxEnvelope> {
        match tx {
            TxEnvelope::Legacy(tx) => Ok(tx.into()),
            TxEnvelope::Eip2930(tx) => Ok(tx.into()),
            TxEnvelope::Eip1559(tx) => Ok(tx.into()),
            tx @ TxEnvelope::Eip4844(_) => Err(tx),
            TxEnvelope::Eip7702(tx) => Ok(tx.into()),
        }
    }

    /// Returns mutable access to the input bytes.
    ///
    /// Caution: modifying this will cause side-effects on the hash.
    ///
    /// Panics for [`Self::Eip8130`] since EIP-8130 transactions have no single
    /// input field; their payload is a list of calls.
    #[doc(hidden)]
    pub fn input_mut(&mut self) -> &mut Bytes {
        match self {
            Self::Eip1559(tx) => &mut tx.tx_mut().input,
            Self::Eip2930(tx) => &mut tx.tx_mut().input,
            Self::Legacy(tx) => &mut tx.tx_mut().input,
            Self::Eip7702(tx) => &mut tx.tx_mut().input,
            Self::Eip8130(_) => {
                unimplemented!("EIP-8130 transactions have no single input field")
            }
            Self::Deposit(tx) => &mut tx.inner_mut().input,
        }
    }

    /// Attempts to convert an ethereum [`TxEnvelope`] into the L2 variant.
    ///
    /// Returns the given envelope as error if [`BaseTxEnvelope`] doesn't support the variant
    /// (EIP-4844)
    #[cfg(feature = "alloy-compat")]
    #[allow(clippy::result_large_err)]
    pub fn try_from_any_envelope(
        tx: alloy_network::AnyTxEnvelope,
    ) -> Result<Self, alloy_network::AnyTxEnvelope> {
        match tx.try_into_envelope() {
            Ok(eth) => {
                Self::try_from_eth_envelope(eth).map_err(alloy_network::AnyTxEnvelope::Ethereum)
            }
            Err(err) => match err.into_value() {
                alloy_network::AnyTxEnvelope::Unknown(unknown) => {
                    let Ok(deposit) = unknown.inner.clone().try_into() else {
                        return Err(alloy_network::AnyTxEnvelope::Unknown(unknown));
                    };
                    Ok(Self::Deposit(Sealed::new_unchecked(deposit, unknown.hash)))
                }
                unsupported => Err(unsupported),
            },
        }
    }

    /// Returns true if the transaction is a deposit transaction.
    #[inline]
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Returns true if the transaction is an EIP-8130 AA transaction.
    #[inline]
    pub const fn is_eip8130(&self) -> bool {
        matches!(self, Self::Eip8130(_))
    }

    /// Returns the [`TxLegacy`] variant if the transaction is a legacy transaction.
    pub const fn as_legacy(&self) -> Option<&Signed<TxLegacy>> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip2930`] variant if the transaction is an EIP-2930 transaction.
    pub const fn as_eip2930(&self) -> Option<&Signed<TxEip2930>> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxEip1559`] variant if the transaction is an EIP-1559 transaction.
    pub const fn as_eip1559(&self) -> Option<&Signed<TxEip1559>> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`TxDeposit`] variant if the transaction is a deposit transaction.
    pub const fn as_deposit(&self) -> Option<&Sealed<TxDeposit>> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns the [`Eip8130Signed`] variant if the transaction is an EIP-8130 AA transaction.
    pub const fn as_eip8130(&self) -> Option<&Eip8130Signed> {
        match self {
            Self::Eip8130(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the reference to signature.
    ///
    /// Returns `None` if this is a deposit or EIP-8130 variant.
    pub const fn signature(&self) -> Option<&Signature> {
        match self {
            Self::Legacy(tx) => Some(tx.signature()),
            Self::Eip2930(tx) => Some(tx.signature()),
            Self::Eip1559(tx) => Some(tx.signature()),
            Self::Eip7702(tx) => Some(tx.signature()),
            Self::Eip8130(_) | Self::Deposit(_) => None,
        }
    }

    /// Return the [`OpTxType`] of the inner txn.
    pub const fn tx_type(&self) -> OpTxType {
        match self {
            Self::Legacy(_) => OpTxType::Legacy,
            Self::Eip2930(_) => OpTxType::Eip2930,
            Self::Eip1559(_) => OpTxType::Eip1559,
            Self::Eip7702(_) => OpTxType::Eip7702,
            Self::Eip8130(_) => OpTxType::Eip8130,
            Self::Deposit(_) => OpTxType::Deposit,
        }
    }

    /// Returns the inner transaction hash.
    pub fn hash(&self) -> &B256 {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Eip8130(tx) => tx.hash(),
            Self::Deposit(tx) => tx.hash_ref(),
        }
    }

    /// Returns the inner transaction hash.
    pub fn tx_hash(&self) -> B256 {
        *self.hash()
    }

    /// Return the length of the inner txn, including type byte length
    pub fn eip2718_encoded_length(&self) -> usize {
        match self {
            Self::Legacy(t) => t.eip2718_encoded_length(),
            Self::Eip2930(t) => t.eip2718_encoded_length(),
            Self::Eip1559(t) => t.eip2718_encoded_length(),
            Self::Eip7702(t) => t.eip2718_encoded_length(),
            Self::Eip8130(t) => t.encode_2718_len(),
            Self::Deposit(t) => t.eip2718_encoded_length(),
        }
    }
}

impl TxHashRef for BaseTxEnvelope {
    fn tx_hash(&self) -> &B256 {
        Self::hash(self)
    }
}

#[cfg(feature = "k256")]
impl alloy_consensus::transaction::SignerRecoverable for BaseTxEnvelope {
    fn recover_signer(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
            Self::Eip8130(tx) => return tx.recover_sender(),
            // The Deposit transaction does not have a signature. Directly return the
            // `from` address.
            Self::Deposit(tx) => return Ok(tx.from),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Eip8130(_) | Self::Deposit(_) => {
                unreachable!("non-ECDSA variants short-circuit above")
            }
        };
        alloy_consensus::crypto::secp256k1::recover_signer(signature, signature_hash)
    }

    fn recover_signer_unchecked(
        &self,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        let signature_hash = match self {
            Self::Legacy(tx) => tx.signature_hash(),
            Self::Eip2930(tx) => tx.signature_hash(),
            Self::Eip1559(tx) => tx.signature_hash(),
            Self::Eip7702(tx) => tx.signature_hash(),
            Self::Eip8130(tx) => return tx.recover_sender_unchecked(),
            // The Deposit transaction does not have a signature. Directly return the
            // `from` address.
            Self::Deposit(tx) => return Ok(tx.from),
        };
        let signature = match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Eip8130(_) | Self::Deposit(_) => {
                unreachable!("non-ECDSA variants short-circuit above")
            }
        };
        alloy_consensus::crypto::secp256k1::recover_signer_unchecked(signature, signature_hash)
    }

    fn recover_unchecked_with_buf(
        &self,
        buf: &mut alloc::vec::Vec<u8>,
    ) -> Result<alloy_primitives::Address, alloy_consensus::crypto::RecoveryError> {
        match self {
            Self::Legacy(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip2930(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip1559(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip7702(tx) => {
                alloy_consensus::transaction::SignerRecoverable::recover_unchecked_with_buf(tx, buf)
            }
            Self::Eip8130(tx) => tx.recover_sender_unchecked(),
            Self::Deposit(tx) => Ok(tx.from),
        }
    }
}

/// Bincode-compatible serde implementation for [`BaseTxEnvelope`].
#[cfg(all(feature = "serde", feature = "serde-bincode-compat"))]
pub(super) mod serde_bincode_compat {
    use alloy_consensus::{
        Sealed, Signed,
        transaction::serde_bincode_compat::{TxEip1559, TxEip2930, TxEip7702, TxLegacy},
    };
    use alloy_primitives::{B256, Signature};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_with::{DeserializeAs, SerializeAs};

    use crate::{serde_bincode_compat::TxDeposit, transaction::Eip8130Signed};

    /// Bincode-compatible representation of an [`BaseTxEnvelope`].
    #[derive(Debug, Serialize, Deserialize)]
    pub enum BaseTxEnvelope<'a> {
        /// Legacy variant.
        Legacy {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed legacy transaction data.
            transaction: TxLegacy<'a>,
        },
        /// EIP-2930 variant.
        Eip2930 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-2930 transaction data.
            transaction: TxEip2930<'a>,
        },
        /// EIP-1559 variant.
        Eip1559 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-1559 transaction data.
            transaction: TxEip1559<'a>,
        },
        /// EIP-7702 variant.
        Eip7702 {
            /// Transaction signature.
            signature: Signature,
            /// Borrowed EIP-7702 transaction data.
            transaction: TxEip7702<'a>,
        },
        /// Deposit variant.
        Deposit {
            /// Precomputed hash.
            hash: B256,
            /// Borrowed deposit transaction data.
            transaction: TxDeposit<'a>,
        },
        /// EIP-8130 Account Abstraction variant.
        Eip8130 {
            /// Owned [`Eip8130Signed`] envelope.
            ///
            /// The [`Eip8130Signed`] payload includes variable-length `calls`,
            /// `account_changes`, and authentication buffers, so we serialize
            /// it directly instead of borrowing a flattened bincode-friendly
            /// projection.
            transaction: Eip8130Signed,
        },
    }

    impl<'a> From<&'a super::BaseTxEnvelope> for BaseTxEnvelope<'a> {
        fn from(value: &'a super::BaseTxEnvelope) -> Self {
            match value {
                super::BaseTxEnvelope::Legacy(signed_legacy) => Self::Legacy {
                    signature: *signed_legacy.signature(),
                    transaction: signed_legacy.tx().into(),
                },
                super::BaseTxEnvelope::Eip2930(signed_2930) => Self::Eip2930 {
                    signature: *signed_2930.signature(),
                    transaction: signed_2930.tx().into(),
                },
                super::BaseTxEnvelope::Eip1559(signed_1559) => Self::Eip1559 {
                    signature: *signed_1559.signature(),
                    transaction: signed_1559.tx().into(),
                },
                super::BaseTxEnvelope::Eip7702(signed_7702) => Self::Eip7702 {
                    signature: *signed_7702.signature(),
                    transaction: signed_7702.tx().into(),
                },
                super::BaseTxEnvelope::Eip8130(eip8130_signed) => {
                    Self::Eip8130 { transaction: eip8130_signed.clone() }
                }
                super::BaseTxEnvelope::Deposit(sealed_deposit) => Self::Deposit {
                    hash: sealed_deposit.seal(),
                    transaction: sealed_deposit.inner().into(),
                },
            }
        }
    }

    impl<'a> From<BaseTxEnvelope<'a>> for super::BaseTxEnvelope {
        fn from(value: BaseTxEnvelope<'a>) -> Self {
            match value {
                BaseTxEnvelope::Legacy { signature, transaction } => {
                    Self::Legacy(Signed::new_unhashed(transaction.into(), signature))
                }
                BaseTxEnvelope::Eip2930 { signature, transaction } => {
                    Self::Eip2930(Signed::new_unhashed(transaction.into(), signature))
                }
                BaseTxEnvelope::Eip1559 { signature, transaction } => {
                    Self::Eip1559(Signed::new_unhashed(transaction.into(), signature))
                }
                BaseTxEnvelope::Eip7702 { signature, transaction } => {
                    Self::Eip7702(Signed::new_unhashed(transaction.into(), signature))
                }
                BaseTxEnvelope::Eip8130 { transaction } => Self::Eip8130(transaction),
                BaseTxEnvelope::Deposit { hash, transaction } => {
                    Self::Deposit(Sealed::new_unchecked(transaction.into(), hash))
                }
            }
        }
    }

    impl SerializeAs<super::BaseTxEnvelope> for BaseTxEnvelope<'_> {
        fn serialize_as<S>(source: &super::BaseTxEnvelope, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let borrowed = BaseTxEnvelope::from(source);
            borrowed.serialize(serializer)
        }
    }

    impl<'de> DeserializeAs<'de, super::BaseTxEnvelope> for BaseTxEnvelope<'de> {
        fn deserialize_as<D>(deserializer: D) -> Result<super::BaseTxEnvelope, D::Error>
        where
            D: Deserializer<'de>,
        {
            let borrowed = BaseTxEnvelope::deserialize(deserializer)?;
            Ok(borrowed.into())
        }
    }

    #[cfg(test)]
    mod tests {
        use arbitrary::Arbitrary;
        use rand::Rng;
        use serde::{Deserialize, Serialize};
        use serde_with::serde_as;

        use super::*;

        /// Tests a bincode round-trip for [`BaseTxEnvelope`] using an arbitrary instance.
        #[test]
        fn test_base_tx_envelope_bincode_roundtrip_arbitrary() {
            #[serde_as]
            #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
            struct Data {
                // Use the bincode-compatible representation defined in this module.
                #[serde_as(as = "BaseTxEnvelope<'_>")]
                envelope: super::super::BaseTxEnvelope,
            }

            let mut bytes = [0u8; 1024];
            rand::rng().fill(bytes.as_mut_slice());
            let data = Data {
                envelope: super::super::BaseTxEnvelope::arbitrary(
                    &mut arbitrary::Unstructured::new(&bytes),
                )
                .unwrap(),
            };

            let encoded = bincode::serde::encode_to_vec(&data, bincode::config::legacy()).unwrap();
            let (decoded, _) =
                bincode::serde::decode_from_slice::<Data, _>(&encoded, bincode::config::legacy())
                    .unwrap();
            assert_eq!(decoded, data);
        }
    }
}

impl InMemorySize for BaseTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Eip8130(tx) => tx.size(),
            Self::Deposit(tx) => tx.size(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::{SignableTransaction, Transaction};
    use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, hex};

    use super::*;

    #[test]
    fn test_tx_gas_limit() {
        let tx = TxDeposit { gas_limit: 1, ..Default::default() };
        let tx_envelope = BaseTxEnvelope::Deposit(tx.seal_slow());
        assert_eq!(tx_envelope.gas_limit(), 1);
    }

    #[test]
    fn test_deposit() {
        let tx = TxDeposit { is_system_transaction: true, ..Default::default() };
        let tx_envelope = BaseTxEnvelope::Deposit(tx.seal_slow());
        assert!(tx_envelope.is_deposit());

        let tx = TxEip1559::default();
        let sig = Signature::test_signature();
        let tx_envelope = BaseTxEnvelope::Eip1559(tx.into_signed(sig));
        assert!(!tx_envelope.is_system_transaction());
    }

    #[test]
    fn test_system_transaction() {
        let mut tx = TxDeposit { is_system_transaction: true, ..Default::default() };
        let tx_envelope = BaseTxEnvelope::Deposit(tx.clone().seal_slow());
        assert!(tx_envelope.is_system_transaction());

        tx.is_system_transaction = false;
        let tx_envelope = BaseTxEnvelope::Deposit(tx.seal_slow());
        assert!(!tx_envelope.is_system_transaction());
    }

    #[test]
    fn test_encode_decode_deposit() {
        let tx = TxDeposit {
            source_hash: B256::left_padding_from(&[0xde, 0xad]),
            from: Address::left_padding_from(&[0xbe, 0xef]),
            mint: 1,
            gas_limit: 2,
            to: TxKind::Call(Address::left_padding_from(&[3])),
            value: U256::from(4_u64),
            input: Bytes::from(vec![5]),
            is_system_transaction: false,
        };
        let tx_envelope = BaseTxEnvelope::Deposit(tx.seal_slow());
        let encoded = tx_envelope.encoded_2718();
        let decoded = BaseTxEnvelope::decode_2718(&mut encoded.as_ref()).unwrap();
        assert_eq!(encoded.len(), tx_envelope.encode_2718_len());
        assert_eq!(decoded, tx_envelope);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serde_roundtrip_deposit() {
        let tx = TxDeposit {
            gas_limit: u64::MAX,
            to: TxKind::Call(Address::random()),
            value: U256::MAX,
            input: Bytes::new(),
            source_hash: U256::MAX.into(),
            from: Address::random(),
            mint: u128::MAX,
            is_system_transaction: false,
        };
        let tx_envelope = BaseTxEnvelope::Deposit(tx.seal_slow());

        let serialized = serde_json::to_string(&tx_envelope).unwrap();
        let deserialized: BaseTxEnvelope = serde_json::from_str(&serialized).unwrap();

        assert_eq!(tx_envelope, deserialized);
    }

    #[test]
    fn eip2718_deposit_decode() {
        // <https://basescan.org/tx/0xc468b38a20375922828c8126912740105125143b9856936085474b2590bbca91>
        let b = hex!(
            "7ef8f8a0417d134467f4737fcdf2475f0ecdd2a0ed6d87ecffc888ba9f60ee7e3b8ac26a94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e20000008dd00101c1200000000000000040000000066c352bb000000000139c4f500000000000000000000000000000000000000000000000000000000c0cff1460000000000000000000000000000000000000000000000000000000000000001d4c88f4065ac9671e8b1329b90773e89b5ddff9cf8675b2b5e9c1b28320609930000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9"
        );

        let tx = BaseTxEnvelope::decode_2718(&mut b[..].as_ref()).unwrap();
        let deposit = tx.as_deposit().unwrap();
        assert_eq!(deposit.mint, 0);
    }

    #[cfg(feature = "alloy-compat")]
    use alloy_network::{AnyRpcTransaction, AnyTxEnvelope, UnknownTxEnvelope};

    #[cfg(feature = "alloy-compat")]
    #[test]
    fn test_alloy_compat_conversion() {
        let deposit = r#"{
  "blockHash": "0x2c475c5d2d609929cec7be9caaaebd29be53e4ef21b1f7b897cb954469e20d01",
  "blockNumber": "0x191350d",
  "depositReceiptVersion": "0x1",
  "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
  "gas": "0xf4240",
  "gasPrice": "0x0",
  "hash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
  "input": "0x440a5e20000008dd00101c1200000000000000030000000067acc63f00000000014d1f2d000000000000000000000000000000000000000000000000000000005ba4c0eb00000000000000000000000000000000000000000000000000000001ce2291bdcbb8f62c15343b39cfacdbf81c4747822ebb16c2518126e47d984422a82defc10000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9",
  "mint": "0x0",
  "nonce": "0x191350e",
  "r": "0x0",
  "s": "0x0",
  "sourceHash": "0x990d7122a1f121f3a6bc45723e28f4921c269037a77e77ffee3c8585136d1a92",
  "to": "0x4200000000000000000000000000000000000015",
  "transactionIndex": "0x0",
  "type": "0x7e",
  "v": "0x0",
  "value": "0x0"
}"#;

        let unknown_tx_envelope: UnknownTxEnvelope = serde_json::from_str(deposit).unwrap();
        let _deposit: crate::TxDeposit = unknown_tx_envelope.try_into().unwrap();

        let any: AnyTxEnvelope = serde_json::from_str(deposit).unwrap();
        let envelope = BaseTxEnvelope::try_from(any).unwrap();
        assert!(envelope.is_deposit());
    }

    #[cfg(feature = "alloy-compat")]
    #[test]
    fn test_alloy_compat_rpc_transaction() {
        let json = r#"{
  "blockHash": "0x2c475c5d2d609929cec7be9caaaebd29be53e4ef21b1f7b897cb954469e20d01",
  "blockNumber": "0x191350d",
  "depositReceiptVersion": "0x1",
  "from": "0xdeaddeaddeaddeaddeaddeaddeaddeaddead0001",
  "gas": "0xf4240",
  "gasPrice": "0x0",
  "hash": "0x096c03d72acb06339c9c7860d1c36b6451932ec0ff16fd34aa9e30a73a245e13",
  "input": "0x440a5e20000008dd00101c1200000000000000030000000067acc63f00000000014d1f2d000000000000000000000000000000000000000000000000000000005ba4c0eb00000000000000000000000000000000000000000000000000000001ce2291bdcbb8f62c15343b39cfacdbf81c4747822ebb16c2518126e47d984422a82defc10000000000000000000000005050f69a9786f081509234f1a7f4684b5e5b76c9",
  "mint": "0x0",
  "nonce": "0x191350e",
  "r": "0x0",
  "s": "0x0",
  "sourceHash": "0x990d7122a1f121f3a6bc45723e28f4921c269037a77e77ffee3c8585136d1a92",
  "to": "0x4200000000000000000000000000000000000015",
  "transactionIndex": "0x0",
  "type": "0x7e",
  "v": "0x0",
  "value": "0x0"
}"#;
        let tx: AnyRpcTransaction = serde_json::from_str(json).unwrap();
        let tx = BaseTxEnvelope::try_from(tx).unwrap();
        assert!(tx.is_deposit());
    }

    #[test]
    fn eip1559_decode() {
        let tx = TxEip1559 {
            chain_id: 1u64,
            nonce: 2,
            max_fee_per_gas: 3,
            max_priority_fee_per_gas: 4,
            gas_limit: 5,
            to: Address::left_padding_from(&[6]).into(),
            value: U256::from(7_u64),
            input: vec![8].into(),
            access_list: Default::default(),
        };
        let sig = Signature::test_signature();
        let tx_signed = tx.into_signed(sig);
        let envelope: BaseTxEnvelope = tx_signed.into();
        let encoded = envelope.encoded_2718();
        let mut slice = encoded.as_slice();
        let decoded = BaseTxEnvelope::decode_2718(&mut slice).unwrap();
        assert!(matches!(decoded, BaseTxEnvelope::Eip1559(_)));
    }

    #[cfg(feature = "k256")]
    #[test]
    fn eip8130_envelope_recovery_honors_checked_vs_unchecked_contract() {
        use alloy_consensus::transaction::SignerRecoverable;
        use alloy_signer::SignerSync;
        use alloy_signer_local::PrivateKeySigner;

        use crate::transaction::eip8130::{Eip8130Signed, TxEip8130};

        // secp256k1 curve order N — used to flip a canonical signature into
        // the upper half via (r, s, v) -> (r, N - s, !v).
        const SECP256K1_N: U256 = U256::from_be_slice(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ]);

        let signer = PrivateKeySigner::random();
        let expected = signer.address();

        let tx = TxEip8130 { sender: None, ..Default::default() };
        let hash = tx.sender_signature_hash();
        let canonical = signer.sign_hash_sync(&hash).unwrap();
        let high_s = Signature::new(canonical.r(), SECP256K1_N - canonical.s(), !canonical.v());

        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(
            tx,
            Bytes::from(high_s.as_bytes().to_vec()),
            Bytes::new(),
        ));

        // Checked path enforces EIP-2 low-s and must reject; the unchecked
        // path is contractually required to accept and recover the address.
        assert!(envelope.recover_signer().is_err());
        assert_eq!(envelope.recover_signer_unchecked().unwrap(), expected);

        let mut buf = alloc::vec::Vec::new();
        assert_eq!(envelope.recover_unchecked_with_buf(&mut buf).unwrap(), expected);
    }

    #[cfg(feature = "k256")]
    #[test]
    fn eip8130_envelope_recovery_short_circuits_configured_owner() {
        use alloy_consensus::transaction::SignerRecoverable;

        use crate::transaction::eip8130::{Eip8130Signed, TxEip8130};

        let explicit = Address::repeat_byte(0xab);
        let tx = TxEip8130 { sender: Some(explicit), ..Default::default() };
        // sender_auth is irrelevant on the configured-owner path; supply 65
        // zero bytes so the structural shape stays well-formed.
        let envelope = BaseTxEnvelope::Eip8130(Eip8130Signed::new(
            tx,
            Bytes::from(vec![0u8; 65]),
            Bytes::new(),
        ));

        assert_eq!(envelope.recover_signer().unwrap(), explicit);
        assert_eq!(envelope.recover_signer_unchecked().unwrap(), explicit);
        let mut buf = alloc::vec::Vec::new();
        assert_eq!(envelope.recover_unchecked_with_buf(&mut buf).unwrap(), explicit);
    }
}
