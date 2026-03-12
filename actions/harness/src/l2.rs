use std::{collections::VecDeque, sync::Arc};

use alloy_consensus::{Header, SignableTransaction};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_trie::{EMPTY_ROOT_HASH, TrieAccount, root::state_root_unhashed};
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_execution_chainspec::OpChainSpecBuilder;
use base_execution_evm::OpEvmConfig;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo};
use base_revm::OpTransaction;
use reth_evm::{ConfigureEvm, Evm as _, FromRecoveredTx};
use revm::{
    DatabaseCommit,
    context::{TxEnv, result::ResultAndState},
    database::InMemoryDB,
    state::AccountInfo,
};

use crate::{L2BlockProvider, SharedL1Chain};

/// Hardcoded private key for the test account used across all action tests.
///
/// The corresponding address is deterministic: derive it via
/// `PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY).unwrap().address()`.
/// Tests that need to fund the account should include it in the genesis
/// allocation with a sufficient ETH balance.
pub const TEST_ACCOUNT_KEY: B256 = B256::new([0x01u8; 32]);

/// The L2 address derived from [`TEST_ACCOUNT_KEY`].
///
/// Pre-computed so callers can reference it without constructing a signer.
// Address derived from the secp256k1 public key of [0x01; 32].
pub const TEST_ACCOUNT_ADDRESS: Address =
    alloy_primitives::address!("1a642f0E3c3aF545E7AcBD38b07251B3990914F1");

/// Initial balance for the test account (1 ETH).
const TEST_ACCOUNT_BALANCE: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

/// Error type returned by [`L2Sequencer`].
#[derive(Debug, thiserror::Error)]
pub enum L2SequencerError {
    /// The L1 block required for the current epoch is missing from the chain.
    #[error("L1 block {0} not found in shared chain")]
    MissingL1Block(u64),
    /// Failed to build the L1 info deposit transaction.
    #[error("failed to build L1 info deposit: {0}")]
    L1Info(#[from] base_protocol::BlockInfoError),
    /// Transaction signing failed.
    #[error("signing failed: {0}")]
    Signing(#[from] alloy_signer::Error),
    /// EVM execution failed.
    #[error("EVM execution failed: {0}")]
    Evm(String),
}

/// A pre-built queue of [`OpBlock`]s for the batcher to drain.
///
/// Tests push fully-formed blocks into the source, which the batcher
/// consumes one at a time via [`L2BlockProvider::next_block`].
#[derive(Debug, Default)]
pub struct ActionL2Source {
    blocks: VecDeque<OpBlock>,
}

impl ActionL2Source {
    /// Create an empty source.
    pub const fn new() -> Self {
        Self { blocks: VecDeque::new() }
    }

    /// Push a block to the back of the queue.
    pub fn push(&mut self, block: OpBlock) {
        self.blocks.push_back(block);
    }

    /// Return the number of blocks remaining.
    pub fn remaining(&self) -> usize {
        self.blocks.len()
    }

    /// Return `true` if the source has been fully drained.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl L2BlockProvider for ActionL2Source {
    fn next_block(&mut self) -> Option<OpBlock> {
        self.blocks.pop_front()
    }
}

/// Builds real [`OpBlock`]s for use in action tests.
///
/// Each block contains:
/// - A correct L1-info deposit transaction (type `0x7E`) as the first
///   transaction, built from the actual L1 block at the current epoch.
/// - A configurable number of signed EIP-1559 user transactions from the
///   test account ([`TEST_ACCOUNT_KEY`]).
///
/// Epoch selection mirrors the real sequencer: the epoch advances to the next
/// L1 block once that block's timestamp is ≤ the new L2 block's timestamp,
/// unless an L1 origin is pinned via [`pin_l1_origin`].
///
/// # EVM Execution & State Root
///
/// Transactions are executed against an in-memory EVM database seeded with the
/// test account balance (1 ETH). The state root in the header is computed from
/// the post-execution state using a Merkle Patricia Trie. The header is sealed
/// to produce a real block hash, which is used for correct parent-hash chaining.
///
/// [`pin_l1_origin`]: L2Sequencer::pin_l1_origin
#[derive(Debug)]
pub struct L2Sequencer {
    /// Current unsafe L2 head.
    head: L2BlockInfo,
    /// Shared view of the L1 chain for epoch selection.
    l1_chain: SharedL1Chain,
    /// Rollup configuration.
    rollup_config: RollupConfig,
    /// L1 chain config (needed for [`L1BlockInfoTx`]).
    l1_chain_config: L1ChainConfig,
    /// Current system config (updated on epoch changes or key rotation).
    system_config: SystemConfig,
    /// Signer for user transactions.
    signer: PrivateKeySigner,
    /// Number of user EIP-1559 transactions to include per block.
    user_txs_per_block: usize,
    /// Per-account nonce for the test signer.
    nonce: u64,
    /// In-memory EVM database carrying state across blocks.
    db: InMemoryDB,
    /// Optional pinned L1 origin. When set, epoch selection is bypassed and
    /// this block is used as the epoch for every subsequent L2 block built.
    l1_origin_pin: Option<BlockInfo>,
}

impl L2Sequencer {
    /// Create a new sequencer starting from the given L2 genesis head.
    pub fn new(
        head: L2BlockInfo,
        l1_chain: SharedL1Chain,
        rollup_config: RollupConfig,
        system_config: SystemConfig,
    ) -> Self {
        let signer = PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY)
            .expect("TEST_ACCOUNT_KEY is a valid secp256k1 scalar");

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            TEST_ACCOUNT_ADDRESS,
            AccountInfo { balance: TEST_ACCOUNT_BALANCE, ..Default::default() },
        );

        Self {
            head,
            l1_chain,
            rollup_config,
            l1_chain_config: L1ChainConfig::default(),
            system_config,
            signer,
            user_txs_per_block: 1,
            nonce: 0,
            db,
            l1_origin_pin: None,
        }
    }

