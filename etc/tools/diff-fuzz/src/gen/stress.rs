//! State-stress traffic for chains that real nodes follow: transactions that shape the account
//! and storage tries rather than exercise the EVM.
//!
//! Every transaction calls [`STRESSOR`], a genesis contract that interprets its calldata as a
//! script of 64-byte commands (see [`Stress::runtime`]). Scripts write many slots, often to keys
//! whose hashed paths share long prefixes so that writes and deletions split and collapse branch
//! nodes, reset slots within a transaction or block, rewrite a hot set every block, spray value
//! to fresh accounts, touch empty genesis accounts (EIP-161 removal), create children with
//! `CREATE2` that write storage and self-destruct in the same transaction (a storage wipe) and
//! are recreated later, and deploy code up to the maximum size.

use std::sync::OnceLock;

use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use serde::{Deserialize, Serialize};

use crate::r#gen::{
    asm::{Asm, Op},
    plan::Planner,
    rng::Rng,
    state::WorldState,
};

/// The stress contract placed at genesis (see [`Stress::runtime`]).
pub const STRESSOR: Address = address!("0x00000000000000000000000000000000d1ff5700");

/// Per-block knobs of the stress traffic. Each is a share or a count; zero turns a behavior off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StressPlan {
    /// Stress transactions in the block (at most 12).
    pub txs: u8,
    /// Slot writes per transaction (about).
    pub slots: u16,
    /// Percentage of slot keys taken from the clustered pools (hashed paths with shared
    /// prefixes); the rest are small integers or random keys.
    pub cluster: u8,
    /// Percentage of slot keys taken from the hot set every block rewrites.
    pub hot: u8,
    /// Percentage of writes that delete an existing slot.
    pub delete: u8,
    /// Percentage of writes reset to their previous value later in the same transaction or by
    /// a later transaction of the block.
    pub reset: u8,
    /// Accounts sprayed with value per transaction.
    pub accounts: u16,
    /// Percentage of transactions that create a child, write its storage, and destroy it (or
    /// recreate a destroyed one).
    pub destroy: u8,
    /// Percentage of transactions that touch empty genesis accounts.
    pub touch: u8,
    /// Percentage of transactions that deploy a large code blob.
    pub large: u8,
    /// Ordinary random transactions in the block, besides the stress ones.
    pub random_txs: u8,
}

impl StressPlan {
    /// A random plan, biased toward heavy trie work.
    pub fn random(rng: &mut Rng) -> Self {
        let pct = |rng: &mut Rng, hi: u64| {
            let mid = rng.below(hi + 1) as u8;
            *rng.pick(&[0, mid, hi as u8])
        };
        let slots = rng.range(1, 600) as u16;
        let accounts = rng.range(1, 300) as u16;
        Self {
            txs: rng.range(1, 9) as u8,
            slots: *rng.pick(&[4u16, 16, 64, 200, slots]),
            cluster: pct(rng, 100),
            hot: pct(rng, 60),
            delete: pct(rng, 70),
            reset: pct(rng, 40),
            accounts: *rng.pick(&[0u16, 0, 8, 64, accounts]),
            destroy: pct(rng, 60),
            touch: pct(rng, 30),
            large: pct(rng, 20),
            random_txs: rng.below(8) as u8,
        }
    }
}

/// Command codes of the [`STRESSOR`] script.
#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum Cmd {
    /// `SSTORE(key, arg)`.
    Store = 0,
    /// `CALL(address = key, value = arg)` with no input.
    Call = 1,
    /// `SELFDESTRUCT(key)`.
    Destruct = 2,
    /// `CREATE2(salt = key, value = arg)` of a copy of the stressor.
    Clone = 3,
    /// `CALL(key)` with the next `arg` bytes of the script as its script.
    Nested = 4,
    /// `CREATE2(salt = key)` of `arg & 0xffff` zero bytes of code.
    Blob = 5,
    /// `SSTORE(key, SLOAD(key) + arg)`.
    Add = 6,
    /// `SSTORE(key + j, arg >> 128)` for `j < arg & 0xffff`.
    Range = 7,
    /// `CALL(keccak(key, j), value = arg >> 128)` for `j < arg & 0xffff`.
    Spray = 8,
}

