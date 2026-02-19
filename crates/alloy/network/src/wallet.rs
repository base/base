use alloy_consensus::{TxEnvelope, TypedTransaction};
use alloy_network::{Ethereum, EthereumWallet, NetworkWallet};
use alloy_primitives::Address;
use base_alloy_consensus::{OpTxEnvelope, OpTypedTransaction};

use crate::Base;

impl NetworkWallet<Base> for EthereumWallet {
    fn default_signer_address(&self) -> Address {
        NetworkWallet::<Ethereum>::default_signer_address(self)
    }

    fn has_signer_for(&self, address: &Address) -> bool {
        NetworkWallet::<Ethereum>::has_signer_for(self, address)
    }

    fn signer_addresses(&self) -> impl Iterator<Item = Address> {
        NetworkWallet::<Ethereum>::signer_addresses(self)
    }

    async fn sign_transaction_from(
        &self,
        sender: Address,
        tx: OpTypedTransaction,
    ) -> alloy_signer::Result<OpTxEnvelope> {
        let tx = match tx {
            OpTypedTransaction::Legacy(tx) => TypedTransaction::Legacy(tx),
            OpTypedTransaction::Eip2930(tx) => TypedTransaction::Eip2930(tx),
            OpTypedTransaction::Eip1559(tx) => TypedTransaction::Eip1559(tx),
            OpTypedTransaction::Eip7702(tx) => TypedTransaction::Eip7702(tx),
            OpTypedTransaction::Deposit(_) => {
                return Err(alloy_signer::Error::other("not implemented for deposit tx"));
            }
        };
        let tx = NetworkWallet::<Ethereum>::sign_transaction_from(self, sender, tx).await?;

        Ok(match tx {
            TxEnvelope::Eip1559(tx) => OpTxEnvelope::Eip1559(tx),
            TxEnvelope::Eip2930(tx) => OpTxEnvelope::Eip2930(tx),
            TxEnvelope::Eip7702(tx) => OpTxEnvelope::Eip7702(tx),
            TxEnvelope::Legacy(tx) => OpTxEnvelope::Legacy(tx),
            _ => unreachable!(),
        })
    }
}