    /// Set the number of signed user transactions included per block.
    pub const fn with_user_txs_per_block(mut self, n: usize) -> Self {
        self.user_txs_per_block = n;
        self
    }

    /// Return the current unsafe L2 head.
    pub const fn head(&self) -> L2BlockInfo {
        self.head
    }

    /// Pin the L1 origin to the given block, bypassing automatic epoch advance.
    ///
    /// While pinned, every call to [`build_next_block`] uses `origin` as the
    /// epoch regardless of timestamps. The sequencer number increments within
    /// the same epoch until the pin is cleared.
    ///
    /// [`build_next_block`]: L2Sequencer::build_next_block
    pub const fn pin_l1_origin(&mut self, origin: BlockInfo) {
        self.l1_origin_pin = Some(origin);
    }

    /// Clear the pinned L1 origin, restoring automatic epoch selection.
    pub const fn clear_l1_origin_pin(&mut self) {
        self.l1_origin_pin = None;
    }

    /// Build the next L2 block containing no user transactions.
    ///
    /// Temporarily sets `user_txs_per_block = 0`, calls [`build_next_block`],
    /// then restores the original count. Useful for simulating forced-empty
    /// blocks at the sequencer drift boundary.
    ///
    /// [`build_next_block`]: L2Sequencer::build_next_block
    pub fn build_empty_block(&mut self) -> Result<OpBlock, L2SequencerError> {
        let saved = self.user_txs_per_block;
        self.user_txs_per_block = 0;
        let result = self.build_next_block();
        self.user_txs_per_block = saved;
        result
    }

