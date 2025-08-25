use crate::{
    tests::{ONE_ETH, Protocol, framework::driver::ChainDriver},
    tx_signer::Signer,
};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, BlockHash, TxHash, TxKind, U256, hex};
use alloy_rpc_types_eth::{Block, BlockTransactionHashes};
use core::future::Future;
use op_alloy_consensus::{OpTypedTransaction, TxDeposit};
use op_alloy_rpc_types::Transaction;
use reth_db::{
    ClientVersion, DatabaseEnv, init_db,
    mdbx::{DatabaseArguments, KILOBYTE, MEGABYTE, MaxReadTransactionDuration},
    test_utils::{ERROR_DB_CREATION, TempDatabase},
};
use reth_node_core::{args::DatadirArgs, dirs::DataDirPath, node_config::NodeConfig};
use reth_optimism_chainspec::OpChainSpec;
use std::sync::Arc;

use super::{FUNDED_PRIVATE_KEYS, TransactionBuilder};

pub trait TransactionBuilderExt {
    fn random_valid_transfer(self) -> Self;
    fn random_reverting_transaction(self) -> Self;
    fn random_big_transaction(self) -> Self;
}

impl TransactionBuilderExt for TransactionBuilder {
    fn random_valid_transfer(self) -> Self {
        self.with_to(rand::random::<Address>()).with_value(1)
    }

    fn random_reverting_transaction(self) -> Self {
        self.with_create().with_input(hex!("60006000fd").into()) // PUSH1 0x00 PUSH1 0x00 REVERT
    }

    fn random_big_transaction(self) -> Self {
        // PUSH13 0x63ffffffff60005260046000f3 PUSH1 0x00 MSTORE PUSH1 0x02 PUSH1 0x0d PUSH1 0x13 PUSH1 0x00 CREATE2
        self.with_create()
            .with_input(hex!("6c63ffffffff60005260046000f36000526002600d60136000f5").into())
    }
}

pub trait ChainDriverExt {
    fn fund_default_accounts(&self) -> impl Future<Output = eyre::Result<()>>;
    fn fund_many(
        &self,
        addresses: Vec<Address>,
        amount: u128,
    ) -> impl Future<Output = eyre::Result<BlockHash>>;
    fn fund(&self, address: Address, amount: u128)
    -> impl Future<Output = eyre::Result<BlockHash>>;
    fn first_funded_address(&self) -> Address {
        FUNDED_PRIVATE_KEYS[0]
            .parse()
            .expect("Invalid funded private key")
    }

    fn fund_accounts(
        &self,
        count: usize,
        amount: u128,
    ) -> impl Future<Output = eyre::Result<Vec<Signer>>> {
        async move {
            let accounts = (0..count).map(|_| Signer::random()).collect::<Vec<_>>();
            self.fund_many(accounts.iter().map(|a| a.address).collect(), amount)
                .await?;
            Ok(accounts)
        }
    }

    fn build_new_block_with_valid_transaction(
        &self,
    ) -> impl Future<Output = eyre::Result<(TxHash, Block<Transaction>)>>;

    fn build_new_block_with_reverrting_transaction(
        &self,
    ) -> impl Future<Output = eyre::Result<(TxHash, Block<Transaction>)>>;
}

impl<P: Protocol> ChainDriverExt for ChainDriver<P> {
    async fn fund_default_accounts(&self) -> eyre::Result<()> {
        for key in FUNDED_PRIVATE_KEYS {
            let signer: Signer = key.parse()?;
            self.fund(signer.address, ONE_ETH).await?;
        }
        Ok(())
    }

    async fn fund_many(&self, addresses: Vec<Address>, amount: u128) -> eyre::Result<BlockHash> {
        let mut txs = Vec::with_capacity(addresses.len());

        for address in addresses {
            let deposit = TxDeposit {
                source_hash: B256::default(),
                from: address, // Set the sender to the address of the account to seed
                to: TxKind::Create,
                mint: amount, // Amount to deposit
                value: U256::default(),
                gas_limit: 210000,
                is_system_transaction: false,
                input: Default::default(), // No input data for the deposit
            };

            let signer = Signer::random();
            let signed_tx = signer.sign_tx(OpTypedTransaction::Deposit(deposit))?;
            let signed_tx_rlp = signed_tx.encoded_2718();
            txs.push(signed_tx_rlp.into());
        }

        Ok(self.build_new_block_with_txs(txs).await?.header.hash)
    }

