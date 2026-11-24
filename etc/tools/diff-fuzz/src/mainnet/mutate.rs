//! Build-mode fuzz cases made from real Base mainnet blocks: real transactions dropped,
//! reordered, duplicated, and spliced across neighboring blocks on a real pre-state, and real
//! calls replayed with perturbed gas, value, and calldata through deposits, which are unsigned
//! and can act as any sender.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use alloy_consensus::{Transaction, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use base_common_consensus::{BaseTxEnvelope, SystemAddresses, TxDeposit};
use serde::{Deserialize, Serialize};

use crate::{Mode, Request, Rng, Txs};

/// Highest gas limit a replayed deposit gets: the EIP-7825 per-transaction gas cap, so an edited
/// replay stays within the gas a real call can have.
pub const TX_GAS_LIMIT_CAP: u64 = 1 << 24;

/// A real call decoded from a mainnet transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealCall {
    /// Sender (recovered signer, or the deposit's `from`).
    pub from: Address,
    /// Destination; `Create` for deployments.
    pub to: TxKind,
    /// Value.
    pub value: U256,
    /// Calldata or init code.
    pub input: Bytes,
    /// Gas limit.
    pub gas: u64,
}

impl RealCall {
    /// Decodes a transaction; `None` for undecodable or unrecoverable ones.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let tx = BaseTxEnvelope::decode_2718(&mut &raw[..]).ok()?;
        let from = match &tx {
            BaseTxEnvelope::Deposit(deposit) => deposit.from,
            other => other.recover_signer().ok()?,
        };
        Some(Self {
            from,
            to: tx.kind(),
            value: tx.value(),
            input: tx.input().clone(),
            gas: tx.gas_limit(),
        })
    }
}

/// Validate-mode mainnet requests cached on disk (`<number>.json`), loaded on demand with a
/// small in-memory cache.
#[derive(Debug)]
pub struct BlockPool {
    /// Cache directory of [`crate::MainnetFetcher`].
    pub dir: PathBuf,
    /// Cached block numbers.
    pub numbers: Vec<u64>,
    loaded: Mutex<HashMap<u64, Arc<Request>>>,
    calls: Mutex<HashMap<(u64, usize), Option<RealCall>>>,
}

impl BlockPool {
    /// Most requests kept in memory.
    pub const CACHE: usize = 48;

    /// Lists the cached blocks in `dir`.
    pub fn open(dir: PathBuf) -> eyre::Result<Self> {
        let mut numbers: Vec<u64> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".json")?.parse().ok())
            .collect();
        numbers.sort_unstable();
        Ok(Self {
            dir,
            numbers,
            loaded: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
        })
    }

    /// Whether block `number` is cached.
    pub fn has(&self, number: u64) -> bool {
        self.numbers.binary_search(&number).is_ok()
    }

    /// The cached request of block `number`.
    pub fn get(&self, number: u64) -> eyre::Result<Arc<Request>> {
        if let Some(request) = self.loaded.lock().unwrap().get(&number) {
            return Ok(Arc::clone(request));
        }
        let request: Arc<Request> =
            Arc::new(serde_json::from_slice(&fs::read(self.dir.join(format!("{number}.json")))?)?);
        let mut loaded = self.loaded.lock().unwrap();
        if loaded.len() >= Self::CACHE {
            let evict = *loaded.keys().next().expect("cache is not empty");
            loaded.remove(&evict);
        }
        loaded.insert(number, Arc::clone(&request));
        Ok(request)
    }

    /// The decoded call of transaction `index` of block `number`.
    pub fn call(&self, number: u64, index: usize) -> Option<RealCall> {
        if let Some(call) = self.calls.lock().unwrap().get(&(number, index)) {
            return call.clone();
        }
        let call =
            self.get(number).ok()?.transactions.get(index).and_then(|raw| RealCall::decode(raw));
        self.calls.lock().unwrap().insert((number, index), call.clone());
        call
    }

    /// Number of leading system deposits (the L1 attributes deposit, and the `BaseTime`
    /// deposit from Denim) of a block: deposits from the depositor account.
    pub fn system_prefix(request: &Request) -> usize {
        request
            .transactions
            .iter()
            .take_while(|raw| {
                matches!(
                    BaseTxEnvelope::decode_2718(&mut raw.as_ref()),
                    Ok(BaseTxEnvelope::Deposit(d)) if d.from == SystemAddresses::DEPOSITOR_ACCOUNT
                )
            })
            .count()
            .max(1)
    }

    /// Picks a block that has `donors` cached successors.
    pub fn pick_block(&self, rng: &mut Rng, donors: u64) -> Option<u64> {
        for _ in 0..64 {
            let number = *rng.pick(&self.numbers);
            if (1..=donors).all(|d| self.has(number + d)) {
                return Some(number);
            }
        }
        None
    }
}

