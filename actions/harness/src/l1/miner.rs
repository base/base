use alloy_consensus::{
    Header, Receipt, ReceiptEnvelope, SignableTransaction, Transaction, TxEip1559, TxEnvelope,
    transaction::{SignerRecoverable, TransactionMeta},
};
use alloy_eips::eip4844::Blob;
use alloy_primitives::{Address, B256, Bloom, Bytes, Log, LogData, TxKind, U256};
use alloy_rpc_types_eth::{Log as RpcLog, TransactionReceipt};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_genesis::SystemConfigUpdate;
use base_protocol::{BlockInfo, Deposits};
use tracing::info;

use crate::Action;

const EVENT_TX_CHAIN_ID: u64 = 0;

/// Convert an [`L1Block`] reference to a [`BlockInfo`].
pub fn block_info_from(block: &L1Block) -> BlockInfo {
    BlockInfo {
        hash: block.hash(),
        number: block.number(),
        parent_hash: block.header.parent_hash,
        timestamp: block.timestamp(),
    }
}

/// Parameters for a user deposit transaction, passed to [`L1Miner::enqueue_user_deposit`].
///
/// Mirrors the fields of the on-chain `OptimismPortal.sol` `TransactionDeposited` event.
#[derive(Debug, Clone)]
pub struct UserDeposit {
    /// The L1 portal contract address that emits the deposit event.
    pub deposit_contract: Address,
    /// The L1 sender address.
    pub from: Address,
    /// The L2 recipient address.
    pub to: Address,
    /// ETH value to mint on L2 (in wei, as u128).
    pub mint: u128,
    /// ETH value to transfer on L2.
    pub value: U256,
    /// Gas limit for the L2 deposit transaction.
    pub gas_limit: u64,
    /// Calldata for the L2 deposit transaction.
    pub data: Vec<u8>,
}

/// Error returned by [`L1Miner::reorg_to`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReorgError {
    /// The requested block number is beyond the current chain tip.
    #[error("cannot reorg to block {requested}: chain tip is at block {tip}")]
    BeyondTip {
        /// The requested reorg target.
        requested: u64,
        /// The current chain tip.
        tip: u64,
    },
}

/// Configuration for the [`L1Miner`].
#[derive(Debug, Clone)]
pub struct L1MinerConfig {
    /// Simulated L1 block time in seconds. Post-merge Ethereum uses 12 s.
    pub block_time: u64,
}

impl Default for L1MinerConfig {
    fn default() -> Self {
        Self { block_time: 12 }
    }
}

/// Builder for production-shaped signed L1 test transactions.
#[derive(Debug)]
pub struct L1TxBuilder;

impl L1TxBuilder {
    /// Build a signed EIP-1559 calldata transaction.
    pub fn signed_calldata(
        signer: &PrivateKeySigner,
        chain_id: u64,
        nonce: u64,
        to: Address,
        input: Bytes,
    ) -> Result<TxEnvelope, alloy_signer::Error> {
        let tx = TxEip1559 {
            chain_id,
            nonce,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input,
            access_list: Default::default(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash())?;
        Ok(TxEnvelope::Eip1559(tx.into_signed(signature)))
    }
}

/// A signed L1 transaction waiting to be mined, with the logs its receipt should carry.
#[derive(Debug, Clone)]
pub struct L1PendingTransaction {
    /// Signed L1 transaction body.
    pub envelope: TxEnvelope,
    /// Consensus logs emitted by this transaction.
    pub logs: Vec<Log>,
    /// Whether the transaction receipt succeeded.
    pub success: bool,
}

impl L1PendingTransaction {
    /// Create a pending L1 transaction with logs for its eventual receipt.
    pub const fn new(envelope: TxEnvelope, logs: Vec<Log>) -> Self {
        Self { envelope, logs, success: true }
    }

    /// Create a pending L1 transaction with logs and an explicit receipt status.
    pub const fn new_with_status(envelope: TxEnvelope, logs: Vec<Log>, success: bool) -> Self {
        Self { envelope, logs, success }
    }
}

/// An L1 block produced by the [`L1Miner`].
///
/// Mirrors the structure the derivation pipeline reads from L1: a block
/// header (for timestamp, number, and hash chaining), ordered signed
/// transaction bodies, matching receipts, and any EIP-4844 blob sidecars.
#[derive(Debug, Clone)]
pub struct L1Block {
    /// Consensus header.
    pub header: Header,
    /// Signed L1 transactions included in this block, in submission order.
    pub transactions: Vec<TxEnvelope>,
    /// Consensus receipts for transactions included in this block.
    pub receipts: Vec<Receipt>,
    /// Receipts for transactions included in this block.
    pub transaction_receipts: Vec<TransactionReceipt>,
    /// EIP-4844 blob sidecars attached to this block.
    ///
    /// Each entry is a `(versioned_hash, blob_data)` pair. Action tests store
    /// blobs here rather than in a separate beacon chain, and
    /// [`ActionBlobProvider`](crate::ActionBlobProvider) looks them up by hash.
    pub blob_sidecars: Vec<(B256, Box<Blob>)>,
}

impl L1Block {
    /// Return the block number.
    pub const fn number(&self) -> u64 {
        self.header.number
    }

