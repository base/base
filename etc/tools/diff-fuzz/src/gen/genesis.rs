//! Synthetic starting points: an era's bootstrap state plus generated accounts and contracts,
//! with a consistent synthetic ancestor chain ending at the parent block.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use base_common_consensus::Predeploys;
use base_common_genesis::BaseFeature;

use crate::r#gen::{
    block::{Blocks, SysConfig},
    bootstrap::Bootstrap,
    era::{Era, Schedule},
    keys::Keys,
    program::{Program, ProgramCtx, Templates},
    registry::{DeployedProgram, Registry},
    rng::Rng,
    state::WorldState,
    trie::TrieBuilder,
};

/// A synthetic starting point for a block or chain.
#[derive(Debug, Clone)]
pub struct Genesis {
    /// State at the parent block.
    pub state: WorldState,
    /// Knowledge about the state.
    pub registry: Registry,
    /// Ancestor headers, newest (the parent) first.
    pub ancestors: Vec<Header>,
}

/// Fixed addresses of template contracts placed at genesis.
const TEMPLATE_BASE: u64 = 0xd1ff_0000;
/// The template that fills and clears storage ranges.
pub const FILLER: Address = address!("0x00000000000000000000000000000000d1ff0003");
/// The call relay template (see [`Templates::relay`]).
pub const RELAY: Address = address!("0x00000000000000000000000000000000d1ff0004");
/// The recursion template (see [`Templates::recurser`]).
pub const RECURSER: Address = address!("0x00000000000000000000000000000000d1ff0005");
/// The creation template (see [`Templates::creator`]).
pub const CREATOR: Address = address!("0x00000000000000000000000000000000d1ff0006");
/// The self-destruct-to-argument template (see [`Templates::destructor_to`]).
pub const DESTRUCTOR_TO: Address = address!("0x00000000000000000000000000000000d1ff0007");
/// The refund template (see [`Templates::refunder`]).
pub const REFUNDER: Address = address!("0x00000000000000000000000000000000d1ff0008");
/// A creator whose nonce is `u64::MAX`.
pub const CREATOR_MAX_NONCE: Address = address!("0x00000000000000000000000000000000d1ff0100");
/// A keyless account whose first [`COLLIDER_NONCES`] `CREATE` addresses already exist, so its
/// creations (as deposits) collide.
pub const COLLIDER: Address = address!("0x00000000000000000000000000000000d1ff0101");
/// Nonces of [`COLLIDER`] whose `CREATE` address is taken.
pub const COLLIDER_NONCES: u64 = 4;

const HISTORY: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");
const HISTORY_SERVE_WINDOW: u64 = 8191;