/// One transaction of a [`RealCase`] body.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RealItem {
    /// Transaction `index` of block `block`, verbatim.
    Tx {
        /// Source block.
        block: u64,
        /// Index in the source block.
        index: usize,
    },
    /// A deposit replaying the call of transaction `index` of block `block` as its sender,
    /// with edits.
    Deposit {
        /// Source block.
        block: u64,
        /// Index in the source block.
        index: usize,
        /// Edits to the replayed call.
        edits: Vec<CallEdit>,
    },
}

impl RealItem {
    /// Source block of the item.
    pub const fn block(&self) -> u64 {
        match self {
            Self::Tx { block, .. } | Self::Deposit { block, .. } => *block,
        }
    }
}

/// An edit to a replayed call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CallEdit {
    /// Scales the gas limit by `permille / 1000` (capped at [`TX_GAS_LIMIT_CAP`]).
    GasScale(u32),
    /// Sets the gas limit (capped at [`TX_GAS_LIMIT_CAP`]).
    Gas(u64),
    /// Sets the value.
    Value(U256),
    /// Sets the minted amount.
    Mint(u128),
    /// Overwrites calldata word `index` (wrapping over the words after the selector).
    Word {
        /// Word index.
        index: u32,
        /// New word.
        value: B256,
    },
    /// Flips calldata bit `pos` (wrapping).
    Bit(u32),
    /// Truncates calldata to `len` bytes (wrapping).
    Truncate(u32),
    /// Appends bytes to the calldata.
    Append(Bytes),
    /// Acts as another sender.
    From(Address),
    /// Calls another address.
    To(Address),
}

/// Edits to the block header inputs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HeaderTweak {
    /// Scales the base fee by `permille / 1000`.
    BaseFee(u32),
    /// Sets `prevRandao`.
    PrevRandao(B256),
    /// Scales the block gas limit by `permille / 1000`.
    GasLimit(u32),
}

/// A build-mode case on the real pre-state of mainnet block `block`.
///
/// The request keeps the block's ancestors, header inputs, and system deposits; its body is
/// `items`, drawn from the block and its `donors` successors. Splicing a later block's
/// transaction needs the pre-state it reads: the union of the witnesses of `block..=block +
/// donors` has it, because every trie node on a path the later block reads is either unchanged
/// since `block`'s parent state (and in the later witness) or on a path an earlier block in the
/// window changed (and in that block's witness, at its pre-state).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealCase {
    /// Pre-state block.
    pub block: u64,
    /// Successor blocks whose transactions and witnesses the case may use.
    pub donors: u64,
    /// Body transactions after the system deposits.
    pub items: Vec<RealItem>,
    /// Header edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub header: Vec<HeaderTweak>,
}

impl RealCase {
    /// Stable id of the case.
    pub fn id(&self) -> String {
        let hash = keccak256(serde_json::to_vec(self).expect("cases serialize"));
        format!("real-{}-{}", self.block, &format!("{hash:x}")[..10])
    }