    async fn fund(&self, address: Address, amount: u128) -> eyre::Result<BlockHash> {
        let deposit = TxDeposit {
            source_hash: B256::default(),
            from: address, // Set the sender to the address of the account to seed
            to: TxKind::Create,
            mint: amount, // Amount to deposit
            value: U256::default(),
            gas_limit: 210000,
            is_system_transaction: false,
            input: Default::default(), // No input data for the deposit
        };

        let signer = Signer::random();
        let signed_tx = signer.sign_tx(OpTypedTransaction::Deposit(deposit))?;
        let signed_tx_rlp = signed_tx.encoded_2718();
        Ok(self
            .build_new_block_with_txs(vec![signed_tx_rlp.into()])
            .await?
            .header
            .hash)
    }

    async fn build_new_block_with_valid_transaction(
        &self,
    ) -> eyre::Result<(TxHash, Block<Transaction>)> {
        let tx = self
            .create_transaction()
            .random_valid_transfer()
            .send()
            .await?;
        Ok((*tx.tx_hash(), self.build_new_block().await?))
    }

    async fn build_new_block_with_reverrting_transaction(
        &self,
    ) -> eyre::Result<(TxHash, Block<Transaction>)> {
        let tx = self
            .create_transaction()
            .random_reverting_transaction()
            .send()
            .await?;

        Ok((*tx.tx_hash(), self.build_new_block().await?))
    }
}

pub trait BlockTransactionsExt {
    fn includes(&self, txs: &impl AsTxs) -> bool;
}

impl BlockTransactionsExt for Block<Transaction> {
    fn includes(&self, txs: &impl AsTxs) -> bool {
        txs.as_txs()
            .into_iter()
            .all(|tx| self.transactions.hashes().any(|included| included == tx))
    }
}

impl BlockTransactionsExt for BlockTransactionHashes<'_, Transaction> {
    fn includes(&self, txs: &impl AsTxs) -> bool {
        let mut included_tx_iter = self.clone();
        txs.as_txs()
            .iter()
            .all(|tx| included_tx_iter.any(|included| included == *tx))
    }
}

pub trait OpRbuilderArgsTestExt {
    fn test_default() -> Self;
}

impl OpRbuilderArgsTestExt for crate::args::OpRbuilderArgs {
    fn test_default() -> Self {
        let mut default = Self::default();
        default.flashblocks.flashblocks_port = 0; // randomize port
        default
    }
}

pub trait AsTxs {
    fn as_txs(&self) -> Vec<TxHash>;
}

impl AsTxs for TxHash {
    fn as_txs(&self) -> Vec<TxHash> {
        vec![*self]
    }
}

impl AsTxs for Vec<TxHash> {
    fn as_txs(&self) -> Vec<TxHash> {
        self.clone()
    }
}

pub fn create_test_db(config: NodeConfig<OpChainSpec>) -> Arc<TempDatabase<DatabaseEnv>> {
    let path = reth_node_core::dirs::MaybePlatformPath::<DataDirPath>::from(
        reth_db::test_utils::tempdir_path(),
    );
    let db_config = config.with_datadir_args(DatadirArgs {
        datadir: path.clone(),
        ..Default::default()
    });
    let data_dir = path.unwrap_or_chain_default(db_config.chain.chain(), db_config.datadir.clone());
    let path = data_dir.db();
    let db = init_db(
        path.as_path(),
        DatabaseArguments::new(ClientVersion::default())
            .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
            .with_geometry_max_size(Some(4 * MEGABYTE))
            .with_growth_step(Some(4 * KILOBYTE)),
    )
    .expect(ERROR_DB_CREATION);
    Arc::new(TempDatabase::new(db, path))
}
