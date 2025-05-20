use crate::tx_signer::Signer;
use alloy_consensus::TxEip1559;
use alloy_eips::{eip2718::Encodable2718, BlockNumberOrTag};
use alloy_primitives::Bytes;
use alloy_provider::{PendingTransactionBuilder, Provider, RootProvider};
use core::cmp::max;
use op_alloy_consensus::{OpTxEnvelope, OpTypedTransaction};
use op_alloy_network::Optimism;
use reth_primitives::Recovered;

use alloy_eips::eip1559::MIN_PROTOCOL_BASE_FEE;

use super::BUILDER_PRIVATE_KEY;

#[derive(Clone)]
pub struct TransactionBuilder {
    provider: RootProvider<Optimism>,
    signer: Option<Signer>,
    nonce: Option<u64>,
    base_fee: Option<u128>,
    tx: TxEip1559,
}

impl TransactionBuilder {
    pub fn new(provider: RootProvider<Optimism>) -> Self {
        Self {
            provider,
            signer: None,
            nonce: None,
            base_fee: None,
            tx: TxEip1559 {
                chain_id: 901,
                gas_limit: 210000,
                ..Default::default()
            },
        }
    }

    pub fn with_signer(mut self, signer: Signer) -> Self {
        self.signer = Some(signer);
        self
    }

    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.tx.chain_id = chain_id;
        self
    }

    pub fn with_nonce(mut self, nonce: u64) -> Self {
        self.tx.nonce = nonce;
        self
    }

    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.tx.gas_limit = gas_limit;
        self
    }

    pub fn with_max_fee_per_gas(mut self, max_fee_per_gas: u128) -> Self {
        self.tx.max_fee_per_gas = max_fee_per_gas;
        self
    }

    pub fn with_max_priority_fee_per_gas(mut self, max_priority_fee_per_gas: u128) -> Self {
        self.tx.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    pub fn with_input(mut self, input: Bytes) -> Self {
        self.tx.input = input;
        self
    }

    pub async fn build(mut self) -> Recovered<OpTxEnvelope> {
        let signer = match self.signer {
            Some(signer) => signer,
            None => Signer::try_from_secret(
                BUILDER_PRIVATE_KEY
                    .parse()
                    .expect("invalid hardcoded builder private key"),
            )
            .expect("Failed to create signer from hardcoded private key"),
        };

        let nonce = match self.nonce {
            Some(nonce) => nonce,
            None => self
                .provider
                .get_transaction_count(signer.address)
                .pending()
                .await
                .expect("Failed to get transaction count"),
        };

        let base_fee = match self.base_fee {
            Some(base_fee) => base_fee,
            None => {
                let previous_base_fee = self
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .await
                    .expect("failed to get latest block")
                    .expect("latest block should exist")
                    .header
                    .base_fee_per_gas
                    .expect("base fee should be present in latest block");

                max(previous_base_fee as u128, MIN_PROTOCOL_BASE_FEE as u128)
            }
        };

        self.tx.nonce = nonce;
        self.tx.max_fee_per_gas = base_fee + self.tx.max_priority_fee_per_gas;

        signer
            .sign_tx(OpTypedTransaction::Eip1559(self.tx))
            .expect("Failed to sign transaction")
    }

    pub async fn send(self) -> eyre::Result<PendingTransactionBuilder<Optimism>> {
        let provider = self.provider.clone();
        let transaction = self.build().await;
        Ok(provider
            .send_raw_transaction(transaction.encoded_2718().as_slice())
            .await?)
    }
}
