//! Real mainnet contracts and transactions moved into a synthetic generator state: every
//! account and storage slot a real block read is copied from its witness into the genesis of a
//! generator case, and the block's real transactions, re-signed copies of its calls from
//! generator keys (so their gas, value, and calldata can change), and deposits impersonating its
//! senders run there next to the generator's own traffic.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    sync::{Arc, Mutex, OnceLock},
};

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, TxKind, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{EMPTY_ROOT_HASH, Nibbles, TrieAccount, nodes::TrieNode};
use serde::{Deserialize, Serialize};

use super::{BlockPool, CallEdit, RealCase, TX_GAS_LIMIT_CAP};
use crate::{
    Account, Era, Genesis, L1Origin, Native, PlannedTx, Request, Rng, Scenarios, Schedule,
    SysConfig, Template, TemplateInput, TrafficMix, WorldState, era_blocks,
};

/// Accounts and storage recovered from a block's witness, keyed by address.
pub type RealState = BTreeMap<Address, (Account, Option<Bytes>)>;

/// One transaction of a [`Transplant`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransplantItem {
    /// Real transaction `index` of the source block, verbatim (its sender's real account is in
    /// the state, so it stays valid).
    Real {
        /// Index in the source block.
        index: usize,
    },
    /// The call of real transaction `index`, re-signed from a generator key, with edits (sender
    /// edits are ignored).
    Signed {
        /// Index in the source block.
        index: usize,
        /// Edits.
        edits: Vec<CallEdit>,
    },
    /// The call of real transaction `index` as a deposit from its real sender, with edits.
    Deposit {
        /// Index in the source block.
        index: usize,
        /// Edits.
        edits: Vec<CallEdit>,
    },
}

/// A generator case whose genesis also holds the state a real block read.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transplant {
    /// Source mainnet block.
    pub block: u64,
    /// Era the synthetic block runs in.
    pub era: Era,
    /// Seed of the synthetic environment (block number, system config, keys' balances, and
    /// generated programs).
    pub seed: u64,
    /// Most random generator transactions to add.
    pub synthetic: u8,
    /// Transactions from the source block.
    pub items: Vec<TransplantItem>,
}

impl Transplant {
    /// Stable id.
    pub fn id(&self) -> String {
        let hash = keccak256(serde_json::to_vec(self).expect("cases serialize"));
        format!("tp-{}-{:?}-{}", self.block, self.era, &format!("{hash:x}")[..10]).to_lowercase()
    }

    /// Path of the witness key preimages saved next to a cached block.
    pub fn keys_path(pool: &BlockPool, block: u64) -> std::path::PathBuf {
        pool.dir.join(format!("{block}.keys.json"))
    }

    /// The state a block read, from its witness and key preimages, cached per block.
    pub fn real_state(pool: &BlockPool, block: u64) -> eyre::Result<Arc<RealState>> {
        static CACHE: OnceLock<Mutex<HashMap<u64, Arc<RealState>>>> = OnceLock::new();
        let cache = CACHE.get_or_init(Default::default);
        if let Some(state) = cache.lock().unwrap().get(&block) {
            return Ok(Arc::clone(state));
        }
        let request = pool.get(block)?;
        let keys: Vec<Bytes> = serde_json::from_slice(&fs::read(Self::keys_path(pool, block))?)?;
        let state = Arc::new(Self::extract(&request, &keys)?);
        let mut cache = cache.lock().unwrap();
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(block, Arc::clone(&state));
        Ok(state)
    }

