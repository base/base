use alloc::{vec, vec::Vec};
use core::marker::PhantomData;

use alloy_consensus::Header;
use reth_chainspec::{ChainSpecProvider, EthereumHardforks};
use reth_db_api::{cursor::DbCursorRO, tables, transaction::DbTx};
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::{Block, BlockBody, FullBlockHeader, SignedTransaction};
use reth_storage_errors::provider::ProviderResult;

use crate::DBProvider;

/// Input for reading a block body. Contains a header of block being read and a list of pre-fetched
/// transactions.
pub type ReadBodyInput<'a, B> =
    (&'a <B as Block>::Header, Vec<<<B as Block>::Body as BlockBody>::Transaction>);

/// Trait that implements how block bodies are read from the storage.
///
/// Note: Within the current abstraction, transactions persistence is handled separately, thus this
/// trait is provided with transactions read beforehand and is expected to construct the block body
/// from those transactions and additional data read from elsewhere.
#[auto_impl::auto_impl(&, Arc)]
pub trait BlockBodyReader<Provider> {
    /// The block type.
    type Block: Block;

    /// Receives a list of block headers along with block transactions and returns the block bodies.
    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>>;
}

/// Ethereum storage implementation.
#[derive(Debug, Clone, Copy)]
pub struct EthStorage<T = TransactionSigned, H = Header>(PhantomData<(T, H)>);

impl<T, H> Default for EthStorage<T, H> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<Provider, T, H> BlockBodyReader<Provider> for EthStorage<T, H>
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    type Block = alloy_consensus::Block<T, H>;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>> {
        // TODO: Ideally storage should hold its own copy of chain spec
        let chain_spec = provider.chain_spec();

        let mut withdrawals_cursor = provider.tx_ref().cursor_read::<tables::BlockWithdrawals>()?;

        let mut bodies = Vec::with_capacity(inputs.len());

        for (header, transactions) in inputs {
            // If we are past shanghai, then all blocks should have a withdrawal list,
            // even if empty
            let withdrawals = if chain_spec.is_shanghai_active_at_timestamp(header.timestamp()) {
                withdrawals_cursor
                    .seek_exact(header.number())?
                    .map(|(_, w)| w.withdrawals)
                    .unwrap_or_default()
                    .into()
            } else {
                None
            };
            let ommers = if chain_spec.is_paris_active_at_block(header.number()) {
                Vec::new()
            } else {
                // Pre-merge: fetch ommers from database using direct database access
                provider
                    .tx_ref()
                    .cursor_read::<tables::BlockOmmers<H>>()?
                    .seek_exact(header.number())?
                    .map(|(_, stored_ommers)| stored_ommers.ommers)
                    .unwrap_or_default()
            };
            bodies.push(alloy_consensus::BlockBody { transactions, ommers, withdrawals });
        }

        Ok(bodies)
    }
}

/// A noop storage for chains that don’t have custom body storage.
///
/// This will never read nor write additional body content such as withdrawals or ommers.
/// But will respect the optionality of withdrawals if activated and fill them if the corresponding
/// hardfork is activated.
#[derive(Debug, Clone, Copy)]
pub struct EmptyBodyStorage<T, H>(PhantomData<(T, H)>);

impl<T, H> Default for EmptyBodyStorage<T, H> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<Provider, T, H> BlockBodyReader<Provider> for EmptyBodyStorage<T, H>
where
    Provider: ChainSpecProvider<ChainSpec: EthereumHardforks>,
    T: SignedTransaction,
    H: FullBlockHeader,
{
    type Block = alloy_consensus::Block<T, H>;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<<Self::Block as Block>::Body>> {
        let chain_spec = provider.chain_spec();

        Ok(inputs
            .into_iter()
            .map(|(header, transactions)| {
                alloy_consensus::BlockBody {
                    transactions,
                    ommers: vec![], // Empty storage never has ommers
                    withdrawals: chain_spec
                        .is_shanghai_active_at_timestamp(header.timestamp())
                        .then(Default::default),
                }
            })
            .collect())
    }
}
