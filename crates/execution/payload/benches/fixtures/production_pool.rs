//! Fixture for benches that select from the production Base transaction pool: a
//! [`BaseTransactionPool`] with the Base validator over an in-memory state provider, the
//! transaction kinds that can rest in it, and the pool maintenance a node runs around builds.
//!
//! The pool limits are explicit, so a baseline and a candidate build of the same bench have equal
//! headroom. Validation reads an in-memory provider, so admission is cheaper than on a node;
//! benches report it apart from selection.
//!
//! The pool is built as the production node builds it: a reth protocol pool wrapped by
//! [`BaseTransactionPool::new`], then configured with guard limits and a validity dormancy mode.
//! [`ProductionPool::maintain_canonical`] runs the node's canonical state-diff maintenance.

use std::{ops::Range, sync::Arc};

use alloy_consensus::{Header, SignableTransaction, TxEip1559};
use alloy_eips::{BlockNumHash, eip2718::Encodable2718};
use alloy_primitives::{Address, B256, Bytes, Signature, TxHash, TxKind, U256, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::{
    BaseBlock, BasePooledTransaction as ConsensusPooledTransaction, BasePrimitives,
    BaseTransactionSigned, BaseTxEnvelope, Eip8130Constants, Eip8130Signed, TxEip8130,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_evm::BaseEvmConfig;
use base_execution_txpool::{
    AccountStateDiff, BaseL1BlockInfo, BaseOrdering, BasePooledTransaction, BaseTransactionPool,
    BaseTransactionValidator, GuardLimits, StateDiffInvalidation, ValidityDormancyMode,
    ValidityOperator, ValidityPredicate,
};
use base_test_utils::build_test_genesis_everest;
use reth_chainspec::EthChainSpec;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::{Recovered, SealedBlock};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_tasks::Runtime;
use reth_transaction_pool::{
    CanonicalStateUpdate, Pool as ProtocolPool, PoolConfig, PoolTransaction, PoolUpdateKind,
    SubPoolLimit, TransactionOrigin, TransactionPool, TransactionPoolExt,
    blobstore::InMemoryBlobStore, validate::EthTransactionValidatorBuilder,
};
use revm::database::BundleState;

/// The in-memory state provider the pool validates against.
pub type Provider = MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>;
/// The production Base pool.
pub type Pool = BaseTransactionPool<Provider, InMemoryBlobStore, BaseEvmConfig>;
/// The production ordering, by coinbase tip.
pub type Ordering = BaseOrdering<BasePooledTransaction>;

/// Wall clock the pool starts at, in Unix seconds. Blocks advance it by [`BLOCK_TIME_SECS`].
pub const NOW_SECS: u64 = 1_700_000_000;
/// Seconds between the blocks the pool is told about.
pub const BLOCK_TIME_SECS: u64 = 1;
/// Gas a plain transfer uses.
pub const TRANSFER_GAS: u64 = 21_000;
/// Seconds a nonce-free EIP-8130 transaction admitted at [`NOW_SECS`] stays in the pool.
pub const NONCE_FREE_LIFETIME_SECS: u64 = Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW / 1_000;
/// Gas limit of each EIP-8130 transaction, above its intrinsic gas with no calls.
const EIP8130_GAS: u64 = 50_000;
/// Fee cap of every transaction, far above any base fee the benches set, so fee changes reorder
/// candidates without demoting any of them.
pub const MAX_FEE_PER_GAS: u128 = 1_000_000_000_000;
/// Tip of validity transactions, above [`ORDINARY_PRIORITY_FEE`] so every iterator reaches them
/// before ordinary traffic.
pub const VALIDITY_PRIORITY_FEE: u128 = 10;
/// Tip of ordinary transfers.
pub const ORDINARY_PRIORITY_FEE: u128 = 1;
/// Balance of every funded sender in the pool's provider.
const SENDER_BALANCE: u128 = 100_000_000_000_000_000_000;
/// Transactions each sub-pool admits: above the largest population plus every block of ordinary
/// traffic, so no bench evicts.
const SUB_POOL_TRANSACTIONS: usize = 1_000_000;
/// Bytes each sub-pool admits, sized with [`SUB_POOL_TRANSACTIONS`].
const SUB_POOL_BYTES: usize = 1 << 32;
/// Transactions admitted per validation batch.
const ADMISSION_BATCH: usize = 4_096;
/// Recipient of filler and resting transfers, whose distinct values keep otherwise identical
/// transfers apart: every transfer carries the same test signature.
const SINK: Address = Address::repeat_byte(0xee);

/// Address families, so senders, watched accounts, and filler never collide.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Accounts {
    /// Senders of validity transactions.
    Validity = 1,
    /// Accounts the validity predicates read; unfunded, so a balance predicate of one never
    /// holds until a bench funds the account.
    Watched = 2,
    /// Senders of ordinary transfers.
    Ordinary = 3,
    /// Senders of the transfers that satisfy resting validity transactions by funding their
    /// watched accounts.
    Satisfier = 4,
}

impl Accounts {
    /// A hash-derived address, so hash maps see keys distributed like real addresses.
    pub fn address(self, index: usize) -> Address {
        let mut preimage = [0u8; 9];
        preimage[0] = self as u8;
        preimage[1..].copy_from_slice(&(index as u64).to_be_bytes());
        Address::from_word(keccak256(preimage))
    }
}

/// The production pool, its provider, and the async runtime its validation runs on.
pub struct ProductionPool {
    /// The pool every bench builder selects from.
    pub pool: Pool,
    client: Provider,
    chain_id: u64,
    runtime: tokio::runtime::Runtime,
}

impl ProductionPool {
    /// Enters the pool's runtime, so pool work the builder hands to background tasks runs off the
    /// bench thread as it does on a node.
    pub fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.runtime.enter()
    }
}