    /// Build the next L2 block and advance the internal head.
    ///
    /// Returns a fully-formed [`OpBlock`] containing the L1-info deposit and
    /// any configured user transactions, with a real state root and block hash.
    pub fn build_next_block(&mut self) -> Result<OpBlock, L2SequencerError> {
        let next_number = self.head.block_info.number + 1;
        let next_timestamp = self.head.block_info.timestamp + self.rollup_config.block_time;
        let parent_hash = self.head.block_info.hash;
        let current_epoch = self.head.l1_origin.number;

        // Epoch selection: use pinned origin if set, otherwise auto-advance.
        let (epoch_number, l1_header) = if let Some(pin) = self.l1_origin_pin {
            let block = self
                .l1_chain
                .get_block(pin.number)
                .ok_or(L2SequencerError::MissingL1Block(pin.number))?;
            (block.number(), block.header)
        } else if let Some(next_l1) = self.l1_chain.get_block(current_epoch + 1) {
            if next_l1.timestamp() <= next_timestamp {
                (next_l1.number(), next_l1.header)
            } else {
                let cur = self
                    .l1_chain
                    .get_block(current_epoch)
                    .ok_or(L2SequencerError::MissingL1Block(current_epoch))?;
                (cur.number(), cur.header)
            }
        } else {
            let cur = self
                .l1_chain
                .get_block(current_epoch)
                .ok_or(L2SequencerError::MissingL1Block(current_epoch))?;
            (cur.number(), cur.header)
        };

        let seq_num =
            if epoch_number == self.head.l1_origin.number { self.head.seq_num + 1 } else { 0 };

        // Build the L1 info deposit (first transaction in every L2 block).
        let (_l1_info, deposit_tx) = L1BlockInfoTx::try_new_with_deposit_tx(
            &self.rollup_config,
            &self.l1_chain_config,
            &self.system_config,
            seq_num,
            &l1_header,
            next_timestamp,
        )?;

        let mut transactions: Vec<OpTxEnvelope> = vec![OpTxEnvelope::Deposit(deposit_tx)];

        // Build signed EIP-1559 user transactions.
        for _ in 0..self.user_txs_per_block {
            let tx = alloy_consensus::TxEip1559 {
                chain_id: self.rollup_config.l2_chain_id.id(),
                nonce: self.nonce,
                max_fee_per_gas: 1_000_000_000,
                max_priority_fee_per_gas: 1_000_000,
                gas_limit: 21_000,
                to: TxKind::Call(Address::ZERO),
                value: U256::from(1u64),
                input: Bytes::new(),
                access_list: Default::default(),
            };
            let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
            self.nonce += 1;
            transactions.push(OpTxEnvelope::Eip1559(tx.into_signed(sig)));
        }

        // Execute transactions against the in-memory EVM.
        let (state_root, gas_used) =
            self.execute_transactions(&transactions, next_number, next_timestamp, parent_hash)?;

        let epoch_hash = l1_header.hash_slow();
        let header = Header {
            number: next_number,
            timestamp: next_timestamp,
            parent_hash,
            gas_limit: 30_000_000,
            gas_used,
            state_root,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let block_hash = header.hash_slow();

        let block = OpBlock {
            header,
            body: alloy_consensus::BlockBody { transactions, ommers: vec![], withdrawals: None },
        };

        self.head = L2BlockInfo {
            block_info: BlockInfo {
                number: next_number,
                timestamp: next_timestamp,
                parent_hash,
                hash: block_hash,
            },
            l1_origin: BlockNumHash { number: epoch_number, hash: epoch_hash },
            seq_num,
        };

        Ok(block)
    }

    /// Execute all transactions in the block against the in-memory EVM and
    /// return the resulting (`state_root`, `cumulative_gas_used`).
    fn execute_transactions(
        &mut self,
        transactions: &[OpTxEnvelope],
        block_number: u64,
        timestamp: u64,
        parent_hash: B256,
    ) -> Result<(B256, u64), L2SequencerError> {
        let chain_spec = Arc::new(
            OpChainSpecBuilder::base_mainnet().chain(self.rollup_config.l2_chain_id).build(),
        );
        let evm_config = OpEvmConfig::optimism(chain_spec);

        let header = Header {
            number: block_number,
            timestamp,
            parent_hash,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut cumulative_gas_used = 0u64;

        for tx in transactions {
            let sender = tx_sender(tx, &self.signer);
            let op_tx: OpTransaction<TxEnv> = OpTransaction::from_recovered_tx(tx, sender);

            let evm_env =
                evm_config.evm_env(&header).map_err(|e| L2SequencerError::Evm(e.to_string()))?;
            let mut evm = evm_config.evm_with_env(&mut self.db, evm_env);
            match evm.transact(op_tx) {
                Ok(ResultAndState { state, result }) => {
                    cumulative_gas_used = cumulative_gas_used.saturating_add(result.gas_used());
                    self.db.commit(state);
                }
                Err(e) => {
                    return Err(L2SequencerError::Evm(format!("{e:?}")));
                }
            }
        }

        let state_root = compute_state_root(&self.db);
        Ok((state_root, cumulative_gas_used))
    }
}

/// Determine the sender address for a transaction.
///
/// Deposit transactions carry an explicit `from` field. Signed user
/// transactions are always from [`TEST_ACCOUNT_ADDRESS`] in this test harness.
const fn tx_sender(tx: &OpTxEnvelope, signer: &PrivateKeySigner) -> Address {
    match tx {
        OpTxEnvelope::Deposit(sealed) => sealed.inner().from,
        _ => signer.address(),
    }
}

/// Compute a Merkle Patricia Trie state root from the in-memory database.
///
/// Iterates over all accounts in the DB cache and builds a proper MPT root,
/// giving each account the correct storage root and code hash.
fn compute_state_root(db: &InMemoryDB) -> B256 {
    let accounts = db
        .cache
        .accounts
        .iter()
        .filter(|(_, db_account)| {
            !matches!(db_account.account_state, revm::database::AccountState::NotExisting)
        })
        .map(|(address, db_account)| {
            let storage_root = if db_account.storage.is_empty() {
                EMPTY_ROOT_HASH
            } else {
                alloy_trie::root::storage_root_unhashed(
                    db_account.storage.iter().map(|(slot, value)| (B256::from(*slot), *value)),
                )
            };

            let code_hash = db_account.info.code_hash;

            (
                *address,
                TrieAccount {
                    nonce: db_account.info.nonce,
                    balance: db_account.info.balance,
                    storage_root,
                    code_hash,
                },
            )
        });

    state_root_unhashed(accounts)
}

impl L2BlockProvider for L2Sequencer {
    fn next_block(&mut self) -> Option<OpBlock> {
        Some(
            self.build_next_block()
                .unwrap_or_else(|e| panic!("L2Sequencer::next_block failed: {e}")),
        )
    }
}