/// Keys whose hashes share prefixes, found once by search.
#[derive(Debug)]
struct Pools {
    /// Groups of slot keys whose `keccak256(slot)` share a 1 to 5 nibble prefix.
    slots: Vec<Vec<B256>>,
    /// Groups of addresses whose `keccak256(address)` share a 1 to 4 nibble prefix.
    addresses: Vec<Vec<Address>>,
}

/// Builds and plans state-stress traffic.
#[derive(Debug)]
pub struct Stress;

impl Stress {
    /// Most code a blob deployment asks for (one past the EIP-170 limit, which fails).
    pub const MAX_BLOB: u64 = 0x6001;
    /// Empty genesis accounts (balance, nonce, and code all zero) for EIP-161 removal; the odd
    /// ones hold storage.
    pub const EMPTY_ACCOUNTS: u64 = 12;
    /// Stress traffic of a block never exceeds this many transactions.
    pub const MAX_TXS: u8 = 12;

    /// The empty genesis account `index`.
    pub fn empty_account(index: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&[0xd1, 0xff, 0x57, 0xee]);
        bytes[12..].copy_from_slice(&index.to_be_bytes());
        Address::from(bytes)
    }

    /// Runtime of [`STRESSOR`]: an interpreter of 64-byte commands `(op << 248 | key, arg)`
    /// (see [`Cmd`]); unknown ops are skipped.
    pub fn runtime() -> Bytes {
        static CODE: OnceLock<Bytes> = OnceLock::new();
        CODE.get_or_init(|| {
            let mut a = Asm::new(true);
            let (top, next, end) = (a.label(), a.label(), a.label());
            let handlers: Vec<usize> = (0..9).map(|_| a.label()).collect();
            let key_mask = (U256::from(1) << 248) - U256::from(1);
            a.push_u64(0); // [i]
            a.place(top);
            a.op(Op::DUP1).op(Op::CALLDATASIZE).op(Op::GT).op(Op::ISZERO).jumpi(end);
            a.op(Op::DUP1).op(Op::CALLDATALOAD); // [i, w0]
            a.op(Op::DUP2).push_u64(32).op(Op::ADD).op(Op::CALLDATALOAD); // [i, w0, w1]
            a.op(Op::DUP2).push_u64(0).op(Op::BYTE); // [i, w0, w1, op]
            a.op(Op::SWAP2).push(key_mask).op(Op::AND).op(Op::SWAP2); // [i, key, w1, op]
            for (code, label) in handlers.iter().enumerate() {
                a.op(Op::DUP1).push_u64(code as u64).op(Op::EQ).jumpi(*label);
            }
            a.jump(next);

            // Store: SSTORE(key, w1).
            a.place(handlers[Cmd::Store as usize]);
            a.op(Op::DUP2).op(Op::DUP4).op(Op::SSTORE).jump(next);

            // Call: CALL(gas, key, w1, 0, 0, 0, 0).
            a.place(handlers[Cmd::Call as usize]);
            a.push_u64(0).push_u64(0).push_u64(0).push_u64(0).op(Op::DUP6).op(Op::DUP8);
            a.op(Op::GAS).op(Op::CALL).op(Op::POP).jump(next);

            // Destruct: SELFDESTRUCT(key).
            a.place(handlers[Cmd::Destruct as usize]);
            a.op(Op::DUP3).op(Op::SELFDESTRUCT);

            // Clone: CREATE2(w1, 0, len, key) of the copying init code.
            a.place(handlers[Cmd::Clone as usize]);
            let init = Self::clone_init();
            a.mstore_bytes(0, &init);
            a.op(Op::DUP3).push_u64(init.len() as u64).push_u64(0).op(Op::DUP5);
            a.op(Op::CREATE2).op(Op::POP).jump(next);

            // Nested: CALLDATACOPY(0, i + 64, w1); CALL(gas, key, 0, 0, w1, 0, 0); i += 64 + w1.
            a.place(handlers[Cmd::Nested as usize]);
            a.op(Op::DUP2).op(Op::DUP5).push_u64(64).op(Op::ADD).push_u64(0).op(Op::CALLDATACOPY);
            a.push_u64(0).push_u64(0).op(Op::DUP4).push_u64(0).push_u64(0).op(Op::DUP8);
            a.op(Op::GAS).op(Op::CALL).op(Op::POP);
            a.op(Op::POP).op(Op::SWAP1).op(Op::POP).op(Op::ADD).push_u64(64).op(Op::ADD).jump(top);

            // Blob: CREATE2(0, 0, 5, key) of `PUSH2 n PUSH0 RETURN`.
            a.place(handlers[Cmd::Blob as usize]);
            let template = (U256::from(Op::PUSH2) << 248)
                | (U256::from(Op::PUSH0) << 224)
                | (U256::from(Op::RETURN) << 216);
            a.op(Op::DUP2).push_u64(0xffff).op(Op::AND).push_u64(232).op(Op::SHL);
            a.push(template).op(Op::OR).push_u64(0).op(Op::MSTORE);
            a.op(Op::DUP3).push_u64(5).push_u64(0).push_u64(0).op(Op::CREATE2).op(Op::POP);
            a.jump(next);

            // Add: SSTORE(key, SLOAD(key) + w1).
            a.place(handlers[Cmd::Add as usize]);
            a.op(Op::DUP3).op(Op::SLOAD).op(Op::DUP3).op(Op::ADD).op(Op::DUP4).op(Op::SSTORE);
            a.jump(next);

            // Range and Spray loop j over w1 & 0xffff.
            for cmd in [Cmd::Range, Cmd::Spray] {
                let (head, done) = (a.label(), a.label());
                a.place(handlers[cmd as usize]);
                a.push_u64(0); // [i, key, w1, op, j]
                a.place(head);
                a.op(Op::DUP1).op(Op::DUP4).push_u64(0xffff).op(Op::AND).op(Op::GT).op(Op::ISZERO);
                a.jumpi(done);
                match cmd {
                    Cmd::Range => {
                        a.op(Op::DUP3).push_u64(128).op(Op::SHR); // [.., j, value]
                        a.op(Op::DUP2).op(Op::DUP6).op(Op::ADD).op(Op::SSTORE);
                    }
                    _ => {
                        a.op(Op::DUP4).push_u64(0).op(Op::MSTORE);
                        a.op(Op::DUP1).push_u64(32).op(Op::MSTORE);
                        a.push_u64(0).push_u64(0).push_u64(0).push_u64(0);
                        a.op(Op::DUP7).push_u64(128).op(Op::SHR);
                        a.push_u64(64).push_u64(0).op(Op::KECCAK256);
                        a.op(Op::GAS).op(Op::CALL).op(Op::POP);
                    }
                }
                a.push_u64(1).op(Op::ADD).jump(head);
                a.place(done);
                a.op(Op::POP).jump(next);
            }

            a.place(next);
            a.op(Op::POP).op(Op::POP).op(Op::POP).push_u64(64).op(Op::ADD).jump(top);
            a.place(end);
            a.op(Op::STOP);
            a.finish()
        })
        .clone()
    }

    /// Init code that deploys a copy of the stressor's runtime, read with `EXTCODECOPY`.
    fn clone_init() -> Vec<u8> {
        let mut a = Asm::new(true);
        a.push_bytes(STRESSOR.as_slice()).op(Op::EXTCODESIZE);
        a.op(Op::DUP1).push_u64(0).push_u64(0).push_bytes(STRESSOR.as_slice());
        a.op(Op::EXTCODECOPY).push_u64(0).op(Op::RETURN);
        a.finish().to_vec()
    }

    /// The address a stressor child created with `salt` by `creator` gets.
    fn child(creator: Address, salt: B256) -> Address {
        creator.create2(salt, keccak256(Self::clone_init()))
    }

    fn pools() -> &'static Pools {
        static POOLS: OnceLock<Pools> = OnceLock::new();
        POOLS.get_or_init(|| {
            let mut rng = Rng::new(&[0x57e5_5000]);
            // (prefix nibbles, members) per group.
            let shapes: &[(usize, usize)] =
                &[(1, 24), (2, 16), (2, 16), (3, 12), (3, 12), (3, 8), (4, 6), (4, 6), (5, 2)];
            let slots = shapes
                .iter()
                .map(|(nibbles, members)| {
                    Self::grind(&mut rng, *nibbles, *members, |value| {
                        let mut slot = [0u8; 32];
                        slot[1..].copy_from_slice(&value.to_be_bytes::<32>()[1..]);
                        let slot = B256::from(slot);
                        (keccak256(slot), slot)
                    })
                })
                .collect();
            let addresses = [(1usize, 16usize), (2, 12), (3, 8), (4, 4)]
                .iter()
                .map(|(nibbles, members)| {
                    Self::grind(&mut rng, *nibbles, *members, |value| {
                        let address = Address::from_slice(&value.to_be_bytes::<32>()[12..]);
                        (keccak256(address), address)
                    })
                })
                .collect();
            Pools { slots, addresses }
        })
    }

    /// Finds `members` values whose hash starts with the same `nibbles` nibbles as the first.
    fn grind<T>(
        rng: &mut Rng,
        nibbles: usize,
        members: usize,
        hash: impl Fn(U256) -> (B256, T),
    ) -> Vec<T> {
        let prefix = |h: &B256| {
            let bits = nibbles * 4;
            U256::from_be_bytes(h.0) >> (256 - bits)
        };
        let (first_hash, first) = hash(rng.u256() >> 8);
        let target = prefix(&first_hash);
        let mut out = vec![first];
        let mut value = rng.u256() >> 8;
        while out.len() < members {
            value += U256::from(1);
            let (h, item) = hash(value);
            if prefix(&h) == target {
                out.push(item);
            }
        }
        out
    }

    /// Adds the stressor (prefilled storage, funded) and the empty accounts to a genesis state.
    pub fn seed_genesis(state: &mut WorldState, seed: u64) {
        let mut rng = Rng::new(&[seed, 0x57e5_5001]);
        state.set_code(STRESSOR, Self::runtime());
        state.fund(STRESSOR, U256::from(10).pow(U256::from(30)));
        let account = state.accounts.get_mut(&STRESSOR).expect("just created");
        for group in &Self::pools().slots {
            for slot in group {
                if rng.chance(70) {
                    account.storage.insert(*slot, U256::from(rng.range(1, u64::MAX)));
                }
            }
        }
        for slot in 0..rng.range(64, 400) {
            account.storage.insert(B256::from(U256::from(slot)), U256::from(rng.range(1, 1 << 40)));
        }
        for index in 0..Self::EMPTY_ACCOUNTS {
            let account = state.accounts.entry(Self::empty_account(index)).or_default();
            if index % 2 == 1 {
                for slot in 0..index {
                    account.storage.insert(B256::from(U256::from(slot)), U256::from(slot + 1));
                }
            }
        }
        for length in [1, 0x3000, Self::MAX_BLOB - 1] {
            state.register_code(Bytes::from(vec![0u8; length as usize]));
        }
    }

    /// Plans the block's stress transactions.
    pub fn plan(planner: &mut Planner<'_>, plan: &StressPlan) {
        let mut rng = planner.rng.fork(0x57e5_5002);
        // Slots written earlier in this block, for resets by later transactions.
        let mut written: Vec<(Address, B256, U256)> = Vec::new();
        // The salt of a child destroyed earlier in this block: creating it again gives an
        // account destroyed and recreated within one block.
        let mut destroyed: Option<B256> = None;
        for index in 0..plan.txs.min(Self::MAX_TXS) {
            let mut script = Script::default();
            let mut gas: u64 = 60_000;
            let mut label = "stress:slots";
            let mut target = STRESSOR;
            if rng.chance(u64::from(plan.destroy)) {
                // A child, created this transaction or earlier, written and maybe destroyed.
                // Ephemeral children are destroyed in the transaction that creates them (unless
                // one is recreated to stay); the others persist.
                let ephemeral = rng.chance(60);
                let salt = match destroyed {
                    Some(salt) if ephemeral && rng.chance(50) => salt,
                    _ if ephemeral => B256::from(U256::from(0x100 + rng.below(4096))),
                    _ => B256::from(U256::from(rng.below(6))),
                };
                let child = Self::child(STRESSOR, salt);
                let exists = planner.state.account(&child).is_some();
                if !exists {
                    script.push(Cmd::Clone, salt, U256::from(rng.below(3)));
                    gas += 200_000;
                }
                let mut inner = Script::default();
                gas += Self::slot_writes(&mut rng, planner.state, child, plan, &mut inner, None);
                // A child destroyed earlier in the block is recreated to stay.
                let destroy = !exists && ephemeral && destroyed != Some(salt) && rng.chance(75);
                if destroy {
                    inner.push(Cmd::Destruct, Self::beneficiary(&mut rng), U256::ZERO);
                    destroyed = Some(salt);
                }
                script.push(Cmd::Nested, B256::left_padding_from(child.as_slice()), U256::ZERO);
                script.nest(inner);
                label = if destroy { "stress:destroy" } else { "stress:child" };
                if exists && rng.chance(30) {
                    target = child;
                }
            }
            if target == STRESSOR {
                gas += Self::slot_writes(
                    &mut rng,
                    planner.state,
                    STRESSOR,
                    plan,
                    &mut script,
                    Some(&mut written),
                );
            } else {
                // Only the child's script: call it directly, without the clone and nesting.
                script = Script::default();
                gas += Self::slot_writes(&mut rng, planner.state, target, plan, &mut script, None);
            }
            if plan.accounts > 0 && rng.chance(50) {
                let count = rng.range(1, u64::from(plan.accounts) + 1);
                if rng.chance(50) {
                    let seed = Self::key(&mut rng);
                    script.push(Cmd::Spray, seed, (U256::from(1) << 128) | U256::from(count));
                } else {
                    let group = rng.pick(&Self::pools().addresses);
                    for address in group.iter().take(count as usize) {
                        script.push(
                            Cmd::Call,
                            B256::left_padding_from(address.as_slice()),
                            U256::from(1),
                        );
                    }
                }
                gas += 40_000 * count;
            }
            if rng.chance(u64::from(plan.touch)) {
                for _ in 0..rng.range(1, 4) {
                    let address = Self::empty_account(rng.below(Self::EMPTY_ACCOUNTS));
                    let value = if rng.chance(20) { U256::from(1) } else { U256::ZERO };
                    script.push(Cmd::Call, B256::left_padding_from(address.as_slice()), value);
                    gas += 40_000;
                }
            }
            if rng.chance(u64::from(plan.large)) {
                let any = rng.range(1, 0x6000);
                let len = *rng.pick(&[Self::MAX_BLOB - 1, Self::MAX_BLOB, 1, 0x3000, any]);
                planner.new_codes.push(Bytes::from(vec![0u8; len as usize]));
                let salt = Self::key(&mut rng);
                script.push(Cmd::Blob, salt, U256::from(len));
                gas += 100_000 + 220 * len;
                label = "stress:large";
            }
            if index > 0 && rng.chance(u64::from(plan.reset)) && !written.is_empty() {
                // Put back slots an earlier transaction of this block changed.
                for _ in 0..rng.range(1, 8) {
                    let (address, slot, previous) = written[rng.index(written.len())];
                    if address == STRESSOR {
                        script.push(Cmd::Store, slot, previous);
                        gas += 25_000;
                    }
                }
            }
            let value = if rng.chance(10) { U256::from(rng.range(1, 1000)) } else { U256::ZERO };
            let bytes = script.0.into();
            if planner.ctx.deposits_allowed && rng.chance(10) {
                planner.deposit_call(
                    Self::beneficiary_address(&mut rng),
                    target,
                    U256::ZERO,
                    bytes,
                    label,
                );
            } else {
                planner.call_gas(None, target, value, bytes, label.into(), Some(gas));
            }
        }
    }

    /// Adds about `plan.slots` slot writes to `script` for `account` and returns their gas.
    fn slot_writes(
        rng: &mut Rng,
        state: &WorldState,
        account: Address,
        plan: &StressPlan,
        script: &mut Script,
        mut written: Option<&mut Vec<(Address, B256, U256)>>,
    ) -> u64 {
        let pools = Self::pools();
        let existing: Vec<B256> = state
            .account(&account)
            .map(|a| a.storage.keys().copied().collect())
            .unwrap_or_default();
        let count = rng.range(1, u64::from(plan.slots.max(1)) + 1);
        let mut gas = 0;
        let mut index = 0;
        while index < count {
            if rng.chance(8) {
                // A dense run of consecutive small slots, filled or cleared.
                let start = B256::from(U256::from(rng.below(512)));
                let len = rng.range(1, 64).min(count - index);
                let value =
                    if rng.chance(u64::from(plan.delete)) { 0 } else { rng.range(1, 1 << 30) };
                script.push(Cmd::Range, start, (U256::from(value) << 128) | U256::from(len));
                gas += 25_000 * len;
                index += len;
                continue;
            }
            index += 1;
            let delete = rng.chance(u64::from(plan.delete)) && !existing.is_empty();
            let slot = if delete {
                existing[rng.index(existing.len())]
            } else if rng.chance(u64::from(plan.hot)) {
                pools.slots[0][rng.index(8)]
            } else if rng.chance(u64::from(plan.cluster)) {
                let group = rng.pick(&pools.slots);
                group[rng.index(group.len())]
            } else if rng.chance(50) {
                B256::from(U256::from(rng.below(1024)))
            } else {
                Self::key(rng)
            };
            let previous = state.storage(&account, slot);
            let value = if delete {
                U256::ZERO
            } else if rng.chance(15) {
                previous
            } else {
                U256::from(rng.range(1, u64::MAX))
            };
            if rng.chance(15) {
                script.push(Cmd::Add, slot, U256::from(rng.range(1, 1000)));
            } else {
                script.push(Cmd::Store, slot, value);
            }
            gas += 25_000;
            if rng.chance(u64::from(plan.reset) / 2) {
                // Written and put back (or cleared) within the transaction.
                let back = if rng.chance(70) { previous } else { U256::ZERO };
                script.push(Cmd::Store, slot, back);
                gas += 5_000;
            }
            if let Some(written) = written.as_mut() {
                written.push((account, slot, previous));
            }
        }
        gas
    }

    /// A slot the stress traffic often writes (from the clustered pools or a small integer),
    /// worth reading back on any account it touches.
    pub fn probe_slot(rng: &mut Rng) -> B256 {
        if rng.chance(70) {
            let group = rng.pick(&Self::pools().slots);
            group[rng.index(group.len())]
        } else {
            B256::from(U256::from(rng.below(512)))
        }
    }

    /// A random key below 2^248 (the script's key width).
    fn key(rng: &mut Rng) -> B256 {
        B256::from(rng.u256() >> 8)
    }

    fn beneficiary(rng: &mut Rng) -> B256 {
        B256::left_padding_from(Self::beneficiary_address(rng).as_slice())
    }

    fn beneficiary_address(rng: &mut Rng) -> Address {
        match rng.below(3) {
            0 => STRESSOR,
            1 => Self::empty_account(rng.below(Self::EMPTY_ACCOUNTS)),
            _ => rng.address(),
        }
    }
}