impl std::fmt::Debug for ProductionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductionPool").field("chain_id", &self.chain_id).finish_non_exhaustive()
    }
}

impl ProductionPool {
    /// Creates an empty pool for `chain_id` under validity `dormancy`, with every Base upgrade
    /// through Everest active so it admits EIP-8130 transactions.
    pub fn new(chain_id: u64, dormancy: ValidityDormancyMode) -> Self {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = chain_id;
        Self::with_chain_spec(Arc::new(BaseChainSpec::from_genesis(genesis)), dormancy)
    }

    /// Creates an empty pool validating against `chain_spec` under validity `dormancy`, for a
    /// workload whose transactions must be admitted under the same upgrades its builder executes
    /// them under.
    pub fn with_chain_spec(chain_spec: Arc<BaseChainSpec>, dormancy: ValidityDormancyMode) -> Self {
        let runtime =
            tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
        let _guard = runtime.enter();
        let chain_id = chain_spec.chain_id();
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(client.clone(), evm_config)
            .no_shanghai()
            .no_cancun()
            .build_with_tasks(Runtime::test(), blob_store.clone())
            .map(|inner| {
                let validator =
                    BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
                let header = Header { timestamp: NOW_SECS, ..Default::default() };
                validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
                validator
            });
        let limit = SubPoolLimit::new(SUB_POOL_TRANSACTIONS, SUB_POOL_BYTES);
        let config = PoolConfig {
            pending_limit: limit,
            basefee_limit: limit,
            queued_limit: limit,
            ..PoolConfig::default()
        };
        let ordering = Ordering::coinbase_tip();
        let protocol_pool = ProtocolPool::new(validator, ordering.clone(), blob_store, config);
        let pool = BaseTransactionPool::new(protocol_pool, ordering)
            .with_guard_limits(GuardLimits::default())
            .with_validity_dormancy(dormancy);
        Self { pool, client, chain_id, runtime }
    }

    /// Funds `address` in the state the pool validates against.
    pub fn fund(&self, address: Address) {
        self.client.add_account(address, ExtendedAccount::new(0, U256::from(SENDER_BALANCE)));
    }

    /// Admits `transactions` as a forwarding mempool node would, returning their hashes in order.
    /// Panics if the pool rejects any of them.
    pub fn admit(&self, transactions: Vec<BasePooledTransaction>) -> Vec<TxHash> {
        let hashes: Vec<TxHash> = transactions.iter().map(|tx| *tx.hash()).collect();
        let mut transactions = transactions.into_iter().peekable();
        while transactions.peek().is_some() {
            let batch: Vec<_> = transactions.by_ref().take(ADMISSION_BATCH).collect();
            let added = self
                .runtime
                .block_on(self.pool.add_transactions(TransactionOrigin::External, batch));
            for result in added {
                result.expect("the production pool admits every bench transaction");
            }
        }
        hashes
    }