    /// The unmodified block as a build-mode case.
    pub fn identity(pool: &BlockPool, block: u64) -> eyre::Result<Self> {
        let request = pool.get(block)?;
        let start = BlockPool::system_prefix(&request);
        let items = (start..request.transactions.len())
            .map(|index| RealItem::Tx { block, index })
            .collect();
        Ok(Self { block, donors: 0, items, header: Vec::new() })
    }

    /// The build-mode request of the case.
    pub fn build(&self, pool: &BlockPool) -> eyre::Result<Request> {
        let base = pool.get(self.block)?;
        let mut request = Request::clone(&base);
        request.id = self.id();
        request.mode = Mode::Build;
        request.header.results = None;
        let prefix = BlockPool::system_prefix(&base);
        request.transactions.truncate(prefix);
        let mut state: HashSet<B256> = request.witness.state.iter().map(keccak256).collect();
        let mut codes: HashSet<B256> = request.witness.codes.iter().map(keccak256).collect();
        for donor in 1..=self.donors {
            let donor = pool.get(self.block + donor)?;
            for node in &donor.witness.state {
                if state.insert(keccak256(node)) {
                    request.witness.state.push(node.clone());
                }
            }
            for code in &donor.witness.codes {
                if codes.insert(keccak256(code)) {
                    request.witness.codes.push(code.clone());
                }
            }
        }
        for (position, item) in self.items.iter().enumerate() {
            eyre::ensure!(
                (self.block..=self.block + self.donors).contains(&item.block()),
                "item from block {} is outside the case window",
                item.block()
            );
            match item {
                RealItem::Tx { block, index } => {
                    let source = pool.get(*block)?;
                    let raw = source
                        .transactions
                        .get(*index)
                        .ok_or_else(|| eyre::eyre!("block {block} has no tx {index}"))?;
                    request.transactions.push(raw.clone());
                }
                RealItem::Deposit { block, index, edits } => {
                    let call = pool.call(*block, *index).ok_or_else(|| {
                        eyre::eyre!("tx {index} of block {block} does not decode")
                    })?;
                    request.transactions.push(Txs::deposit(Self::deposit(
                        call,
                        edits,
                        keccak256([self.id().as_bytes(), &position.to_be_bytes()].concat()),
                    )));
                }
            }
        }
        for tweak in &self.header {
            match tweak {
                HeaderTweak::BaseFee(permille) => {
                    request.header.base_fee_per_gas =
                        (request.header.base_fee_per_gas as u128 * *permille as u128 / 1000) as u64;
                }
                HeaderTweak::PrevRandao(value) => request.header.prev_randao = *value,
                HeaderTweak::GasLimit(permille) => {
                    // The system deposits must still fit: a block that skips its L1 attributes
                    // deposit is malformed (and hits an already reported Nethermind gas-pool
                    // difference).
                    let system: u64 = request.transactions[..prefix]
                        .iter()
                        .filter_map(|raw| match BaseTxEnvelope::decode_2718(&mut raw.as_ref()) {
                            Ok(BaseTxEnvelope::Deposit(d)) => Some(d.gas_limit),
                            _ => None,
                        })
                        .sum();
                    request.header.gas_limit =
                        ((request.header.gas_limit as u128 * *permille as u128 / 1000) as u64)
                            .max(system);
                }
            }
        }
        Ok(request)
    }

