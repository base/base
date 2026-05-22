use alloy_consensus::{
    InMemorySize, SignableTransaction, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy,
    Typed2718, TypedTransaction, error::ValueError, transaction::RlpEcdsaEncodableTx,
};
use alloy_eips::Encodable2718;
use alloy_primitives::{B256, ChainId, Signature, TxHash, bytes::BufMut};

pub use crate::transaction::envelope::BaseTypedTransaction;
use crate::{BaseTxEnvelope, OpTxType, TxDeposit, TxEip8130, transaction::Eip8130Signed};

impl From<TxLegacy> for BaseTypedTransaction {
    fn from(tx: TxLegacy) -> Self {
        Self::Legacy(tx)
    }
}

impl From<TxEip2930> for BaseTypedTransaction {
    fn from(tx: TxEip2930) -> Self {
        Self::Eip2930(tx)
    }
}

impl From<TxEip1559> for BaseTypedTransaction {
    fn from(tx: TxEip1559) -> Self {
        Self::Eip1559(tx)
    }
}

impl From<TxEip7702> for BaseTypedTransaction {
    fn from(tx: TxEip7702) -> Self {
        Self::Eip7702(tx)
    }
}

impl From<TxDeposit> for BaseTypedTransaction {
    fn from(tx: TxDeposit) -> Self {
        Self::Deposit(tx)
    }
}

impl From<TxEip8130> for BaseTypedTransaction {
    fn from(tx: TxEip8130) -> Self {
        Self::Eip8130(tx)
    }
}

impl From<BaseTxEnvelope> for BaseTypedTransaction {
    fn from(envelope: BaseTxEnvelope) -> Self {
        match envelope {
            BaseTxEnvelope::Legacy(tx) => Self::Legacy(tx.strip_signature()),
            BaseTxEnvelope::Eip2930(tx) => Self::Eip2930(tx.strip_signature()),
            BaseTxEnvelope::Eip1559(tx) => Self::Eip1559(tx.strip_signature()),
            BaseTxEnvelope::Eip7702(tx) => Self::Eip7702(tx.strip_signature()),
            BaseTxEnvelope::Eip8130(tx) => Self::Eip8130(tx.into_tx()),
            BaseTxEnvelope::Deposit(tx) => Self::Deposit(tx.into_inner()),
        }
    }
}

#[cfg(feature = "alloy-compat")]
impl From<BaseTypedTransaction> for alloy_rpc_types_eth::TransactionRequest {
    fn from(tx: BaseTypedTransaction) -> Self {
        match tx {
            BaseTypedTransaction::Legacy(tx) => tx.into(),
            BaseTypedTransaction::Eip2930(tx) => tx.into(),
            BaseTypedTransaction::Eip1559(tx) => tx.into(),
            BaseTypedTransaction::Eip7702(tx) => tx.into(),
            BaseTypedTransaction::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::Eip8130 cannot be converted to an alloy TransactionRequest; AA transactions have no single sender/recipient/value to project into the legacy request shape"
            ),
            BaseTypedTransaction::Deposit(tx) => tx.into(),
        }
    }
}

impl BaseTypedTransaction {
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

    /// Calculates the signing hash for the transaction.
    ///
    /// Returns `None` if the tx is a deposit or EIP-8130 transaction (those
    /// do not use a standard ECDSA single-signature path).
    pub fn checked_signature_hash(&self) -> Option<B256> {
        match self {
            Self::Legacy(tx) => Some(tx.signature_hash()),
            Self::Eip2930(tx) => Some(tx.signature_hash()),
            Self::Eip1559(tx) => Some(tx.signature_hash()),
            Self::Eip7702(tx) => Some(tx.signature_hash()),
            Self::Eip8130(_) | Self::Deposit(_) => None,
        }
    }