/// A stressor script under construction.
#[derive(Debug, Default)]
struct Script(Vec<u8>);

impl Script {
    fn push(&mut self, cmd: Cmd, key: B256, arg: U256) {
        let mut word = key.0;
        word[0] = cmd as u8;
        self.0.extend_from_slice(&word);
        self.0.extend_from_slice(&arg.to_be_bytes::<32>());
    }

    /// Appends `inner` as the script of the [`Cmd::Nested`] command just pushed.
    fn nest(&mut self, inner: Self) {
        let at = self.0.len() - 32;
        self.0[at..].copy_from_slice(&U256::from(inner.0.len()).to_be_bytes::<32>());
        self.0.extend(inner.0);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, keccak256};
    use revm::{
        Context, ExecuteEvm, MainBuilder, MainContext,
        context::TxEnv,
        database::{CacheDB, EmptyDB},
        primitives::hardfork::SpecId,
        state::{AccountInfo, Bytecode},
    };

    use super::{Cmd, STRESSOR, Script, Stress};

    #[test]
    fn pools_share_their_prefixes() {
        let pools = Stress::pools();
        for (group, nibbles) in pools.slots.iter().zip([1usize, 2, 2, 3, 3, 3, 4, 4, 5]) {
            let prefix =
                |slot: &B256| U256::from_be_bytes(keccak256(slot).0) >> (256 - 4 * nibbles);
            assert!(group.iter().all(|slot| prefix(slot) == prefix(&group[0])));
            assert!(group.iter().all(|slot| slot.0[0] == 0));
        }
    }