    /// The deposit replaying `call` with `edits`.
    pub fn deposit(call: RealCall, edits: &[CallEdit], source_hash: B256) -> TxDeposit {
        let mut deposit = TxDeposit {
            source_hash,
            from: call.from,
            to: call.to,
            mint: call.value.saturating_to::<u128>(),
            value: call.value,
            gas_limit: call.gas,
            is_system_transaction: false,
            input: call.input,
        };
        let mut input = deposit.input.to_vec();
        for edit in edits {
            match edit {
                CallEdit::GasScale(permille) => {
                    deposit.gas_limit = (deposit.gas_limit as u128 * *permille as u128 / 1000)
                        .min(u64::MAX as u128) as u64;
                }
                CallEdit::Gas(gas) => deposit.gas_limit = *gas,
                CallEdit::Value(value) => deposit.value = *value,
                CallEdit::Mint(mint) => deposit.mint = *mint,
                CallEdit::Word { index, value } => {
                    let words = input.len().saturating_sub(4) / 32;
                    if words > 0 {
                        let start = 4 + (*index as usize % words) * 32;
                        input[start..start + 32].copy_from_slice(value.as_slice());
                    }
                }
                CallEdit::Bit(pos) => {
                    if !input.is_empty() {
                        let bit = *pos as usize % (input.len() * 8);
                        input[bit / 8] ^= 1 << (bit % 8);
                    }
                }
                CallEdit::Truncate(len) => {
                    let len = *len as usize % (input.len() + 1);
                    input.truncate(len);
                }
                CallEdit::Append(bytes) => input.extend_from_slice(bytes),
                CallEdit::From(from) => deposit.from = *from,
                CallEdit::To(to) => deposit.to = TxKind::Call(*to),
            }
        }
        deposit.input = input.into();
        deposit.gas_limit = deposit.gas_limit.clamp(21_000, TX_GAS_LIMIT_CAP);
        deposit
    }

