use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use alloy_consensus::{Header, SignableTransaction};
use alloy_eips::{BlockNumHash, eip2718::Decodable2718};
use alloy_hardforks::ForkCondition;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_trie::{EMPTY_ROOT_HASH, TrieAccount, root::state_root_unhashed};
use base_alloy_consensus::{OpBlock, OpTxEnvelope};
use base_alloy_upgrades::BaseUpgrade;
use base_consensus_genesis::{L1ChainConfig, RollupConfig, SystemConfig};
use base_execution_chainspec::OpChainSpecBuilder;
use base_execution_evm::OpEvmConfig;
use base_protocol::{BlockInfo, L1BlockInfoTx, L2BlockInfo, OpAttributesWithParent};
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

/// A test account with nonce tracking and signing capability.
///
/// Wraps a [`PrivateKeySigner`] with an auto-incrementing nonce so callers
/// can build correctly-sequenced signed transactions without manual bookkeeping.
/// Shared via [`Arc`] so the sequencer and external test code stay in sync.
#[derive(Debug)]
pub struct TestAccount {
    signer: PrivateKeySigner,
    nonce: u64,
}

impl TestAccount {
    /// Create a new test account from a private key with nonce starting at 0.
    pub fn new(key: B256) -> Self {
        let signer = PrivateKeySigner::from_bytes(&key).expect("valid key");
        Self { signer, nonce: 0 }
    }

    /// Return the address derived from this account's private key.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }

    /// Sign a pre-built EIP-1559 transaction without modifying the nonce.
    ///
    /// The caller is responsible for setting the correct nonce in the
    /// transaction fields before calling this method.
    pub fn sign_tx(
        &mut self,
        tx: alloy_consensus::TxEip1559,
    ) -> Result<OpTxEnvelope, alloy_signer::Error> {
        let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
        Ok(OpTxEnvelope::Eip1559(tx.into_signed(sig)))
    }

    /// Creates and signs a minimal EIP-1559 transfer, auto-incrementing the nonce.
    pub fn create_eip1559_tx(&mut self, chain_id: u64) -> OpTxEnvelope {
        self.create_tx(chain_id, TxKind::Call(Address::ZERO), Bytes::new(), U256::from(1), 21_000)
    }

    /// Creates and signs a custom EIP-1559 transaction, auto-incrementing the nonce.
    ///
    /// The caller provides the destination, calldata, value, and gas limit.
    /// Chain-level fields (`chain_id`, `nonce`, fee caps) are filled in automatically.
    pub fn create_tx(
        &mut self,
        chain_id: u64,
        to: TxKind,
        input: Bytes,
        value: U256,
        gas_limit: u64,
    ) -> OpTxEnvelope {
        let tx = alloy_consensus::TxEip1559 {
            chain_id,
            nonce: self.nonce,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit,
            to,
            value,
            input,
            access_list: Default::default(),
        };
        let sig = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("test account signing must not fail");
        self.nonce += 1;
        OpTxEnvelope::Eip1559(tx.into_signed(sig))
    }

    /// Return the current nonce.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }
}

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

type BlockHashInner = Arc<Mutex<HashMap<u64, (B256, Option<B256>)>>>;

/// Shared L2 block hashes and state roots keyed by block number.
///
/// `L2Sequencer` writes into this registry as blocks are built, and
/// `L2Verifier` reads from the same registry when it applies derived
/// attributes so the resulting safe-head hash chain matches the sequencer's
/// sealed headers. When a [`StatefulL2Executor`] is attached to the verifier,
/// it also reads the stored state root for post-derivation execution
/// validation.
///
/// The state root field is `Option<B256>`: it is `Some` only when the entry
/// was produced by real EVM execution (e.g. via [`L2Sequencer`] or
/// [`L2Verifier::act_l2_unsafe_gossip_receive`]). Entries created with
/// [`L2Verifier::register_block_hash`] store `None`, which causes the
/// executor to skip state-root validation for that block rather than panic
/// against a bogus sentinel value.
///
/// [`L2Verifier::act_l2_unsafe_gossip_receive`]: crate::L2Verifier::act_l2_unsafe_gossip_receive
/// [`L2Verifier::register_block_hash`]: crate::L2Verifier::register_block_hash
#[derive(Debug, Clone, Default)]
pub struct SharedBlockHashRegistry(BlockHashInner);

impl SharedBlockHashRegistry {
    /// Create an empty shared registry.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Record the block hash and optional state root for an L2 block number.
    ///
    /// Pass `Some(state_root)` when the block was produced by real EVM
    /// execution so that an attached [`StatefulL2Executor`] can validate it.
    /// Pass `None` for synthetic blocks (e.g. via
    /// [`L2Verifier::register_block_hash`]); the executor will skip
    /// state-root validation for those blocks.
    pub fn insert(&self, number: u64, hash: B256, state_root: Option<B256>) {
        self.0
            .lock()
            .expect("block hash registry lock poisoned")
            .insert(number, (hash, state_root));
    }