    /// Recovers every account named by a 20-byte key and every slot named by a 32-byte key
    /// that the witness of `request` proves, with the account's code when the witness has it.
    pub fn extract(request: &Request, keys: &[Bytes]) -> eyre::Result<RealState> {
        let nodes: HashMap<B256, &Bytes> =
            request.witness.state.iter().map(|node| (keccak256(node), node)).collect();
        let codes: HashMap<B256, &Bytes> =
            request.witness.codes.iter().map(|code| (keccak256(code), code)).collect();
        let parent: alloy_consensus::Header = alloy_rlp::decode_exact(&request.ancestors[0])?;
        let slots: Vec<B256> =
            keys.iter().filter(|k| k.len() == 32).map(|k| B256::from_slice(k)).collect();
        let mut out = RealState::new();
        for key in keys.iter().filter(|k| k.len() == 20) {
            let address = Address::from_slice(key);
            let Some(Some(leaf)) = Self::lookup(&nodes, parent.state_root, keccak256(address))
            else {
                continue;
            };
            let Ok(trie) = alloy_rlp::decode_exact::<TrieAccount>(&leaf) else { continue };
            let mut account = Account {
                nonce: trie.nonce,
                balance: trie.balance,
                code_hash: trie.code_hash,
                storage: BTreeMap::new(),
            };
            if trie.storage_root != EMPTY_ROOT_HASH {
                for slot in &slots {
                    if let Some(Some(value)) =
                        Self::lookup(&nodes, trie.storage_root, keccak256(slot))
                        && let Ok(value) = alloy_rlp::decode_exact::<U256>(&value)
                        && !value.is_zero()
                    {
                        account.storage.insert(*slot, value);
                    }
                }
            }
            let code = (trie.code_hash != KECCAK256_EMPTY)
                .then(|| codes.get(&trie.code_hash).map(|c| (*c).clone()))
                .flatten();
            out.insert(address, (account, code));
        }
        Ok(out)
    }

    /// The value at `key` in the trie rooted at `root`: `Some(None)` when the trie proves it
    /// absent, `None` when a node on the path is missing.
    pub fn lookup(nodes: &HashMap<B256, &Bytes>, root: B256, key: B256) -> Option<Option<Vec<u8>>> {
        let path = Nibbles::unpack(key);
        let mut pos = 0;
        let mut node: Vec<u8> = nodes.get(&root)?.to_vec();
        loop {
            let next = match TrieNode::decode(&mut node.as_slice()).ok()? {
                TrieNode::EmptyRoot => return Some(None),
                TrieNode::Leaf(leaf) => {
                    return Some((path.slice(pos..) == leaf.key).then_some(leaf.value));
                }
                TrieNode::Extension(ext) => {
                    if !path.slice(pos..).starts_with(&ext.key) {
                        return Some(None);
                    }
                    pos += ext.key.len();
                    ext.child
                }
                TrieNode::Branch(branch) => {
                    let nibble = path.get(pos)?;
                    pos += 1;
                    match branch.as_ref().children().find(|(n, _)| *n == nibble)?.1 {
                        Some(child) => child.clone(),
                        None => return Some(None),
                    }
                }
            };
            node = match next.as_hash() {
                Some(hash) => nodes.get(&hash)?.to_vec(),
                None => next.to_vec(),
            };
        }
    }