    /// Addresses the block window's transactions name as senders or recipients, to retarget
    /// replayed calls at.
    pub fn addresses(&self, pool: &BlockPool) -> Vec<Address> {
        let mut out = BTreeSet::new();
        for block in self.block..=self.block + self.donors {
            let Ok(request) = pool.get(block) else { continue };
            for index in BlockPool::system_prefix(&request)..request.transactions.len() {
                if let Some(call) = pool.call(block, index) {
                    out.insert(call.from);
                    if let TxKind::Call(to) = call.to {
                        out.insert(to);
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// A random item from the case's window (never a system deposit).
    pub fn random_item(&self, pool: &BlockPool, rng: &mut Rng) -> Option<RealItem> {
        let block = self.block + rng.below(self.donors + 1);
        let request = pool.get(block).ok()?;
        let start = BlockPool::system_prefix(&request);
        if start >= request.transactions.len() {
            return None;
        }
        let index = start + rng.index(request.transactions.len() - start);
        Some(if rng.chance(55) {
            RealItem::Tx { block, index }
        } else {
            RealItem::Deposit { block, index, edits: Self::random_edits(rng, 1, 2) }
        })
    }

    /// Between `min` and `max` random call edits.
    pub fn random_edits(rng: &mut Rng, min: u64, max: u64) -> Vec<CallEdit> {
        let count = rng.range(min, max);
        (0..count).map(|_| Self::random_edit(rng, &[])).collect()
    }

    /// One random call edit; `addresses` are candidates for sender and recipient changes.
    pub fn random_edit(rng: &mut Rng, addresses: &[Address]) -> CallEdit {
        match rng.below(if addresses.is_empty() { 10 } else { 13 }) {
            0 => CallEdit::GasScale(*rng.pick(&[0, 100, 500, 900, 990, 1010, 1500, 3000])),
            1 => CallEdit::Gas(*rng.pick(&[
                21_000,
                21_001,
                50_000,
                100_000,
                1_000_000,
                TX_GAS_LIMIT_CAP,
            ])),
            2 => CallEdit::Value(match rng.below(4) {
                0 => U256::ZERO,
                1 => U256::from(1),
                2 => U256::from(rng.next_u64()),
                _ => U256::from(10u64.pow(18)),
            }),
            3 => CallEdit::Mint(match rng.below(3) {
                0 => 0,
                1 => 1,
                _ => 10u128.pow(21),
            }),
            4 | 5 => {
                CallEdit::Word { index: rng.below(16) as u32, value: B256::from(rng.edge_u256()) }
            }
            6 | 7 => CallEdit::Bit(rng.next_u64() as u32),
            8 => CallEdit::Truncate(rng.next_u64() as u32),
            9 => CallEdit::Append(rng.bytes_between(1, 64).into()),
            10 | 11 => CallEdit::From(*rng.pick(addresses)),
            _ => CallEdit::To(*rng.pick(addresses)),
        }
    }

    /// A mutated copy; `donor` is another corpus case to splice from.
    pub fn mutate(&self, pool: &BlockPool, rng: &mut Rng, donor: Option<&Self>) -> Self {
        let mut case = self.clone();
        for _ in 0..1 + rng.below(3) {
            let len = case.items.len();
            match rng.below(13) {
                0 if len > 0 => {
                    case.items.remove(rng.index(len));
                }
                1 if len > 1 => {
                    let (a, b) = (rng.index(len), rng.index(len));
                    case.items.swap(a, b);
                }
                2 if len > 0 => {
                    let item = case.items.remove(rng.index(len));
                    case.items.insert(rng.index(len), item);
                }
                3 if len > 0 => {
                    let i = rng.index(len);
                    let item = case.items[i].clone();
                    case.items.insert(rng.index(len + 1).max(i), item);
                }
                4 | 5 => {
                    if case.donors < 3
                        && rng.chance(30)
                        && (1..=case.donors + 1).all(|d| pool.has(case.block + d))
                    {
                        case.donors += 1;
                    }
                    if let Some(item) = case.random_item(pool, rng) {
                        case.items.insert(rng.index(len + 1), item);
                    }
                }
                6 | 7 if len > 0 => {
                    let addresses = if rng.chance(30) { case.addresses(pool) } else { Vec::new() };
                    let i = rng.index(len);
                    let edit = Self::random_edit(rng, &addresses);
                    case.items[i] = match case.items[i].clone() {
                        RealItem::Tx { block, index } => {
                            RealItem::Deposit { block, index, edits: vec![edit] }
                        }
                        RealItem::Deposit { block, index, mut edits } => {
                            if !edits.is_empty() && rng.chance(25) {
                                edits.remove(rng.index(edits.len()));
                            } else {
                                edits.push(edit);
                            }
                            RealItem::Deposit { block, index, edits }
                        }
                    };
                }
                8 if len > 1 => {
                    let a = rng.index(len);
                    let b = (a + 1 + rng.index(len - a)).min(len);
                    case.items[a..b].reverse();
                }
                9 => match rng.below(3) {
                    0 => case.header.push(HeaderTweak::BaseFee(
                        *rng.pick(&[0, 1, 500, 999, 1001, 2000, 100_000]),
                    )),
                    1 => case.header.push(HeaderTweak::PrevRandao(rng.b256())),
                    _ => case.header.push(HeaderTweak::GasLimit(*rng.pick(&[100, 500, 900, 999]))),
                },
                10 if !case.header.is_empty() => {
                    let i = rng.index(case.header.len());
                    case.header.remove(i);
                }
                11 => {
                    if let Some(donor) =
                        donor.filter(|d| d.block == case.block && !d.items.is_empty())
                    {
                        case.donors = case.donors.max(donor.donors);
                        let a = rng.index(donor.items.len());
                        let b = (a + 1 + rng.below(8) as usize).min(donor.items.len());
                        let at = rng.index(len + 1);
                        for (k, item) in donor.items[a..b].iter().enumerate() {
                            case.items.insert(at + k, item.clone());
                        }
                    }
                }
                _ if len > 2 => {
                    // Keep a random window of the body, so mutations of long blocks stay focused.
                    let keep = 1 + rng.index(len.min(24));
                    let start = rng.index(len - keep + 1);
                    case.items = case.items[start..start + keep].to_vec();
                }
                _ => {}
            }
        }
        case.items.truncate(400);
        case
    }
}