    /// Return the registered block hash for an L2 block number.
    pub fn get(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).map(|(h, _)| *h)
    }

    /// Return the registered state root for an L2 block number, if any.
    ///
    /// Returns `None` when the block was not registered or was registered
    /// without a state root (e.g. via [`L2Verifier::register_block_hash`]).
    pub fn get_state_root(&self, number: u64) -> Option<B256> {
        self.0.lock().expect("block hash registry lock poisoned").get(&number).and_then(|(_, s)| *s)
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
    /// Test account used for signing user transactions.
    test_account: Arc<Mutex<TestAccount>>,
    /// In-process EVM executor carrying state across blocks.
    executor: StatefulL2Executor,
    /// Optional pinned L1 origin. When set, epoch selection is bypassed and
    /// this block is used as the epoch for every subsequent L2 block built.
    l1_origin_pin: Option<BlockInfo>,
    /// Shared registry of built L2 block hashes, keyed by block number.
    block_hashes: SharedBlockHashRegistry,
}

impl L2Sequencer {
    /// Create a new sequencer starting from the given L2 genesis head.
    pub fn new(
        head: L2BlockInfo,
        l1_chain: SharedL1Chain,
        rollup_config: RollupConfig,
        system_config: SystemConfig,
    ) -> Self {
        let test_account = Arc::new(Mutex::new(TestAccount::new(TEST_ACCOUNT_KEY)));
        let executor = StatefulL2Executor::new(rollup_config.clone());

        Self {
            head,
            l1_chain,
            rollup_config,
            l1_chain_config: L1ChainConfig::default(),
            system_config,
            test_account,
            executor,
            l1_origin_pin: None,
            block_hashes: SharedBlockHashRegistry::new(),
        }
    }

    /// Return the current unsafe L2 head.
    pub const fn head(&self) -> L2BlockInfo {
        self.head
    }

    /// Return a shared handle to the sequencer's test account.
    ///
    /// External test code can use this to build signed transactions with
    /// correct nonce tracking, independent of the sequencer.
    pub fn test_account(&self) -> Arc<Mutex<TestAccount>> {
        Arc::clone(&self.test_account)
    }

    /// Returns a reference to the sequencer's in-memory EVM database.
    ///
    /// Callers can inspect account state and storage written by executed
    /// transactions.
    pub const fn db(&self) -> &InMemoryDB {
        self.executor.db()
    }