    /// One script through every command: the resulting state is what each command promises.
    #[test]
    fn stressor_commands_do_what_they_say() {
        let word = |n: u64| B256::from(U256::from(n));
        let pad = |a: Address| B256::left_padding_from(a.as_slice());
        let salt = word(7);
        let child = Stress::child(STRESSOR, salt);
        let kept = Stress::child(STRESSOR, word(8));
        let beneficiary = Address::with_last_byte(0xbe);
        let mut script = Script::default();
        script.push(Cmd::Store, word(1), U256::from(11));
        script.push(Cmd::Store, word(2), U256::from(22));
        script.push(Cmd::Store, word(2), U256::ZERO);
        script.push(Cmd::Add, word(1), U256::from(5));
        script.push(Cmd::Range, word(100), (U256::from(9) << 128) | U256::from(3));
        script.push(Cmd::Spray, word(0x5eed), (U256::from(1) << 128) | U256::from(2));
        script.push(Cmd::Call, pad(Address::with_last_byte(0xca)), U256::from(4));
        script.push(Cmd::Clone, salt, U256::ZERO);
        script.push(Cmd::Nested, pad(child), U256::ZERO);
        let mut inner = Script::default();
        inner.push(Cmd::Store, word(3), U256::from(33));
        inner.push(Cmd::Destruct, pad(beneficiary), U256::ZERO);
        script.nest(inner);
        script.push(Cmd::Clone, word(8), U256::ZERO);
        script.push(Cmd::Nested, pad(kept), U256::ZERO);
        let mut inner = Script::default();
        inner.push(Cmd::Store, word(4), U256::from(44));
        script.nest(inner);
        script.push(Cmd::Blob, word(9), U256::from(0x6000));
        script.push(Cmd::Store, word(5), U256::from(55));

        let mut db = CacheDB::<EmptyDB>::default();
        let mut info = AccountInfo::from_bytecode(Bytecode::new_raw(Stress::runtime()));
        info.balance = U256::from(1_000_000);
        db.insert_account_info(STRESSOR, info);
        let mut evm = Context::mainnet()
            .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecId::PRAGUE))
            .with_db(db)
            .build_mainnet();
        let tx = TxEnv::builder()
            .kind(STRESSOR.into())
            .data(script.0.into())
            .gas_limit(16_000_000)
            .gas_price(0)
            .build_fill();
        let outcome = evm.transact(tx).expect("executes");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let state = &outcome.state;
        let slot = |address: &Address, n: u64| {
            state[address].storage.get(&U256::from(n)).map_or(U256::ZERO, |s| s.present_value)
        };
        assert_eq!(slot(&STRESSOR, 1), U256::from(16));
        assert_eq!(slot(&STRESSOR, 2), U256::ZERO);
        for n in 100..103 {
            assert_eq!(slot(&STRESSOR, n), U256::from(9));
        }
        assert_eq!(slot(&STRESSOR, 103), U256::ZERO);
        for j in 0..2u64 {
            let mut preimage = [0u8; 64];
            preimage[..32].copy_from_slice(word(0x5eed).as_slice());
            preimage[32..].copy_from_slice(word(j).as_slice());
            let sprayed = Address::from_slice(&keccak256(preimage)[12..]);
            assert_eq!(state[&sprayed].info.balance, U256::from(1), "sprayed account {j}");
        }
        assert_eq!(state[&Address::with_last_byte(0xca)].info.balance, U256::from(4));
        assert!(state[&child].is_selfdestructed(), "the child destroys itself");
        assert_eq!(slot(&kept, 4), U256::from(44));
        assert_eq!(state[&kept].info.code_hash, keccak256(Stress::runtime()));
        let blob = STRESSOR.create2(word(9), keccak256([0x61, 0x60, 0x00, 0x5f, 0xf3]));
        assert_eq!(state[&blob].info.code_hash, keccak256(vec![0u8; 0x6000]));
        assert_eq!(slot(&STRESSOR, 5), U256::from(55), "the script runs past every command");
    }
}