    /// Return the block timestamp.
    pub const fn timestamp(&self) -> u64 {
        self.header.timestamp
    }

    /// Compute the block hash by hashing the header fields.
    ///
    /// Uses alloy's `Header::hash_slow` which hashes the RLP-encoded header.
    pub fn hash(&self) -> B256 {
        self.header.hash_slow()
    }
}

/// Simulated L1 block producer for action tests.
///
/// `L1Miner` maintains an in-memory chain of [`L1Block`]s, starting from a
/// genesis block at number 0, timestamp 0. Each call to [`mine_block`]
/// (or [`Action::act`]) advances the chain by one block, draining any
/// pending signed transactions into the new block's body.
///
/// The miner tracks safe and finalized head pointers via explicit movable
/// numbers. Tests advance them with [`act_l1_safe_next`],
/// [`act_l1_finalize_next`], [`act_l1_safe`], and [`act_l1_finalize`].
///
/// # Reorgs
///
/// [`reorg_to`] truncates the canonical chain back to a given block number,
/// discarding all later blocks and returning them so tests can inspect which
/// transactions were reorged out. After a reorg the caller can submit new
/// transactions and mine new blocks as normal — the new fork diverges from the
/// reorg point.
///
/// To guarantee that post-reorg blocks have distinct hashes from the original
/// blocks at the same heights (even when their contents happen to be
/// identical), the miner stamps a monotonically increasing `fork_id` into
/// every block's `extra_data`. The `fork_id` increments on each call to
/// `reorg_to`, so blocks mined on the new fork always differ from those on
/// the old one.
///
/// [`mine_block`]: L1Miner::mine_block
/// [`reorg_to`]: L1Miner::reorg_to
/// [`act_l1_safe_next`]: L1Miner::act_l1_safe_next
/// [`act_l1_finalize_next`]: L1Miner::act_l1_finalize_next
/// [`act_l1_safe`]: L1Miner::act_l1_safe
/// [`act_l1_finalize`]: L1Miner::act_l1_finalize
#[derive(Debug)]
pub struct L1Miner {
    /// All blocks produced so far, indexed by block number.
    blocks: Vec<L1Block>,
    /// Signed transactions waiting to be included in the next block.
    pending_transactions: Vec<L1PendingTransaction>,
    /// EIP-4844 blob sidecars to be attached to the next mined block.
    pending_blobs: Vec<(B256, Box<Blob>)>,
    /// Configuration.
    config: L1MinerConfig,
    /// Monotonically increasing fork counter, stamped into every block's
    /// `extra_data` to ensure distinct hashes across forks.
    fork_id: u64,
    /// The L1 block number that is considered safe.
    ///
    /// Advanced explicitly via [`act_l1_safe_next`] or [`act_l1_safe`].
    ///
    /// [`act_l1_safe_next`]: L1Miner::act_l1_safe_next
    /// [`act_l1_safe`]: L1Miner::act_l1_safe
    safe_number: u64,
    /// The L1 block number that is considered finalized.
    ///
    /// Advanced explicitly via [`act_l1_finalize_next`] or [`act_l1_finalize`].
    ///
    /// [`act_l1_finalize_next`]: L1Miner::act_l1_finalize_next
    /// [`act_l1_finalize`]: L1Miner::act_l1_finalize
    finalized_number: u64,
    /// Deterministic signer used for synthetic L1 event transactions.
    event_signer: PrivateKeySigner,
    /// Next nonce for synthetic L1 event transactions.
    event_nonce: u64,
}

impl L1Miner {
    /// Create a new [`L1Miner`] initialised with a genesis block.
    pub fn new(config: L1MinerConfig) -> Self {
        let genesis = L1Block {
            header: Header { number: 0, timestamp: 0, ..Default::default() },
            transactions: vec![],
            receipts: vec![],
            transaction_receipts: vec![],
            blob_sidecars: vec![],
        };
        Self {
            blocks: vec![genesis],
            pending_transactions: vec![],
            pending_blobs: vec![],
            config,
            fork_id: 0,
            safe_number: 0,
            finalized_number: 0,
            event_signer: PrivateKeySigner::from_bytes(&B256::repeat_byte(0xE1))
                .expect("valid event signer"),
            event_nonce: 0,
        }
    }

    /// Queue a [`Log`] as a signed synthetic L1 event transaction in the next
    /// mined block.
    ///
    /// The harness does not execute L1 contracts, so event helpers create a
    /// signed transaction to the emitting contract and attach the log to that
    /// transaction's receipt. This keeps derivation-facing receipts shaped like
    /// production blocks while preserving deterministic in-memory tests.
    ///
    /// [`mine_block`]: L1Miner::mine_block
    pub fn enqueue_log(&mut self, log: Log) {
        self.enqueue_log_with_status(log, true);
    }

    /// Queue a [`Log`] as a signed synthetic L1 event transaction with an
    /// explicit receipt status in the next mined block.
    pub fn enqueue_log_with_status(&mut self, log: Log, success: bool) {
        let tx = L1TxBuilder::signed_calldata(
            &self.event_signer,
            EVENT_TX_CHAIN_ID,
            self.event_nonce,
            log.address,
            Bytes::new(),
        )
        .expect("event transaction signs");
        self.event_nonce += 1;
        self.submit_transaction_with_logs_and_status(tx, vec![log], success);
    }