    /// Return the inner legacy transaction if it exists.
    pub const fn legacy(&self) -> Option<&TxLegacy> {
        match self {
            Self::Legacy(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner EIP-2930 transaction if it exists.
    pub const fn eip2930(&self) -> Option<&TxEip2930> {
        match self {
            Self::Eip2930(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner EIP-1559 transaction if it exists.
    pub const fn eip1559(&self) -> Option<&TxEip1559> {
        match self {
            Self::Eip1559(tx) => Some(tx),
            _ => None,
        }
    }

    /// Return the inner deposit transaction if it exists.
    pub const fn deposit(&self) -> Option<&TxDeposit> {
        match self {
            Self::Deposit(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns `true` if transaction is deposit transaction.
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Return the inner EIP-8130 transaction if it exists.
    pub const fn eip8130(&self) -> Option<&TxEip8130> {
        match self {
            Self::Eip8130(tx) => Some(tx),
            _ => None,
        }
    }

    /// Returns `true` if transaction is an EIP-8130 transaction.
    pub const fn is_eip8130(&self) -> bool {
        matches!(self, Self::Eip8130(_))
    }

    /// Calculate the transaction hash for the given signature.
    ///
    /// For a deposit variant the regular tx hash is returned (signature ignored).
    /// Panics on an EIP-8130 variant: that variant has no ECDSA signature and
    /// callers must hash through the [`BaseTxEnvelope`] path instead.
    pub fn tx_hash(&self, signature: &Signature) -> TxHash {
        match self {
            Self::Legacy(tx) => tx.tx_hash(signature),
            Self::Eip2930(tx) => tx.tx_hash(signature),
            Self::Eip1559(tx) => tx.tx_hash(signature),
            Self::Eip7702(tx) => tx.tx_hash(signature),
            Self::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::tx_hash invoked on an EIP-8130 variant; use Eip8130Signed::hash via the envelope path"
            ),
            Self::Deposit(tx) => tx.tx_hash(),
        }
    }

    /// Convenience function to convert this typed transaction into an [`BaseTxEnvelope`].
    ///
    /// Note: If this is a [`BaseTypedTransaction::Deposit`] variant, the signature will be ignored.
    pub fn into_envelope(self, signature: Signature) -> BaseTxEnvelope {
        self.into_signed(signature).into()
    }

    /// Attempts to convert the L2 variant into an ethereum [`TypedTransaction`].
    ///
    /// Returns the typed transaction as error if it is a variant unsupported on ethereum:
    /// [`TxDeposit`]
    pub fn try_into_eth(self) -> Result<TypedTransaction, ValueError<Self>> {
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
}

impl RlpEcdsaEncodableTx for BaseTypedTransaction {
    fn rlp_encoded_fields_length(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip2930(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip1559(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip7702(tx) => tx.rlp_encoded_fields_length(),
            Self::Eip8130(tx) => tx.rlp_encoded_fields_length(),
            Self::Deposit(tx) => tx.rlp_encoded_fields_length(),
        }
    }

    fn rlp_encode_fields(&self, out: &mut dyn alloy_rlp::BufMut) {
        match self {
            Self::Legacy(tx) => tx.rlp_encode_fields(out),
            Self::Eip2930(tx) => tx.rlp_encode_fields(out),
            Self::Eip1559(tx) => tx.rlp_encode_fields(out),
            Self::Eip7702(tx) => tx.rlp_encode_fields(out),
            Self::Eip8130(tx) => tx.rlp_encode_fields(out),
            Self::Deposit(tx) => tx.rlp_encode_fields(out),
        }
    }

    fn eip2718_encode_with_type(&self, signature: &Signature, _ty: u8, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip2930(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip1559(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip7702(tx) => tx.eip2718_encode_with_type(signature, tx.ty(), out),
            Self::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::eip2718_encode_with_type invoked on EIP-8130 variant; use Eip8130Signed::encode_2718"
            ),
            Self::Deposit(tx) => tx.encode_2718(out),
        }
    }

    fn eip2718_encode(&self, signature: &Signature, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(tx) => tx.eip2718_encode(signature, out),
            Self::Eip2930(tx) => tx.eip2718_encode(signature, out),
            Self::Eip1559(tx) => tx.eip2718_encode(signature, out),
            Self::Eip7702(tx) => tx.eip2718_encode(signature, out),
            Self::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::eip2718_encode invoked on EIP-8130 variant; use Eip8130Signed::encode_2718"
            ),
            Self::Deposit(tx) => tx.encode_2718(out),
        }
    }

    fn network_encode_with_type(&self, signature: &Signature, _ty: u8, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip2930(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip1559(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip7702(tx) => tx.network_encode_with_type(signature, tx.ty(), out),
            Self::Eip8130(_) => unimplemented!(
                "BaseTypedTransaction::network_encode_with_type invoked on EIP-8130 variant"
            ),
            Self::Deposit(tx) => tx.network_encode(out),
        }
    }

    fn network_encode(&self, signature: &Signature, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(tx) => tx.network_encode(signature, out),
            Self::Eip2930(tx) => tx.network_encode(signature, out),
            Self::Eip1559(tx) => tx.network_encode(signature, out),
            Self::Eip7702(tx) => tx.network_encode(signature, out),
            Self::Eip8130(_) => {
                unimplemented!("BaseTypedTransaction::network_encode invoked on EIP-8130 variant")
            }
            Self::Deposit(tx) => tx.network_encode(out),
        }
    }

    fn tx_hash_with_type(&self, signature: &Signature, _ty: u8) -> TxHash {
        match self {
            Self::Legacy(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip2930(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip1559(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip7702(tx) => tx.tx_hash_with_type(signature, tx.ty()),
            Self::Eip8130(_) => {
                unimplemented!(
                    "BaseTypedTransaction::tx_hash_with_type invoked on EIP-8130 variant"
                )
            }
            Self::Deposit(tx) => tx.tx_hash(),
        }
    }

    fn tx_hash(&self, signature: &Signature) -> TxHash {
        match self {
            Self::Legacy(tx) => tx.tx_hash(signature),
            Self::Eip2930(tx) => tx.tx_hash(signature),
            Self::Eip1559(tx) => tx.tx_hash(signature),
            Self::Eip7702(tx) => tx.tx_hash(signature),
            Self::Eip8130(_) => {
                unimplemented!("BaseTypedTransaction::tx_hash invoked on EIP-8130 variant")
            }
            Self::Deposit(tx) => tx.tx_hash(),
        }
    }
}

impl SignableTransaction<Signature> for BaseTypedTransaction {
    fn set_chain_id(&mut self, chain_id: ChainId) {
        match self {
            Self::Legacy(tx) => tx.set_chain_id(chain_id),
            Self::Eip2930(tx) => tx.set_chain_id(chain_id),
            Self::Eip1559(tx) => tx.set_chain_id(chain_id),
            Self::Eip7702(tx) => tx.set_chain_id(chain_id),
            Self::Eip8130(tx) => tx.set_chain_id(chain_id),
            Self::Deposit(_) => {}
        }
    }

    fn encode_for_signing(&self, out: &mut dyn BufMut) {
        match self {
            Self::Legacy(tx) => tx.encode_for_signing(out),
            Self::Eip2930(tx) => tx.encode_for_signing(out),
            Self::Eip1559(tx) => tx.encode_for_signing(out),
            Self::Eip7702(tx) => tx.encode_for_signing(out),
            Self::Eip8130(tx) => tx.encode_for_signing(out),
            Self::Deposit(_) => {}
        }
    }

    fn payload_len_for_signature(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.payload_len_for_signature(),
            Self::Eip2930(tx) => tx.payload_len_for_signature(),
            Self::Eip1559(tx) => tx.payload_len_for_signature(),
            Self::Eip7702(tx) => tx.payload_len_for_signature(),
            Self::Eip8130(tx) => tx.payload_len_for_signature(),
            Self::Deposit(_) => 0,
        }
    }

    fn into_signed(self, signature: Signature) -> Signed<Self, Signature>
    where
        Self: Sized,
    {
        let hash = self.tx_hash(&signature);
        Signed::new_unchecked(self, signature, hash)
    }
}

impl InMemorySize for BaseTypedTransaction {
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

impl From<Eip8130Signed> for BaseTxEnvelope {
    fn from(signed: Eip8130Signed) -> Self {
        Self::Eip8130(signed)
    }
}