    /// Admits `count` ordinary transfers from fresh funded senders `first..first + count`,
    /// returning their hashes and senders.
    pub fn admit_ordinary(&self, first: usize, count: usize) -> (Vec<TxHash>, Vec<Address>) {
        let senders: Vec<Address> =
            (first..first + count).map(|index| Accounts::Ordinary.address(index)).collect();
        let transactions = senders
            .iter()
            .enumerate()
            .map(|(offset, sender)| {
                self.fund(*sender);
                self.eip1559(
                    *sender,
                    0,
                    ORDINARY_PRIORITY_FEE,
                    SINK,
                    U256::from(first + offset),
                    Vec::new(),
                )
            })
            .collect();
        (self.admit(transactions), senders)
    }

    /// Admits one ordinary transfer per lane in `lanes`, from fresh funded senders `first..`, that
    /// funds the lane's watched account with the one wei [`watched_balance_is_one`] requires,
    /// returning their hashes and senders.
    pub fn admit_satisfiers(&self, first: usize, lanes: &[usize]) -> (Vec<TxHash>, Vec<Address>) {
        let senders: Vec<Address> =
            (first..first + lanes.len()).map(|index| Accounts::Satisfier.address(index)).collect();
        let transactions = senders
            .iter()
            .zip(lanes)
            .map(|(sender, lane)| {
                self.fund(*sender);
                self.eip1559(
                    *sender,
                    0,
                    ORDINARY_PRIORITY_FEE,
                    Accounts::Watched.address(*lane),
                    U256::ONE,
                    Vec::new(),
                )
            })
            .collect();
        (self.admit(transactions), senders)
    }

    /// Admits resting lanes `lanes` of `kind`, lane `i` gated by `predicates(i)`, returning every
    /// admitted hash and the protocol senders the build state must fund.
    pub fn admit_resting(
        &self,
        kind: RestingKind,
        lanes: Range<usize>,
        predicates: impl Fn(usize) -> Vec<ValidityPredicate>,
    ) -> (Vec<TxHash>, Vec<Address>) {
        let mut senders = Vec::with_capacity(lanes.len());
        let mut transactions = Vec::with_capacity(lanes.len() * kind.transactions_per_lane());
        for lane in lanes {
            match kind {
                RestingKind::Protocol | RestingKind::ProtocolDescendants => {
                    let sender = Accounts::Validity.address(lane);
                    self.fund(sender);
                    senders.push(sender);
                    transactions.push(self.eip1559(
                        sender,
                        0,
                        VALIDITY_PRIORITY_FEE,
                        SINK,
                        U256::from(lane),
                        predicates(lane),
                    ));
                    for nonce in 1..kind.transactions_per_lane() as u64 {
                        transactions.push(self.eip1559(
                            sender,
                            nonce,
                            VALIDITY_PRIORITY_FEE,
                            SINK,
                            U256::from(lane),
                            Vec::new(),
                        ));
                    }
                }
                RestingKind::Channel | RestingKind::NonceFree => {
                    let signer = signer(lane);
                    self.fund(signer.address());
                    let (nonce_key, valid_before) = match kind {
                        RestingKind::Channel => (U256::from(1), 0),
                        _ => (
                            Eip8130Constants::NONCE_KEY_MAX,
                            NOW_SECS * 1_000 + Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
                        ),
                    };
                    transactions.push(
                        self.eip8130(&signer, nonce_key, valid_before)
                            .with_validity_predicates(predicates(lane)),
                    );
                }
            }
        }
        (self.admit(transactions), senders)
    }

    /// Tells the pool block `number` became canonical with `base_fee`, mining `mined` from
    /// `senders` (each now at nonce one), and that the next block's base fee is
    /// `pending_base_fee`, as the node's pool maintenance does after every block.
    pub fn mine(
        &self,
        number: u64,
        base_fee: u64,
        pending_base_fee: u64,
        mined: Vec<TxHash>,
        senders: &[Address],
    ) {
        let _guard = self.runtime.enter();
        let tip = SealedBlock::seal_slow(BaseBlock {
            header: Header {
                number,
                timestamp: NOW_SECS + number * BLOCK_TIME_SECS,
                base_fee_per_gas: Some(base_fee),
                gas_limit: 30_000_000,
                ..Default::default()
            },
            body: Default::default(),
        });
        let changed_accounts = senders
            .iter()
            .map(|&address| ChangedAccount {
                address,
                nonce: 1,
                balance: U256::from(SENDER_BALANCE),
            })
            .collect();
        self.pool.on_canonical_state_change(CanonicalStateUpdate {
            new_tip: &tip,
            pending_block_base_fee: pending_base_fee,
            pending_block_blob_fee: None,
            changed_accounts,
            mined_transactions: mined,
            update_kind: PoolUpdateKind::Commit,
        });
    }