    /// Whether a real account may be copied over the synthetic genesis: not a predeploy, a
    /// predeploy implementation, a precompile, or a system address, and not already part of the generator's state.
    pub fn transplantable(address: &Address, state: &WorldState) -> bool {
        let bytes = address.as_slice();
        let predeploy = bytes[..18] == [0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let low = bytes[..17].iter().all(|b| *b == 0);
        let system = bytes[..18].iter().all(|b| *b == 0xde || *b == 0xad);
        // Predeploy implementations (`0xc0d3...`): the generator's predeploy proxies may not
        // link them yet, and a copy would leave an upgrade target pre-installed.
        let implementation = bytes[..18].chunks(2).all(|c| c == [0xc0, 0xd3]);
        !predeploy && !low && !system && !implementation && !state.accounts.contains_key(address)
    }

    /// A fresh case from a block with saved key preimages.
    pub fn fresh(pool: &BlockPool, rng: &mut Rng) -> Option<Self> {
        let block = (0..64)
            .map(|_| *rng.pick(&pool.numbers))
            .find(|b| Self::keys_path(pool, *b).exists())?;
        let request = pool.get(block).ok()?;
        let start = BlockPool::system_prefix(&request);
        let len = request.transactions.len();
        let own = Era::at(request.header.timestamp);
        let era = if rng.chance(60) { own } else { *rng.pick(&Era::ALL[3..]) };
        let mut items = Vec::new();
        for _ in 0..1 + rng.below(12) {
            if start >= len {
                break;
            }
            let index = start + rng.index(len - start);
            items.push(Self::random_item(rng, index));
        }
        Some(Self { block, era, seed: rng.next_u64(), synthetic: rng.below(6) as u8, items })
    }

    fn random_item(rng: &mut Rng, index: usize) -> TransplantItem {
        match rng.below(3) {
            0 => TransplantItem::Real { index },
            1 => TransplantItem::Signed { index, edits: RealCase::random_edits(rng, 0, 2) },
            _ => TransplantItem::Deposit { index, edits: RealCase::random_edits(rng, 0, 2) },
        }
    }

    /// A mutated copy.
    pub fn mutate(&self, pool: &BlockPool, rng: &mut Rng) -> Self {
        let mut case = self.clone();
        let Ok(request) = pool.get(self.block) else { return case };
        let start = BlockPool::system_prefix(&request);
        let len = request.transactions.len();
        for _ in 0..1 + rng.below(3) {
            let n = case.items.len();
            match rng.below(8) {
                0 if n > 0 => {
                    case.items.remove(rng.index(n));
                }
                1 if n > 1 => {
                    let (a, b) = (rng.index(n), rng.index(n));
                    case.items.swap(a, b);
                }
                2 if n > 0 => {
                    let i = rng.index(n);
                    let item = case.items[i].clone();
                    case.items.insert(i, item);
                }
                3 | 4 if start < len => {
                    let index = start + rng.index(len - start);
                    let item = Self::random_item(rng, index);
                    case.items.insert(rng.index(n + 1), item);
                }
                5 if n > 0 => {
                    let i = rng.index(n);
                    let edit = RealCase::random_edit(rng, &[]);
                    match &mut case.items[i] {
                        TransplantItem::Signed { edits, .. }
                        | TransplantItem::Deposit { edits, .. } => edits.push(edit),
                        TransplantItem::Real { index } => {
                            case.items[i] =
                                TransplantItem::Signed { index: *index, edits: vec![edit] }
                        }
                    }
                }
                6 => case.era = *rng.pick(&Era::ALL[3..]),
                _ => {
                    case.seed = rng.next_u64();
                    case.synthetic = rng.below(6) as u8;
                }
            }
        }
        case.items.truncate(64);
        case
    }

    /// The build-mode request: the generator's single-block planning on a genesis that also
    /// holds the real state, with the items added in the planning phases.
    pub fn build(&self, pool: &BlockPool) -> eyre::Result<Request> {
        let real = Self::real_state(pool, self.block)?;
        let mut rng = Rng::new(&[self.seed, self.block]);
        let schedule = Schedule::mainnet();
        let (first, last) = era_blocks(self.era);
        let number = if rng.chance(10) { first } else { rng.range(first + 1, last) };
        let parent_number = number - 1;
        let parent_timestamp = schedule.timestamp_parts(parent_number).0;
        let state_era = Era::at(parent_timestamp);
        let timestamp = schedule.timestamp_parts(number).0;
        let sys = SysConfig::random(&mut rng, timestamp);
        let mut genesis = Genesis::build(&mut rng, state_era, parent_number, &sys, 256);
        let upgrade = schedule.upgrade_at(timestamp);
        for (address, (account, code)) in real.iter() {
            if !Self::transplantable(address, &genesis.state) {
                continue;
            }
            if let Some(code) = code {
                // Code starting with 0xEF (EIP-7702 delegations, B-20 token markers) cannot
                // exist before the upgrade that introduced it (EIP-3541), so it stays out of
                // earlier eras.
                let ef = code.first() == Some(&0xef);
                if ef
                    && (state_era < Era::Isthmus
                        || (code.len() == 1 && !Native::active(state_era.upgrade())))
                {
                    continue;
                }
                genesis.state.register_code(code.clone());
            } else if account.code_hash != KECCAK256_EMPTY {
                // Code the witness does not carry (never executed): leave the account out.
                continue;
            }
            genesis.state.accounts.insert(*address, account.clone());
        }
        let (root, _) = genesis.state.witness();
        genesis.ancestors[0].state_root = root;
        let origin = L1Origin::random(&mut rng, timestamp);
        let activate = Native::active(upgrade) && !Native::active(state_era.upgrade());
        let source = pool.get(self.block)?;
        let calls: Vec<_> = self
            .items
            .iter()
            .map(|item| {
                let index = match item {
                    TransplantItem::Real { index }
                    | TransplantItem::Signed { index, .. }
                    | TransplantItem::Deposit { index, .. } => *index,
                };
                (source.transactions.get(index).cloned(), pool.call(self.block, index))
            })
            .collect();
        let input = TemplateInput {
            id: self.id(),
            state: &genesis.state,
            registry: &genesis.registry,
            sys: &sys,
            origin: &origin,
            ancestors: &genesis.ancestors,
            mix: TrafficMix { min_txs: 0, max_txs: self.synthetic as usize, invalid_percent: 5 },
        };
        let id = self.id();
        let template = Template::build(&mut rng, input, &mut |planner, deposits| {
            if !deposits && activate {
                Scenarios::activate_features(planner);
            }
            for (position, (item, (raw, call))) in self.items.iter().zip(&calls).enumerate() {
                let (Some(raw), Some(call)) = (raw, call) else { continue };
                match item {
                    TransplantItem::Deposit { edits, .. } if deposits => {
                        let source_hash =
                            keccak256([id.as_bytes(), &position.to_be_bytes()].concat());
                        let deposit = RealCase::deposit(call.clone(), edits, source_hash);
                        if deposit.gas_limit <= TX_GAS_LIMIT_CAP {
                            planner.push_deposit(deposit, "tp:deposit".to_owned(), None);
                        }
                    }
                    TransplantItem::Real { .. } if !deposits => {
                        planner.gas_left = planner.gas_left.saturating_sub(call.gas);
                        planner.out.push(PlannedTx {
                            raw: raw.clone(),
                            label: "tp:real".to_owned(),
                            deploy: None,
                        });
                    }
                    TransplantItem::Signed { edits, .. } if !deposits => {
                        let edited = RealCase::deposit(call.clone(), edits, B256::ZERO);
                        if let TxKind::Call(to) = edited.to {
                            planner.call_gas(
                                None,
                                to,
                                edited.value,
                                edited.input,
                                "tp:signed".to_owned(),
                                Some(edited.gas_limit),
                            );
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(template.request)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, Bytes, U256};

    use super::Transplant;
    use crate::{Account, Mode, Request, RequestHeader, WorldState};

    #[test]
    fn extract_recovers_witnessed_accounts_and_slots() {
        // Enough accounts and slots for branch, extension, and inline nodes.
        let mut state = WorldState::default();
        for i in 1..=40u64 {
            let address = Address::with_last_byte(i as u8);
            state.fund(address, U256::from(i * 1000));
            state.accounts.get_mut(&address).unwrap().nonce = i;
            if i % 3 == 0 {
                state.set_code(address, Bytes::from(vec![0x60, i as u8, 0x00]));
                let account = state.accounts.get_mut(&address).unwrap();
                for slot in 0..i {
                    account.storage.insert(B256::from(U256::from(slot)), U256::from(slot + 7));
                }
            }
        }
        let (root, witness) = state.witness();
        let parent = Header { state_root: root, number: 9, ..Default::default() };
        let request = Request {
            id: "t".into(),
            chain: "base-mainnet".into(),
            mode: Mode::Validate,
            ancestors: vec![alloy_rlp::encode(&parent).into()],
            header: RequestHeader { number: 10, ..Default::default() },
            transactions: vec![],
            witness,
            trace: None,
            system_config: None,
        };
        let absent = Address::with_last_byte(0xee);
        let mut keys: Vec<Bytes> = (1..=40u8)
            .map(Address::with_last_byte)
            .chain([absent])
            .map(|a| Bytes::copy_from_slice(a.as_slice()))
            .collect();
        keys.extend(
            (0..45u64).map(|s| Bytes::copy_from_slice(B256::from(U256::from(s)).as_slice())),
        );

        let extracted = Transplant::extract(&request, &keys).unwrap();
        assert!(!extracted.contains_key(&absent));
        assert_eq!(extracted.len(), 40);
        for (address, (account, code)) in &extracted {
            let expected: &Account = state.account(address).unwrap();
            assert_eq!(account, expected, "account {address}");
            assert_eq!(code.as_ref(), state.code(address), "code of {address}");
        }
    }
}