    /// Queue an EIP-4844 blob sidecar for inclusion in the next mined block.
    ///
    /// The `(hash, blob)` pair is stored in [`L1Block::blob_sidecars`] when
    /// the next block is mined. [`ActionBlobProvider`](crate::ActionBlobProvider)
    /// looks blobs up from this sidecar by versioned hash.
    pub fn enqueue_blob(&mut self, hash: B256, blob: Box<Blob>) {
        self.pending_blobs.push((hash, blob));
    }

    /// Queue a `ConfigUpdate(Batcher)` log for the next mined block.
    ///
    /// Encodes a batcher address rotation to `new_batcher`. The derivation
    /// pipeline reads this from L1 receipts and updates its internal
    /// [`SystemConfig`] for subsequent L2 blocks.
    ///
    /// [`SystemConfig`]: base_common_genesis::SystemConfig
    pub fn enqueue_batcher_update(&mut self, l1_sys_cfg_addr: Address, new_batcher: Address) {
        let mut data = [0u8; 96];
        data[31] = 0x20; // pointer → offset 32
        data[63] = 0x20; // length  → 32 bytes
        data[76..96].copy_from_slice(new_batcher.as_slice()); // address, right-aligned
        self.enqueue_log(Log {
            address: l1_sys_cfg_addr,
            data: LogData::new_unchecked(
                vec![SystemConfigUpdate::TOPIC, SystemConfigUpdate::EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        });
    }

    /// Queue a `ConfigUpdate(GasConfig)` log for the next mined block.
    ///
    /// Encodes a GPO `overhead` and `scalar` update. The derivation pipeline
    /// applies this through the `SystemConfig` update mechanism.
    pub fn enqueue_gas_config_update(
        &mut self,
        l1_sys_cfg_addr: Address,
        overhead: u64,
        scalar: u64,
    ) {
        let mut data = [0u8; 128];
        data[31] = 0x20; // pointer = 32
        data[63] = 0x40; // length  = 64
        data[88..96].copy_from_slice(&overhead.to_be_bytes());
        data[120..128].copy_from_slice(&scalar.to_be_bytes());
        let mut update_type = [0u8; 32];
        update_type[31] = 1; // GasConfig = 1
        self.enqueue_log(Log {
            address: l1_sys_cfg_addr,
            data: LogData::new_unchecked(
                vec![
                    SystemConfigUpdate::TOPIC,
                    SystemConfigUpdate::EVENT_VERSION_0,
                    B256::from(update_type),
                ],
                data.into(),
            ),
        });
    }

    /// Queue a `ConfigUpdate(GasLimit)` log for the next mined block.
    pub fn enqueue_gas_limit_update(&mut self, l1_sys_cfg_addr: Address, gas_limit: u64) {
        let mut data = [0u8; 96];
        data[31] = 0x20; // pointer = 32
        data[63] = 0x20; // length  = 32
        data[88..96].copy_from_slice(&gas_limit.to_be_bytes());
        let mut update_type = [0u8; 32];
        update_type[31] = 2; // GasLimit = 2
        self.enqueue_log(Log {
            address: l1_sys_cfg_addr,
            data: LogData::new_unchecked(
                vec![
                    SystemConfigUpdate::TOPIC,
                    SystemConfigUpdate::EVENT_VERSION_0,
                    B256::from(update_type),
                ],
                data.into(),
            ),
        });
    }

    /// Queue a `ConfigUpdate(OperatorFee)` log for the next mined block.
    ///
    /// Encodes an operator fee update with the given `scalar` and `constant`.
    pub fn enqueue_operator_fee_update(
        &mut self,
        l1_sys_cfg_addr: Address,
        operator_fee_scalar: u32,
        operator_fee_constant: u64,
    ) {
        let mut data = [0u8; 96];
        data[31] = 0x20; // pointer = 32
        data[63] = 0x20; // length  = 32
        data[84..88].copy_from_slice(&operator_fee_scalar.to_be_bytes());
        data[88..96].copy_from_slice(&operator_fee_constant.to_be_bytes());
        let mut update_type = [0u8; 32];
        update_type[31] = 5; // OperatorFee = 5
        self.enqueue_log(Log {
            address: l1_sys_cfg_addr,
            data: LogData::new_unchecked(
                vec![
                    SystemConfigUpdate::TOPIC,
                    SystemConfigUpdate::EVENT_VERSION_0,
                    B256::from(update_type),
                ],
                data.into(),
            ),
        });
    }

    /// Queue a `TransactionDeposited` log for the next mined block.
    ///
    /// Mirrors the on-chain `OptimismPortal.sol` `TransactionDeposited` event.
    /// The derivation pipeline reads this from L1 receipts to include the
    /// deposit in the corresponding L2 block's attribute set.
    pub fn enqueue_user_deposit(&mut self, deposit: &UserDeposit) {
        self.enqueue_user_deposit_with_status(deposit, true);
    }

    /// Queue a `TransactionDeposited` log for the next mined block with an
    /// explicit receipt status.
    pub fn enqueue_user_deposit_with_status(&mut self, deposit: &UserDeposit, success: bool) {
        // opaqueData: mint(32) + value(32) + gas_limit(8) + isCreation(1) + calldata
        let opaque_len = 32 + 32 + 8 + 1 + deposit.data.len();
        let opaque_padded = opaque_len.div_ceil(32) * 32;
        let total_len = 64 + opaque_padded; // offset(32) + length(32) + padded opaqueData

        let mut log_data = vec![0u8; total_len];
        log_data[24..32].copy_from_slice(&32u64.to_be_bytes()); // offset
        log_data[56..64].copy_from_slice(&(opaque_len as u64).to_be_bytes()); // length

        let base = 64;
        log_data[base + 16..base + 32].copy_from_slice(&deposit.mint.to_be_bytes());
        log_data[base + 32..base + 64].copy_from_slice(&deposit.value.to_be_bytes::<32>());
        log_data[base + 64..base + 72].copy_from_slice(&deposit.gas_limit.to_be_bytes());
        log_data[base + 72] = 0; // isCreation: false
        log_data[base + 73..base + 73 + deposit.data.len()].copy_from_slice(&deposit.data);

        let mut from_topic = [0u8; 32];
        from_topic[12..32].copy_from_slice(deposit.from.as_slice());
        let mut to_topic = [0u8; 32];
        to_topic[12..32].copy_from_slice(deposit.to.as_slice());

        self.enqueue_log_with_status(
            Log {
                address: deposit.deposit_contract,
                data: LogData::new_unchecked(
                    vec![
                        Deposits::EVENT_ABI_HASH,
                        B256::from(from_topic),
                        B256::from(to_topic),
                        Deposits::EVENT_VERSION_0,
                    ],
                    log_data.into(),
                ),
            },
            success,
        );
    }

    /// Return the most recently mined block.
    pub fn latest(&self) -> &L1Block {
        // Safety: `blocks` always contains at least the genesis block.
        self.blocks.last().expect("chain is never empty")
    }

    /// Return the block number of the latest head.
    pub fn latest_number(&self) -> u64 {
        self.latest().number()
    }

    /// Return the safe head block.
    pub fn safe_head(&self) -> &L1Block {
        self.blocks.get(self.safe_number as usize).expect("safe block must exist in chain")
    }

    /// Return the finalized head block.
    pub fn finalized_head(&self) -> &L1Block {
        self.blocks
            .get(self.finalized_number as usize)
            .expect("finalized block must exist in chain")
    }

    /// Return the current safe block number.
    pub const fn safe_number(&self) -> u64 {
        self.safe_number
    }

    /// Return the current finalized block number.
    pub const fn finalized_number(&self) -> u64 {
        self.finalized_number
    }

    /// Advance the safe pointer to the next block (capped at the latest head).
    pub fn act_l1_safe_next(&mut self) {
        self.safe_number = (self.safe_number + 1).min(self.latest_number());
    }

    /// Advance the finalized pointer to the next block (capped at `safe_number`).
    pub fn act_l1_finalize_next(&mut self) {
        self.finalized_number = (self.finalized_number + 1).min(self.safe_number);
    }

    /// Set the safe pointer to `number`.
    ///
    /// # Panics
    ///
    /// Panics if `number > latest_number()`.
    pub fn act_l1_safe(&mut self, number: u64) {
        assert!(
            number <= self.latest_number(),
            "safe number {number} exceeds latest {}",
            self.latest_number()
        );
        self.safe_number = number;
    }

    /// Set the finalized pointer to `number`.
    ///
    /// # Panics
    ///
    /// Panics if `number > safe_number`.
    pub fn act_l1_finalize(&mut self, number: u64) {
        assert!(
            number <= self.safe_number,
            "finalized number {number} exceeds safe {}",
            self.safe_number
        );
        self.finalized_number = number;
    }

    /// Return the block at `number`, or `None` if it has not been mined yet.
    pub fn block_by_number(&self, number: u64) -> Option<&L1Block> {
        self.blocks.get(number as usize)
    }

    /// Return a slice over the entire chain.
    pub fn chain(&self) -> &[L1Block] {
        &self.blocks
    }

    /// Reorg the canonical chain back to `number`, discarding all later blocks.
    ///
    /// Returns the discarded blocks in order (lowest number first) so tests
    /// can inspect which batcher transactions were reorged out. The pending
    /// transaction queue is left untouched — the batcher can choose to
    /// resubmit or discard pending frames as appropriate for the scenario.
    ///
    /// After a reorg, `mine_block` builds on top of block `number`. Because
    /// the `fork_id` is incremented, new blocks will have different hashes
    /// than the original blocks at the same heights.
    ///
    /// Safe and finalized pointers are clamped to `number` if they were
    /// pointing at discarded blocks.
    ///
    /// # Errors
    ///
    /// Returns [`ReorgError::BeyondTip`] if `number` is greater than the
    /// current chain tip. Reorging to block 0 is valid — it keeps only the
    /// immutable genesis block and discards all subsequent blocks.
    pub fn reorg_to(&mut self, number: u64) -> Result<Vec<L1Block>, ReorgError> {
        let tip = self.latest_number();
        if number > tip {
            return Err(ReorgError::BeyondTip { requested: number, tip });
        }

        self.fork_id += 1;
        // Clamp safe/finalized to reorg target so they never point past the tip.
        self.safe_number = self.safe_number.min(number);
        self.finalized_number = self.finalized_number.min(self.safe_number);
        let discarded: Vec<L1Block> = self.blocks.drain((number as usize + 1)..).collect();

        info!(reorg_to = number, fork_id = self.fork_id, discarded = discarded.len(), "L1 reorg");

        Ok(discarded)
    }

    /// Return all pending signed transactions waiting to be mined.
    pub fn pending_transactions(&self) -> impl ExactSizeIterator<Item = &TxEnvelope> {
        self.pending_transactions.iter().map(|tx| &tx.envelope)
    }

    /// Alias for [`latest`] — returns the current chain tip.
    ///
    /// [`latest`]: L1Miner::latest
    pub fn tip(&self) -> &L1Block {
        self.latest()
    }

    /// Return the current chain tip as a [`BlockInfo`].
    ///
    /// Shorthand for `block_info_from(miner.tip())`.
    pub fn tip_info(&self) -> BlockInfo {
        block_info_from(self.tip())
    }

    /// Return the block at `number` as a [`BlockInfo`].
    ///
    /// Shorthand for `block_info_from(miner.block_by_number(number).expect(...))`.
    ///
    /// # Panics
    ///
    /// Panics if `number` is not in the chain.
    pub fn block_info_at(&self, number: u64) -> BlockInfo {
        block_info_from(
            self.block_by_number(number)
                .unwrap_or_else(|| panic!("L1 block {number} not in chain")),
        )
    }

    /// Enqueue a signed L1 transaction for inclusion in the next mined block.
    ///
    /// Production-mode DA tests use this path so derivation receives the same
    /// `TxEnvelope` shape it reads from an RPC-backed provider.
    pub fn submit_transaction(&mut self, tx: TxEnvelope) {
        self.submit_transaction_with_logs(tx, Vec::new());
    }

    /// Enqueue a signed L1 transaction with logs for its eventual receipt.
    pub fn submit_transaction_with_logs(&mut self, tx: TxEnvelope, logs: Vec<Log>) {
        self.submit_transaction_with_logs_and_status(tx, logs, true);
    }

    /// Enqueue a signed L1 transaction with logs and an explicit receipt status.
    pub fn submit_transaction_with_logs_and_status(
        &mut self,
        tx: TxEnvelope,
        logs: Vec<Log>,
        success: bool,
    ) {
        self.pending_transactions.push(L1PendingTransaction::new_with_status(tx, logs, success));
    }

    /// Build and enqueue a signed calldata transaction for inclusion in the next mined block.
    pub fn submit_calldata_transaction(
        &mut self,
        signer: &PrivateKeySigner,
        chain_id: u64,
        nonce: u64,
        to: Address,
        input: Bytes,
    ) -> Result<TxEnvelope, alloy_signer::Error> {
        let tx = L1TxBuilder::signed_calldata(signer, chain_id, nonce, to, input)?;
        self.submit_transaction(tx.clone());
        Ok(tx)
    }

    /// Mine the next L1 block, consuming all pending signed transactions.
    ///
    /// The new block's `parent_hash` is set to the previous block's hash,
    /// and its timestamp advances by `block_time` seconds. All currently
    /// pending transactions, logs, and blob sidecars are drained into the
    /// block body.
    pub fn mine_block(&mut self) -> &L1Block {
        let parent = self.latest();
        let parent_hash = parent.hash();
        let number = parent.header.number + 1;
        let timestamp = parent.header.timestamp + self.config.block_time;

        let pending_transactions = core::mem::take(&mut self.pending_transactions);
        let transactions =
            pending_transactions.iter().map(|pending| pending.envelope.clone()).collect::<Vec<_>>();
        let (transaction_logs, transaction_statuses): (Vec<_>, Vec<_>) =
            pending_transactions.into_iter().map(|pending| (pending.logs, pending.success)).unzip();
        let blob_sidecars = core::mem::take(&mut self.pending_blobs);
        let logs_bloom: Bloom = transaction_logs.iter().flat_map(|logs| logs.iter()).collect();

        let header = Header {
            number,
            timestamp,
            parent_hash,
            logs_bloom,
            // Approximate a realistic base fee so tests that inspect fee
            // fields don't see a zero value.
            base_fee_per_gas: Some(1_000_000_000),
            // Stamp the current fork_id so that blocks produced on
            // different forks always have distinct hashes, even when
            // their number, timestamp, and parent hash are identical.
            extra_data: Bytes::copy_from_slice(&self.fork_id.to_be_bytes()),
            ..Default::default()
        };
        let block_hash = header.hash_slow();
        let receipts = Self::build_receipts_with_status(&transaction_logs, &transaction_statuses);
        let transaction_receipts = Self::build_transaction_receipts_with_status(
            &transactions,
            &transaction_logs,
            &transaction_statuses,
            block_hash,
            number,
            timestamp,
        );

        let block = L1Block { header, transactions, receipts, transaction_receipts, blob_sidecars };

        info!(
            block_number = number,
            timestamp = timestamp,
            transactions = block.transactions.len(),
            "mined L1 block"
        );

        self.blocks.push(block);
        self.blocks.last().expect("just pushed")
    }

    /// Build consensus receipts for the in-memory chain provider.
    pub fn build_receipts(transaction_logs: &[Vec<Log>]) -> Vec<Receipt> {
        let statuses = vec![true; transaction_logs.len()];
        Self::build_receipts_with_status(transaction_logs, &statuses)
    }

    /// Build consensus receipts for the in-memory chain provider with explicit statuses.
    pub fn build_receipts_with_status(
        transaction_logs: &[Vec<Log>],
        transaction_statuses: &[bool],
    ) -> Vec<Receipt> {
        debug_assert_eq!(
            transaction_logs.len(),
            transaction_statuses.len(),
            "transaction_logs and transaction_statuses must have the same length"
        );
        transaction_logs
            .iter()
            .enumerate()
            .map(|(index, logs)| Receipt {
                status: transaction_statuses[index].into(),
                cumulative_gas_used: 21_000 * (index as u64 + 1),
                logs: logs.clone(),
            })
            .collect()
    }

    /// Build production-shaped RPC receipts for a mined block.
    pub fn build_transaction_receipts(
        transactions: &[TxEnvelope],
        transaction_logs: &[Vec<Log>],
        block_hash: B256,
        block_number: u64,
        timestamp: u64,
    ) -> Vec<TransactionReceipt> {
        let statuses = vec![true; transactions.len()];
        Self::build_transaction_receipts_with_status(
            transactions,
            transaction_logs,
            &statuses,
            block_hash,
            block_number,
            timestamp,
        )
    }

    /// Build production-shaped RPC receipts for a mined block with explicit statuses.
    pub fn build_transaction_receipts_with_status(
        transactions: &[TxEnvelope],
        transaction_logs: &[Vec<Log>],
        transaction_statuses: &[bool],
        block_hash: B256,
        block_number: u64,
        timestamp: u64,
    ) -> Vec<TransactionReceipt> {
        debug_assert_eq!(
            transactions.len(),
            transaction_logs.len(),
            "transactions and transaction_logs must have the same length"
        );
        debug_assert_eq!(
            transaction_logs.len(),
            transaction_statuses.len(),
            "transaction_logs and transaction_statuses must have the same length"
        );
        let mut previous_log_count = 0;
        transactions
            .iter()
            .enumerate()
            .map(|(index, tx)| {
                let gas_used = 21_000;
                let tx_logs = transaction_logs[index].clone();
                let meta = TransactionMeta {
                    tx_hash: *tx.hash(),
                    index: index as u64,
                    block_hash,
                    block_number,
                    base_fee: Some(1_000_000_000),
                    excess_blob_gas: None,
                    timestamp,
                };
                let rpc_logs = RpcLog::collect_for_receipt(previous_log_count, meta, tx_logs);
                previous_log_count += rpc_logs.len();
                let receipt = Receipt::<RpcLog> {
                    status: transaction_statuses[index].into(),
                    cumulative_gas_used: gas_used * (index as u64 + 1),
                    logs: rpc_logs,
                };
                TransactionReceipt {
                    inner: ReceiptEnvelope::Legacy(receipt.with_bloom()),
                    transaction_hash: *tx.hash(),
                    transaction_index: Some(index as u64),
                    block_hash: Some(block_hash),
                    block_number: Some(block_number),
                    gas_used,
                    effective_gas_price: 1_000_000_000,
                    blob_gas_used: tx.blob_gas_used(),
                    blob_gas_price: tx.blob_gas_used().map(|_| 1_000_000_000),
                    from: tx.recover_signer().unwrap_or_default(),
                    to: tx.to(),
                    contract_address: None,
                }
            })
            .collect()
    }
}

impl Default for L1Miner {
    fn default() -> Self {
        Self::new(L1MinerConfig::default())
    }
}

impl Action for L1Miner {
    /// The block number of the newly mined block.
    type Output = u64;
    type Error = core::convert::Infallible;

    fn act(&mut self) -> Result<u64, core::convert::Infallible> {
        Ok(self.mine_block().number())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{Transaction, transaction::SignerRecoverable};
    use alloy_eips::eip4844::Blob;
    use alloy_primitives::{Address, B256, Bloom, Bytes, Log, LogData};
    use alloy_signer_local::PrivateKeySigner;

    use super::{L1Miner, L1TxBuilder, ReorgError};
    use crate::Action;

    struct MinerFixture;

    impl MinerFixture {
        fn miner() -> L1Miner {
            L1Miner::default()
        }

        fn signer() -> PrivateKeySigner {
            PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).expect("valid test signer")
        }

        fn signed_tx(input: Bytes, nonce: u64, to: Address) -> alloy_consensus::TxEnvelope {
            L1TxBuilder::signed_calldata(&Self::signer(), 1, nonce, to, input)
                .expect("test transaction signs")
        }

        fn log(address: Address, topic: B256) -> Log {
            Log { address, data: LogData::new_unchecked(vec![topic], Bytes::from_static(b"event")) }
        }
    }

    #[test]
    fn genesis_block_at_number_zero() {
        let m = MinerFixture::miner();
        assert_eq!(m.latest_number(), 0);
        assert_eq!(m.latest().timestamp(), 0);
    }

    #[test]
    fn mine_increments_number_and_timestamp() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        assert_eq!(m.latest_number(), 1);
        assert_eq!(m.latest().timestamp(), 12);
        m.mine_block();
        assert_eq!(m.latest_number(), 2);
        assert_eq!(m.latest().timestamp(), 24);
    }

    #[test]
    fn blocks_form_valid_parent_hash_chain() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        m.mine_block();
        m.mine_block();
        for i in 1..=3u64 {
            let block = m.block_by_number(i).unwrap();
            let parent = m.block_by_number(i - 1).unwrap();
            assert_eq!(block.header.parent_hash, parent.hash());
        }
    }