    /// Return the sequencer's shared block-hash registry.
    pub fn block_hash_registry(&self) -> SharedBlockHashRegistry {
        self.block_hashes.clone()
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
    /// # Panics
    ///
    /// Panics if the block cannot be built (e.g. missing L1 block data).
    ///
    /// [`build_next_block`]: L2Sequencer::build_next_block
    pub fn build_empty_block(&mut self) -> OpBlock {
        self.build_next_block_with_transactions(vec![])
    }

    /// Build the next L2 block with a single transaction.
    pub fn build_next_block_with_single_transaction(&mut self) -> OpBlock {
        let tx = {
            let mut account = self.test_account.lock().expect("test account lock poisoned");
            account.create_eip1559_tx(self.rollup_config.l2_chain_id.id())
        };
        self.build_next_block_with_transactions(vec![tx])
    }

    /// Build the next L2 block and advance the internal head.
    ///
    /// Returns a fully-formed [`OpBlock`] containing the L1-info deposit and
    /// any configured user transactions, with a real state root and block hash.
    ///
    /// # Panics
    ///
    /// Panics if the block cannot be built (e.g. missing L1 block data or EVM
    /// execution failure). Use [`try_build_next_block`] if you need to inspect
    /// the error.
    ///
    /// [`try_build_next_block`]: L2Sequencer::try_build_next_block
    pub fn build_next_block_with_transactions(
        &mut self,
        transactions: Vec<OpTxEnvelope>,
    ) -> OpBlock {
        self.try_build_next_block_with_transactions(transactions)
            .unwrap_or_else(|e| panic!("L2Sequencer::build_next_block failed: {e}"))
    }

    /// Build the next L2 block, returning an error instead of panicking.
    ///
    /// Prefer [`build_next_block`] in test code; this method exists for
    /// callers that need to inspect the failure reason.
    ///
    /// [`build_next_block`]: L2Sequencer::build_next_block
    pub fn try_build_next_block_with_transactions(
        &mut self,
        transactions: Vec<OpTxEnvelope>,
    ) -> Result<OpBlock, L2SequencerError> {
        let mut transactions = transactions;
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

        transactions.insert(0, OpTxEnvelope::Deposit(deposit_tx));

        // Execute transactions against the in-memory EVM.
        let (state_root, gas_used) = self.executor.execute_transactions(
            &transactions,
            next_number,
            next_timestamp,
            parent_hash,
        )?;

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
        self.block_hashes.insert(next_number, block_hash, Some(state_root));

        Ok(block)
    }
}

/// Determine the sender address for a transaction.
///
/// Deposit transactions carry an explicit `from` field. Signed user
/// transactions are always from the given `default_sender` in this test harness.
const fn tx_sender(tx: &OpTxEnvelope, default_sender: Address) -> Address {
    match tx {
        OpTxEnvelope::Deposit(sealed) => sealed.inner().from,
        _ => default_sender,
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
        Some(self.build_next_block_with_single_transaction())
    }
}

/// Decode raw EIP-2718-encoded transaction bytes into [`OpTxEnvelope`]s.
fn decode_raw_transactions(raw_txs: &[Bytes]) -> Result<Vec<OpTxEnvelope>, L2SequencerError> {
    raw_txs
        .iter()
        .map(|raw| {
            OpTxEnvelope::decode_2718(&mut raw.as_ref())
                .map_err(|e| L2SequencerError::Evm(format!("tx decode: {e}")))
        })
        .collect()
}

/// In-process EVM re-executor for validating derived L2 blocks.
///
/// Mirrors the [`L2Sequencer`]'s execution environment — same seeded test
/// account, same EVM configuration, same state-root computation. Call
/// [`execute_attrs`] in L2-block order so the internal EVM state advances
/// identically to the sequencer's. The returned state root can be compared
/// against the value stored in [`SharedBlockHashRegistry`] to confirm that
/// the derivation pipeline re-produces the exact same execution result.
///
/// [`execute_attrs`]: StatefulL2Executor::execute_attrs
#[derive(Debug)]
pub struct StatefulL2Executor {
    db: InMemoryDB,
    rollup_config: RollupConfig,
}

impl StatefulL2Executor {
    /// Create a new executor with the standard test account seeded.
    ///
    /// The initial EVM state matches the [`L2Sequencer`]'s genesis: the
    /// [`TEST_ACCOUNT_ADDRESS`] is funded with [`TEST_ACCOUNT_BALANCE`].
    pub fn new(rollup_config: RollupConfig) -> Self {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            TEST_ACCOUNT_ADDRESS,
            AccountInfo { balance: TEST_ACCOUNT_BALANCE, ..Default::default() },
        );
        Self { db, rollup_config }
    }

    /// Returns a reference to the internal EVM database.
    ///
    /// Callers can inspect account state and storage written by executed
    /// transactions.
    pub const fn db(&self) -> &InMemoryDB {
        &self.db
    }

    /// Execute the transactions from `attrs` and return the resulting state root.
    ///
    /// Decodes each raw EIP-2718-encoded transaction from
    /// `attrs.attributes.transactions`, executes them in order against the
    /// internal EVM database, and returns the Merkle Patricia Trie state root.
    /// The internal state persists between calls so repeated invocations
    /// mirror the sequencer's block-by-block execution.
    pub fn execute_attrs(
        &mut self,
        attrs: &OpAttributesWithParent,
        block_number: u64,
        parent_hash: B256,
    ) -> Result<B256, L2SequencerError> {
        let timestamp = attrs.attributes.payload_attributes.timestamp;
        let txs = decode_raw_transactions(attrs.attributes.transactions.as_deref().unwrap_or(&[]))?;
        let (state_root, _gas_used) =
            self.execute_transactions(&txs, block_number, timestamp, parent_hash)?;
        Ok(state_root)
    }

    /// Execute transactions against the internal EVM database and return
    /// `(state_root, cumulative_gas_used)`.
    ///
    /// This is the single authoritative execution path shared by both
    /// [`L2Sequencer`] (which needs the gas total for header construction) and
    /// [`execute_attrs`] (which only needs the state root for validation).
    ///
    /// [`execute_attrs`]: StatefulL2Executor::execute_attrs
    pub fn execute_transactions(
        &mut self,
        transactions: &[OpTxEnvelope],
        block_number: u64,
        timestamp: u64,
        parent_hash: B256,
    ) -> Result<(B256, u64), L2SequencerError> {
        let mut spec_builder =
            OpChainSpecBuilder::base_mainnet().chain(self.rollup_config.l2_chain_id);

        if let Some(ts) = self.rollup_config.hardforks.base.v1 {
            spec_builder = spec_builder.with_fork(BaseUpgrade::V1, ForkCondition::Timestamp(ts));
        }

        let chain_spec = Arc::new(spec_builder.build());
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
            let sender = tx_sender(tx, TEST_ACCOUNT_ADDRESS);
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

        Ok((compute_state_root(&self.db), cumulative_gas_used))
    }
}