impl Genesis {
    /// Builds a starting point whose parent is block `parent_number`, with state from `era`.
    pub fn build(
        rng: &mut Rng,
        era: Era,
        parent_number: u64,
        sys: &SysConfig,
        ancestors: u64,
    ) -> Self {
        let schedule = Schedule::mainnet();
        let parent_timestamp = schedule.timestamp_parts(parent_number).0;
        let upgrade = era.upgrade();
        let push0 = upgrade.is_feature_enabled(BaseFeature::Create2Deployer);
        let bootstrapped = Bootstrap::state(era);
        let mut state = bootstrapped.state.clone();
        let mut registry = Registry::default();
        registry.learn_logs(&bootstrapped.logs);

        // Every code the chain can come to hold through upgrades and templates.
        for (hash, code) in Bootstrap::upgrade_codes() {
            state.codes.entry(*hash).or_insert_with(|| code.clone());
        }
        for push0 in [false, true] {
            for runtime in Templates::all(push0) {
                registry.template_hashes.insert(state.register_code(runtime));
            }
        }
        // B-20 tokens carry a one-byte marker code.
        state.register_code(Bytes::from_static(&[0xef]));
        // Codes the creator template's init kinds deploy besides templates: one zero byte and
        // the maximum code size of zeros.
        state.register_code(Bytes::from_static(&[0x00]));
        state.register_code(Bytes::from(vec![0u8; 0x6000]));

        // Funded keys, with a few edge balances and nonces.
        let keys = Keys::get();
        for index in 0..Keys::COUNT {
            let address = keys.address(index);
            let balance = match index {
                22 => U256::ZERO,
                23 => U256::from(rng.range(1, 10u64.pow(15))),
                // Large but below 2^128 even when many such balances end up in one account:
                // revm computes gas_limit * max_fee in u128 and rejects on overflow, which only
                // diverges from 256-bit clients for balances no real chain can reach.
                _ if rng.chance(5) => U256::from(1) << 120,
                _ => {
                    U256::from(rng.range(1, 1000))
                        * U256::from(10).pow(U256::from(rng.range(21, 24)))
                }
            };
            state.fund(address, balance);
            if rng.chance(10) {
                state.accounts.get_mut(&address).expect("funded").nonce = rng.range(1, 50);
            }
        }
        // WETH9 balances (the `balanceOf` mapping is slot 3), backed by the contract's ether,
        // so withdrawals have something to unwrap.
        if state.account(&Predeploys::WETH9).is_some() {
            let mut backing = U256::ZERO;
            for index in 0..16 {
                let amount = U256::from(rng.range(1, 1 << 50));
                let mut preimage = [0u8; 64];
                preimage[12..32].copy_from_slice(keys.address(index).as_slice());
                preimage[63] = 3;
                let account = state.accounts.get_mut(&Predeploys::WETH9).expect("exists");
                account.storage.insert(keccak256(preimage), amount);
                backing += amount;
            }
            state.fund(Predeploys::WETH9, backing);
        }
        // Fee vaults above their withdrawal minimum, so withdrawals reach the bridge.
        for vault in [
            Predeploys::SEQUENCER_FEE_VAULT,
            Predeploys::BASE_FEE_VAULT,
            Predeploys::L1_FEE_VAULT,
            Predeploys::OPERATOR_FEE_VAULT,
        ] {
            if state.account(&vault).is_some() && rng.chance(60) {
                state
                    .fund(vault, U256::from(rng.range(1, 50)) * U256::from(10).pow(U256::from(18)));
            }
        }

        // Template contracts at fixed addresses.
        for (i, runtime) in Templates::all(push0).into_iter().enumerate() {
            let address = Address::from_word(B256::from(U256::from(TEMPLATE_BASE + i as u64)));
            state.set_code(address, runtime);
            registry.templates.insert(address);
        }
        // Non-zero original values for the refunder's slots, endowment for the creator, and a
        // creator copy whose nonce cannot grow (EIP-2681), so its creations fail.
        let refunder = state.accounts.get_mut(&REFUNDER).expect("template exists");
        for slot in 1..=64u64 {
            refunder.storage.insert(B256::from(U256::from(slot)), U256::from(slot * 7 + 1));
        }
        state.fund(CREATOR, U256::from(10).pow(U256::from(18)));
        state.set_code(CREATOR_MAX_NONCE, Templates::creator(push0));
        state.fund(CREATOR_MAX_NONCE, U256::from(10).pow(U256::from(18)));
        state.accounts.get_mut(&CREATOR_MAX_NONCE).expect("just set").nonce = u64::MAX;
        for nonce in 0..COLLIDER_NONCES {
            let taken = COLLIDER.create(nonce);
            state.fund(taken, U256::from(1));
            state.accounts.get_mut(&taken).expect("just funded").nonce = 1;
        }
        // Keys that can never send valid transactions: one at the maximum nonce, one with code
        // that is not a delegation (EIP-3607).
        state.accounts.get_mut(&keys.address(Keys::MAX_NONCE)).expect("funded").nonce = u64::MAX;
        state.set_code(keys.address(Keys::WITH_CODE), Bytes::from_static(&[0x00]));

        // Pre-deployed programs.
        let mut targets: Vec<Address> = vec![Predeploys::WETH9, keys.address(1), FILLER];
        for i in 0..rng.range(2, 6) {
            let address = Address::from_word(B256::from(U256::from(0xc0de_0000u64 + i)));
            let program = Program::generate(rng, &ProgramCtx { upgrade, targets: targets.clone() });
            state.set_code(address, program.runtime.clone());
            state.fund(address, U256::from(rng.below(1 << 40)));
            for runtime in program.created_runtimes {
                state.register_code(runtime);
            }
            registry.programs.push(DeployedProgram { address, fragments: program.fragments });
            targets.push(address);
        }

        // Existing delegations from Isthmus.
        if upgrade.is_feature_enabled(BaseFeature::Eip7702) {
            for index in 16..20 {
                if rng.chance(40) {
                    let delegate = registry.programs[rng.index(registry.programs.len())].address;
                    let mut code = vec![0xef, 0x01, 0x00];
                    code.extend_from_slice(delegate.as_slice());
                    state.set_code(keys.address(index), code.into());
                }
            }
        }

        // Ancestors: everything but the parent's state root first, then the system contract
        // storage that refers to them, then the parent's roots.
        let mut ancestors =
            Blocks::synthetic_ancestors(rng, sys, parent_number, ancestors, B256::ZERO, B256::ZERO);
        if schedule.active(BaseFeature::HistoryStorage, parent_timestamp)
            && state.account(&HISTORY).is_some()
        {
            for header in ancestors.iter().skip(1) {
                let slot = B256::from(U256::from(header.number % HISTORY_SERVE_WINDOW));
                let hash = header.hash_slow();
                state.accounts.get_mut(&HISTORY).expect("exists").storage.insert(slot, hash.into());
            }
        }
        if schedule.active(BaseFeature::BeaconRoots, parent_timestamp)
            && state.account(&Predeploys::BEACON_BLOCK_ROOT).is_some()
        {
            for header in ancestors.iter().rev() {
                if let Some(root) = header.parent_beacon_block_root {
                    let index = header.timestamp % HISTORY_SERVE_WINDOW;
                    let account =
                        state.accounts.get_mut(&Predeploys::BEACON_BLOCK_ROOT).expect("exists");
                    account
                        .storage
                        .insert(B256::from(U256::from(index)), U256::from(header.timestamp));
                    account
                        .storage
                        .insert(B256::from(U256::from(index + HISTORY_SERVE_WINDOW)), root.into());
                }
            }
        }
        let (root, _) = state.witness();
        let parent = &mut ancestors[0];
        parent.state_root = root;
        if schedule.active(BaseFeature::L2ToL1MessageRoot, parent_timestamp) {
            parent.withdrawals_root =
                Some(Self::storage_root(&state, &Predeploys::L2_TO_L1_MESSAGE_PASSER));
        }
        Self { state, registry, ancestors }
    }

    /// The storage root of one account.
    pub fn storage_root(state: &WorldState, address: &Address) -> B256 {
        let mut builder = TrieBuilder::default();
        let leaves = state
            .account(address)
            .map(|account| {
                account
                    .storage
                    .iter()
                    .map(|(slot, value)| (keccak256(slot), alloy_rlp::encode(value)))
                    .collect()
            })
            .unwrap_or_default();
        builder.build(leaves)
    }
}