    #[test]
    fn safe_and_finalized_head_start_at_genesis() {
        let m = MinerFixture::miner();
        assert_eq!(m.safe_head().number(), 0);
        assert_eq!(m.finalized_head().number(), 0);
        assert_eq!(m.safe_number(), 0);
        assert_eq!(m.finalized_number(), 0);
    }

    #[test]
    fn explicit_safe_next_advances_pointer() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // 1
        m.mine_block(); // 2
        m.act_l1_safe_next();
        assert_eq!(m.safe_number(), 1);
        assert_eq!(m.safe_head().number(), 1);
    }

    #[test]
    fn explicit_finalize_next_advances_pointer() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        m.act_l1_safe_next();
        m.act_l1_finalize_next();
        assert_eq!(m.finalized_number(), 1);
        assert_eq!(m.finalized_head().number(), 1);
    }

    #[test]
    fn act_l1_safe_sets_exactly() {
        let mut m = MinerFixture::miner();
        for _ in 0..5 {
            m.mine_block();
        }
        m.act_l1_safe(3);
        assert_eq!(m.safe_number(), 3);
    }

    #[test]
    fn act_l1_finalize_sets_exactly() {
        let mut m = MinerFixture::miner();
        for _ in 0..5 {
            m.mine_block();
        }
        m.act_l1_safe(4);
        m.act_l1_finalize(2);
        assert_eq!(m.finalized_number(), 2);
    }

    #[test]
    fn safe_clamped_to_latest_on_act_safe_next() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // 1
        m.act_l1_safe_next(); // 1
        m.act_l1_safe_next(); // would be 2, but latest=1
        assert_eq!(m.safe_number(), 1);
    }

    #[test]
    fn finalized_clamped_to_safe_on_act_finalize_next() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        m.act_l1_safe_next(); // safe=1
        m.act_l1_finalize_next(); // finalized=1
        m.act_l1_finalize_next(); // clamped to safe=1
        assert_eq!(m.finalized_number(), 1);
    }

    #[test]
    fn reorg_clamps_safe_and_finalized() {
        let mut m = MinerFixture::miner();
        for _ in 0..5 {
            m.mine_block();
        }
        m.act_l1_safe(4);
        m.act_l1_finalize(3);

        m.reorg_to(2).unwrap();
        assert_eq!(m.safe_number(), 2);
        assert_eq!(m.finalized_number(), 2);
    }

    #[test]
    fn pending_transactions_included_in_next_block() {
        let mut m = MinerFixture::miner();
        let to = Address::repeat_byte(0x22);
        let input = Bytes::from_static(b"\x00hello");
        let expected_sender = MinerFixture::signer().address();
        m.submit_transaction(MinerFixture::signed_tx(input.clone(), 0, to));
        m.mine_block();
        assert_eq!(m.latest().transactions.len(), 1);
        assert_eq!(m.latest().transactions[0].input(), &input);
        assert_eq!(m.latest().transactions[0].to(), Some(to));
        assert_eq!(m.latest().transactions[0].recover_signer().unwrap(), expected_sender);
        assert_eq!(m.latest().transaction_receipts[0].from, expected_sender);
        assert_eq!(m.latest().transaction_receipts[0].to, Some(to));
    }

    #[test]
    fn pending_transactions_cleared_after_mining() {
        let mut m = MinerFixture::miner();
        m.submit_transaction(MinerFixture::signed_tx(Bytes::new(), 0, Address::ZERO));
        m.mine_block();
        m.mine_block();
        assert!(m.latest().transactions.is_empty());
    }

    #[test]
    fn enqueued_logs_are_mined_as_signed_event_transaction_receipts() {
        let mut m = MinerFixture::miner();
        let first = MinerFixture::log(Address::repeat_byte(0x51), B256::repeat_byte(0x01));
        let second = MinerFixture::log(Address::repeat_byte(0x52), B256::repeat_byte(0x02));

        m.enqueue_log(first.clone());
        m.enqueue_log(second.clone());
        assert_eq!(m.pending_transactions().len(), 2);

        m.mine_block();
        let block = m.latest();
        assert_eq!(block.transactions.len(), 2);
        assert_eq!(block.transactions[0].to(), Some(first.address));
        assert_eq!(block.transactions[1].to(), Some(second.address));
        assert_eq!(block.receipts[0].logs, vec![first.clone()]);
        assert_eq!(block.receipts[1].logs, vec![second.clone()]);
        assert_ne!(block.header.logs_bloom, Bloom::ZERO);

        let first_receipt = &block.transaction_receipts[0];
        let second_receipt = &block.transaction_receipts[1];
        assert_ne!(*first_receipt.inner.logs_bloom(), Bloom::ZERO);
        assert_ne!(*second_receipt.inner.logs_bloom(), Bloom::ZERO);
        assert_eq!(first_receipt.logs()[0].inner, first);
        assert_eq!(first_receipt.logs()[0].block_hash, Some(block.hash()));
        assert_eq!(first_receipt.logs()[0].block_number, Some(block.number()));
        assert_eq!(first_receipt.logs()[0].block_timestamp, Some(block.timestamp()));
        assert_eq!(first_receipt.logs()[0].transaction_hash, Some(*block.transactions[0].hash()));
        assert_eq!(first_receipt.logs()[0].transaction_index, Some(0));
        assert_eq!(first_receipt.logs()[0].log_index, Some(0));

        assert_eq!(second_receipt.logs()[0].inner, second);
        assert_eq!(second_receipt.logs()[0].transaction_hash, Some(*block.transactions[1].hash()));
        assert_eq!(second_receipt.logs()[0].transaction_index, Some(1));
        assert_eq!(second_receipt.logs()[0].log_index, Some(1));
    }

    #[test]
    fn act_returns_block_number() {
        let mut m = MinerFixture::miner();
        assert_eq!(m.act().unwrap(), 1);
        assert_eq!(m.act().unwrap(), 2);
    }

    #[test]
    fn blob_sidecars_drained_into_block() {
        let mut m = MinerFixture::miner();
        let hash = B256::repeat_byte(0xAA);
        let blob = Box::new(Blob::default());
        m.enqueue_blob(hash, blob);
        m.mine_block();
        assert_eq!(m.latest().blob_sidecars.len(), 1);
        assert_eq!(m.latest().blob_sidecars[0].0, hash);
        // Next block has no blobs.
        m.mine_block();
        assert!(m.latest().blob_sidecars.is_empty());
    }

    // ── reorg tests ──────────────────────────────────────────────────────────

    #[test]
    fn reorg_truncates_chain() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // 1
        m.mine_block(); // 2
        m.mine_block(); // 3

        let discarded = m.reorg_to(1).unwrap();
        assert_eq!(discarded.len(), 2);
        assert_eq!(discarded[0].number(), 2);
        assert_eq!(discarded[1].number(), 3);
        assert_eq!(m.latest_number(), 1);
    }

    #[test]
    fn reorg_returns_transactions_from_discarded_blocks() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // 1 — empty

        let input = Bytes::from_static(b"\x00batch");
        m.submit_transaction(MinerFixture::signed_tx(input.clone(), 0, Address::ZERO));
        m.mine_block(); // 2 — contains the batch tx

        m.mine_block(); // 3 — empty

        let discarded = m.reorg_to(1).unwrap();
        // Block 2 contained the batch, block 3 was empty.
        assert_eq!(discarded[0].number(), 2);
        assert_eq!(discarded[0].transactions.len(), 1);
        assert_eq!(discarded[0].transactions[0].input(), &input);
        assert_eq!(discarded[1].number(), 3);
        assert!(discarded[1].transactions.is_empty());
    }

    #[test]
    fn reorg_to_tip_is_no_op() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        m.mine_block();

        let discarded = m.reorg_to(2).unwrap();
        assert!(discarded.is_empty());
        assert_eq!(m.latest_number(), 2);
    }

    #[test]
    fn post_reorg_blocks_have_distinct_hashes() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // block 1 on fork 0
        let original_hash_1 = m.block_by_number(1).unwrap().hash();

        // Reorg all the way back to genesis (now allowed); mine a new block 1 on fork 1.
        m.reorg_to(0).unwrap(); // fork_id → 1; chain back to [genesis]
        m.mine_block(); // new block 1 on fork 1

        let new_hash_1 = m.block_by_number(1).unwrap().hash();
        assert_ne!(
            new_hash_1, original_hash_1,
            "block 1 on fork 1 must have a different hash than block 1 on fork 0",
        );
    }

    #[test]
    fn mine_after_reorg_builds_valid_parent_chain() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        m.mine_block();
        m.mine_block();

        m.reorg_to(1).unwrap();
        m.mine_block(); // new 2
        m.mine_block(); // new 3

        for i in 1..=3u64 {
            let block = m.block_by_number(i).unwrap();
            let parent = m.block_by_number(i - 1).unwrap();
            assert_eq!(block.header.parent_hash, parent.hash(), "parent chain broken at {i}");
        }
    }

    #[test]
    fn reorg_beyond_tip_returns_error() {
        let mut m = MinerFixture::miner();
        m.mine_block();
        assert!(matches!(m.reorg_to(5), Err(ReorgError::BeyondTip { requested: 5, tip: 1 })));
    }

    #[test]
    fn reorg_to_genesis_discards_all_non_genesis_blocks() {
        let mut m = MinerFixture::miner();
        m.mine_block(); // block 1
        m.mine_block(); // block 2
        let discarded = m.reorg_to(0).expect("reorg to genesis should succeed");
        assert_eq!(discarded.len(), 2, "blocks 1 and 2 should be discarded");
        assert_eq!(m.latest_number(), 0, "only genesis remains");
    }
}