    /// Runs the node's canonical state-diff maintenance for block `canonical`, whose changes
    /// `bundle` holds: invalidates guarded transactions the diff affects, then advances validity
    /// dormancy, which checks the builder's tip and classifies queued transactions against the
    /// provider's state, as `maintain_state_diff_invalidation` does for each committed block. The
    /// blocking work runs on the calling thread rather than a blocking pool, so the caller can
    /// time it.
    pub fn maintain_canonical(&self, canonical: BlockNumHash, bundle: &BundleState) {
        let diffs = AccountStateDiff::collect(bundle);
        if !diffs.is_empty() {
            self.pool.invalidate_from_state_diff(&diffs);
        }
        self.pool.advance_dormancy(&[canonical]);
    }

    /// Removes `hashes` from the pool, as a cancellation or eviction does.
    pub fn remove(&self, hashes: Vec<TxHash>) {
        let count = hashes.len();
        let removed = self.pool.remove_transactions(hashes);
        assert_eq!(removed.len(), count, "every removed transaction was pooled");
    }

    /// Returns whether the pool holds `hash`.
    pub fn contains(&self, hash: &TxHash) -> bool {
        self.pool.get(hash).is_some()
    }

    fn eip1559(
        &self,
        sender: Address,
        nonce: u64,
        priority_fee: u128,
        to: Address,
        value: U256,
        predicates: Vec<ValidityPredicate>,
    ) -> BasePooledTransaction {
        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit: TRANSFER_GAS,
            max_fee_per_gas: MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: priority_fee,
            to: TxKind::Call(to),
            value,
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(Signature::test_signature()));
        let encoded_length = envelope.encode_2718_len();
        BasePooledTransaction::new(
            Recovered::new_unchecked(BaseTransactionSigned::from(envelope), sender),
            encoded_length,
        )
        .with_validity_predicates(predicates)
    }

    /// A self-paid EIP-8130 transaction with no calls; the validator checks its signature.
    fn eip8130(
        &self,
        signer: &PrivateKeySigner,
        nonce_key: U256,
        valid_before: u64,
    ) -> BasePooledTransaction {
        let tx = TxEip8130 {
            chain_id: self.chain_id,
            sender: None,
            nonce_key,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before,
            max_priority_fee_per_gas: VALIDITY_PRIORITY_FEE,
            max_fee_per_gas: MAX_FEE_PER_GAS,
            gas_limit: EIP8130_GAS,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).expect("signs");
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        BasePooledTransaction::from_pooled(Recovered::new_unchecked(
            ConsensusPooledTransaction::Eip8130(signed),
            signer.address(),
        ))
    }
}

/// The lane a resting validity transaction occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestingKind {
    /// A protocol-nonce lane holding only the gated transaction.
    Protocol,
    /// A protocol-nonce lane whose gated head blocks [`DESCENDANTS`] plain successors.
    ProtocolDescendants,
    /// An EIP-8130 nonce channel.
    Channel,
    /// A nonce-free EIP-8130 transaction, which expires
    /// [`Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW`] after [`NOW_SECS`].
    NonceFree,
}

/// Plain successors behind each gated head of [`RestingKind::ProtocolDescendants`].
pub const DESCENDANTS: usize = 3;

impl RestingKind {
    /// Pool transactions in each lane.
    pub const fn transactions_per_lane(self) -> usize {
        match self {
            Self::ProtocolDescendants => 1 + DESCENDANTS,
            Self::Protocol | Self::Channel | Self::NonceFree => 1,
        }
    }
}

/// A deterministic EIP-8130 signer, so runs admit the same transactions.
fn signer(index: usize) -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(U256::from(index as u64 + 1)))
        .expect("small keys are valid")
}

/// A balance predicate on the unfunded watched account `index`, unsatisfied until a bench funds
/// that account with exactly one wei.
pub fn watched_balance_is_one(index: usize) -> ValidityPredicate {
    ValidityPredicate::Balance {
        address: Accounts::Watched.address(index),
        op: ValidityOperator::Equal,
        value: U256::ONE,
    }
}

/// A predicate that holds through block `deadline`, after which the pool expires the transaction.
pub fn valid_through_block(deadline: u64) -> ValidityPredicate {
    ValidityPredicate::BlockNumber {
        op: ValidityOperator::LessThanOrEqual,
        value: U256::from(deadline),
    }
}
