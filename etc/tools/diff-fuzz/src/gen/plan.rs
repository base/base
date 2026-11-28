//! Transaction planning for one block: random traffic, system interactions, deposits, and
//! deliberately invalid transactions.

use std::collections::BTreeMap;

use alloy_eips::{Encodable2718, eip2930::AccessList};
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, TxKind, U256, address, keccak256};
use alloy_signer::SignerSync;
use base_common_base_tx::{IntrinsicGas, NonceManagerSlots};
use base_common_chains::MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS;
use base_common_consensus::{
    AccountChange, Call, Delegation, Predeploys, TxBase, TxBaseConstants, TxBaseSigned, TxDeposit,
};
use base_common_genesis::{BaseFeature, BaseUpgrade};
use base_common_precompiles::{B20FactoryStorage, B20Variant};

use crate::r#gen::{
    abi::{AbiMutation, NativeAbi, NativeTarget},
    asm::Asm,
    block::L1Origin,
    genesis::{
        COLLIDER, COLLIDER_NONCES, CREATOR, CREATOR_MAX_NONCE, DESTRUCTOR_TO, RECURSER, REFUNDER,
        RELAY,
    },
    keys::Keys,
    native::{Caller, Native, NativeCall},
    precompile::Precompiles,
    program::{Ending, Fragment, Param, Program, ProgramCtx, Templates},
    registry::{PendingDeploy, Registry},
    rng::Rng,
    state::WorldState,
    tx::{Shape, TxSpec, Txs},
};

/// The block a plan is for.
#[derive(Debug, Clone)]
pub struct BlockCtx {
    /// Block number.
    pub number: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Rules in effect.
    pub upgrade: BaseUpgrade,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee used for execution.
    pub base_fee: u64,
    /// L1 origin, for deposit source hashes.
    pub origin: L1Origin,
    /// Whether user deposits may be included (first block of an epoch).
    pub deposits_allowed: bool,
}

/// One planned transaction.
#[derive(Debug, Clone)]
pub struct PlannedTx {
    /// EIP-2718 encoding.
    pub raw: Bytes,
    /// What the transaction exercises, for statistics.
    pub label: String,
    /// The deployment it makes, if any.
    pub deploy: Option<PendingDeploy>,
}

/// Plans the transactions of one block against a known pre-state.
#[derive(Debug)]
pub struct Planner<'a> {
    /// Randomness.
    pub rng: Rng,
    /// Block context.
    pub ctx: BlockCtx,
    /// Pre-state.
    pub state: &'a WorldState,
    /// Chain knowledge.
    pub registry: &'a Registry,
    /// Expected next nonce per sender, assuming planned transactions are included.
    pub nonces: BTreeMap<Address, u64>,
    /// Planned spend per sender.
    pub spent: BTreeMap<Address, U256>,
    /// Gas not yet claimed by planned transactions' gas limits.
    pub gas_left: u64,
    /// User deposits planned so far.
    pub deposits: u64,
    /// Planned transactions.
    pub out: Vec<PlannedTx>,
    /// Runtimes planned transactions may deploy.
    pub new_codes: Vec<Bytes>,
    /// Whether the transaction being built carries a valid self-authorization.
    pub self_bump: bool,
    /// Next nonces of other authorities of the transaction being built, applied only once the
    /// transaction is planned.
    pub auth_nonces: BTreeMap<Address, u64>,
    /// See `GeneratorOptions::max_nonce_deposit_creates`.
    pub max_nonce_deposit_creates: bool,
    /// Expected next sequence per Base transaction sender and 2D nonce key.
    pub base_nonces: BTreeMap<(Address, U256), u64>,
}

/// Addresses of contracts every era has.
const WETH: Address = Predeploys::WETH9;
const CREATE2_DEPLOYER: Address = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");
const HISTORY: Address = address!("0x0000F90827F1C53a10cb7A02335B175320002935");

fn selector(signature: &str) -> Vec<u8> {
    keccak256(signature.as_bytes())[..4].to_vec()
}

const fn word(value: U256) -> [u8; 32] {
    value.to_be_bytes()
}

impl<'a> Planner<'a> {
    /// Creates a planner.
    pub const fn new(
        rng: Rng,
        ctx: BlockCtx,
        state: &'a WorldState,
        registry: &'a Registry,
        gas_used: u64,
    ) -> Self {
        let gas_left = ctx.gas_limit.saturating_sub(gas_used);
        Self {
            rng,
            ctx,
            state,
            registry,
            nonces: BTreeMap::new(),
            spent: BTreeMap::new(),
            gas_left,
            deposits: 0,
            out: Vec::new(),
            new_codes: Vec::new(),
            self_bump: false,
            auth_nonces: BTreeMap::new(),
            max_nonce_deposit_creates: true,
            base_nonces: BTreeMap::new(),
        }
    }

    const fn push0(&self) -> bool {
        self.ctx.upgrade.is_feature_enabled(BaseFeature::Create2Deployer)
    }

    const fn feature(&self, feature: BaseFeature) -> bool {
        self.ctx.upgrade.is_feature_enabled(feature)
    }

    /// Expected nonce of `address`.
    pub fn nonce(&self, address: &Address) -> u64 {
        self.nonces.get(address).copied().unwrap_or_else(|| self.state.nonce(address))
    }

    fn available(&self, address: &Address) -> U256 {
        self.state
            .balance(address)
            .saturating_sub(self.spent.get(address).copied().unwrap_or_default())
    }

    fn has_code(&self, address: &Address) -> bool {
        self.state
            .account(address)
            .is_some_and(|a| a.code_hash != KECCAK256_EMPTY && !self.is_delegated(address))
    }

    fn is_delegated(&self, address: &Address) -> bool {
        self.state
            .code(address)
            .is_some_and(|code| code.len() == 23 && code[..3] == [0xef, 0x01, 0x00])
    }

    /// A funded key without code (EIP-3607), preferring `preferred` when it qualifies.
    pub fn sender(&mut self, preferred: Option<Address>) -> Option<usize> {
        let keys = Keys::get();
        let ok = |planner: &Self, index: usize| {
            let address = keys.address(index);
            !planner.has_code(&address)
                && planner.nonce(&address) < u64::MAX
                && planner.available(&address) > U256::from(10u128.pow(17))
        };
        if let Some(address) = preferred
            && let Some(index) = keys.index_of(&address)
            && ok(self, index)
        {
            return Some(index);
        }
        for _ in 0..8 {
            let index = self.rng.index(Keys::COUNT);
            if ok(self, index) {
                return Some(index);
            }
        }
        (0..Keys::COUNT).find(|index| ok(self, *index))
    }

    const fn shape(&mut self) -> Shape {
        let eip7702 = self.feature(BaseFeature::Eip7702);
        match self.rng.below(20) {
            0 => Shape::Legacy { eip155: false },
            1..=4 => Shape::Legacy { eip155: true },
            5..=6 => Shape::Eip2930,
            7 if eip7702 => Shape::Eip7702,
            _ => Shape::Eip1559,
        }
    }

    fn fees(&mut self) -> (u128, u128) {
        let base = u128::from(self.ctx.base_fee);
        let max_fee = match self.rng.below(10) {
            0 => base,
            1 => base + 1,
            2 => base * 2,
            3 => base * 10 + 1_000_000_000,
            _ => base + base / 8 + u128::from(self.rng.below(1_000_000)),
        };
        let priority = match self.rng.below(5) {
            0 => 0,
            1 => max_fee - base,
            _ => u128::from(self.rng.below(1_000_000)).min(max_fee - base),
        };
        (max_fee.max(1), priority)
    }

    const fn tx_cap(&self) -> u64 {
        if self.feature(BaseFeature::TransactionGasLimitCap) { 1 << 24 } else { u64::MAX }
    }

    /// Picks a gas limit at or above the intrinsic cost, near boundaries now and then.
    fn gas_for(&mut self, intrinsic: u64, floor: u64, heavy: bool) -> u64 {
        let base = intrinsic.max(floor);
        let budget = self.gas_left.min(self.tx_cap());
        let wanted = match self.rng.below(12) {
            0 => base,
            1 => base + self.rng.range(1, 3_000),
            2 => base + self.rng.range(3_000, 40_000),
            3 | 4 => base + 200_000,
            5 if heavy => base + 5_000_000,
            _ => base + if heavy { 1_500_000 } else { 400_000 },
        };
        wanted.min(budget.max(base))
    }

    /// Records a signed transaction and its effects on planning state.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        key: usize,
        shape: Shape,
        to: TxKind,
        value: U256,
        input: Bytes,
        access_list: AccessList,
        authorizations: Vec<alloy_eips::eip7702::SignedAuthorization>,
        gas_limit: Option<u64>,
        heavy: bool,
        label: String,
        deploy: Option<PendingDeploy>,
    ) -> Option<()> {
        let bump = if std::mem::take(&mut self.self_bump) { 2 } else { 1 };
        let auth_nonces = std::mem::take(&mut self.auth_nonces);
        let sender = Keys::get().address(key);
        let shape = match (shape, to) {
            (Shape::Eip7702 | Shape::Eip4844, TxKind::Create) => Shape::Eip1559,
            (Shape::Eip7702, _) if authorizations.is_empty() => Shape::Eip1559,
            _ => shape,
        };
        let access_list =
            if matches!(shape, Shape::Legacy { .. }) { AccessList::default() } else { access_list };
        let (intrinsic, floor) = Txs::intrinsic(
            self.ctx.upgrade,
            &input,
            to.is_create(),
            &access_list,
            if shape == Shape::Eip7702 { authorizations.len() } else { 0 },
        );
        let gas_limit = gas_limit.unwrap_or_else(|| self.gas_for(intrinsic, floor, heavy));
        if gas_limit > self.gas_left || gas_limit < intrinsic.max(floor) {
            return None;
        }
        let (max_fee, priority_fee) = self.fees();
        let cost = U256::from(gas_limit) * U256::from(max_fee) + value;
        if cost > self.available(&sender) {
            return None;
        }
        let nonce = self.nonce(&sender);
        let spec = TxSpec {
            key,
            shape,
            chain_id: 8453,
            nonce,
            to,
            value,
            input,
            gas_limit,
            max_fee,
            priority_fee,
            access_list,
            authorizations,
            high_s: false,
        };
        self.out.push(PlannedTx { raw: Txs::sign(&spec), label, deploy });
        self.nonces.insert(sender, nonce.saturating_add(bump));
        self.nonces.extend(auth_nonces);
        *self.spent.entry(sender).or_default() += cost;
        self.gas_left -= gas_limit;
        Some(())
    }

    /// Sends `input` to `to` from a funded key with default shape and gas.
    pub fn call(
        &mut self,
        from: Option<Address>,
        to: Address,
        value: U256,
        input: Bytes,
        label: String,
    ) -> Option<()> {
        self.call_gas(from, to, value, input, label, None)
    }

    /// Like [`Self::call`] with an explicit gas limit (capped by the block and the per-tx cap).
    pub fn call_gas(
        &mut self,
        from: Option<Address>,
        to: Address,
        value: U256,
        input: Bytes,
        label: String,
        gas: Option<u64>,
    ) -> Option<()> {
        let key = self.sender(from)?;
        let shape = self.shape();
        let shape = if shape == Shape::Eip7702 { Shape::Eip1559 } else { shape };
        let access_list = self.access_list(to);
        let gas = gas.map(|gas| gas.min(self.tx_cap()).min(self.gas_left));
        self.send(
            key,
            shape,
            TxKind::Call(to),
            value,
            input,
            access_list,
            Vec::new(),
            gas,
            true,
            label,
            None,
        )
    }

    fn access_list(&mut self, to: Address) -> AccessList {
        if !self.rng.chance(20) {
            return AccessList::default();
        }
        let slots: Vec<B256> = (0..self.rng.below(4)).map(|i| B256::from(U256::from(i))).collect();
        let mut addresses = vec![to];
        if self.rng.chance(30) {
            addresses.push(Precompiles::ECRECOVER);
        }
        Txs::access_list(&addresses, &slots)
    }

    /// A recipient address of any kind.
    pub fn recipient(&mut self) -> Address {
        let keys = Keys::get();
        match self.rng.below(14) {
            0..=3 => keys.address(self.rng.index(Keys::COUNT)),
            4 => self.rng.address(),
            5 => *self.rng.pick(&Precompiles::active(self.ctx.upgrade)),
            6 => *self.rng.pick(&[
                WETH,
                Predeploys::L1_BLOCK_INFO,
                Predeploys::GAS_PRICE_ORACLE,
                Predeploys::SEQUENCER_FEE_VAULT,
                Predeploys::L2_TO_L1_MESSAGE_PASSER,
                Predeploys::BEACON_BLOCK_ROOT,
                HISTORY,
                CREATE2_DEPLOYER,
            ]),
            7..=9 if !self.registry.programs.is_empty() => {
                self.registry.programs[self.rng.index(self.registry.programs.len())].address
            }
            10 if !self.registry.templates.is_empty() => {
                let templates: Vec<Address> = self.registry.templates.iter().copied().collect();
                templates[self.rng.index(templates.len())]
            }
            11 if !self.registry.tokens.is_empty() => {
                self.registry.tokens[self.rng.index(self.registry.tokens.len())].address
            }
            12 => {
                let delegations = Registry::delegations(self.state);
                if delegations.is_empty() {
                    keys.address(1)
                } else {
                    delegations[self.rng.index(delegations.len())].0
                }
            }
            _ => Address::with_last_byte(self.rng.below(256) as u8),
        }
    }

    fn value(&mut self, sender: &Address) -> U256 {
        let available = self.available(sender);
        match self.rng.below(10) {
            0..=3 => U256::ZERO,
            4 => U256::from(1),
            5 => available / U256::from(1000),
            _ => U256::from(self.rng.range(1, 1_000_000_000_000)),
        }
    }

    // ── Traffic kinds ──────────────────────────────────────────────────────

    /// A plain value transfer.
    pub fn transfer(&mut self) -> Option<()> {
        let key = self.sender(None)?;
        let sender = Keys::get().address(key);
        let to = self.recipient();
        let value = self.value(&sender);
        let shape = self.shape();
        let input = if self.rng.chance(20) {
            Bytes::from(self.rng.bytes_between(1, 300))
        } else {
            Bytes::new()
        };
        let (intrinsic, floor) =
            Txs::intrinsic(self.ctx.upgrade, &input, false, &AccessList::default(), 0);
        let gas = if self.rng.chance(50) && !self.has_code(&to) {
            Some(intrinsic.max(floor))
        } else {
            None
        };
        let auths = if shape == Shape::Eip7702 { self.authorizations(key) } else { Vec::new() };
        self.send(
            key,
            shape,
            TxKind::Call(to),
            value,
            input,
            AccessList::default(),
            auths,
            gas,
            false,
            "transfer".into(),
            None,
        )
    }

    /// Parameters for a program call, chosen from what the state holds.
    pub fn program_params(&mut self, target: Address) -> [U256; Param::COUNT] {
        let mut params = [U256::ZERO; Param::COUNT];
        let slot = self.rng.below(16);
        let current = self.state.storage(&target, B256::from(U256::from(slot)));
        params[Param::Slot as usize - 1] = U256::from(slot);
        params[Param::Value as usize - 1] = match self.rng.below(6) {
            0 => U256::ZERO,
            1 => current,
            2 => current + U256::from(1),
            3 => U256::from(1),
            _ => self.rng.edge_u256(),
        };
        params[Param::Offset as usize - 1] = U256::from(self.rng.below(2048));
        let call_target = self.recipient();
        params[Param::Target as usize - 1] = U256::from_be_slice(call_target.as_slice());
        params[Param::CallValue as usize - 1] =
            U256::from(*self.rng.pick(&[0u64, 0, 1, 1000, 1 << 40]));
        // Budgets that run out partway through an inner frame, as well as round numbers.
        params[Param::CallGas as usize - 1] = U256::from(match self.rng.below(4) {
            0 => self.rng.range(1, 5_000),
            1 => self.rng.range(5_000, 200_000),
            _ => *self.rng.pick(&[0u64, 2300, 30_000, 100_000, u64::MAX]),
        });
        params[Param::Depth as usize - 1] = U256::from(match self.rng.below(20) {
            0 | 1 => 1100,
            2..=5 => self.rng.range(1, 3),
            6..=8 => self.rng.range(4, 12),
            _ => 0,
        });
        params[Param::Salt as usize - 1] = U256::from(self.rng.below(4));
        params[Param::Loops as usize - 1] = U256::from(*self.rng.pick(&[0u64, 1, 10, 100, 4095]));
        params[Param::OperandA as usize - 1] = self.rng.edge_u256();
        params[Param::OperandB as usize - 1] = self.rng.edge_u256();
        params[Param::Topic as usize - 1] = self.rng.u256();
        params[Param::Lookback as usize - 1] = {
            let small = self.rng.below(300);
            U256::from(*self.rng.pick(&[0u64, 1, 2, 256, 257, small]))
        };
        let beneficiary = if self.rng.chance(30) { target } else { self.recipient() };
        params[Param::Beneficiary as usize - 1] = U256::from_be_slice(beneficiary.as_slice());
        params
    }

    /// A forwarded inner call: target and calldata for a program's forward fragment.
    fn forward(&mut self) -> (Address, U256, Bytes, String) {
        if Native::active(self.ctx.upgrade) && self.rng.chance(60) {
            let call = Native::random(&mut self.rng, self.ctx.upgrade, self.registry);
            return (call.to, call.value, call.input, format!("forward:{}", call.label));
        }
        if !self.registry.programs.is_empty() && self.rng.chance(50) {
            let program =
                self.registry.programs[self.rng.index(self.registry.programs.len())].clone();
            let params = self.program_params(program.address);
            let mask = Program::random_mask(&mut self.rng, program.fragments.len());
            let input = Program::calldata(
                mask & !self.forward_bits(&program.fragments),
                Ending::Return,
                &params,
                &[],
            );
            return (program.address, U256::ZERO, input, "forward:program".into());
        }
        let to = self.recipient();
        (to, U256::ZERO, Bytes::from(self.rng.bytes_between(0, 67)), "forward:any".into())
    }

    fn forward_bits(&self, fragments: &[Fragment]) -> u64 {
        fragments
            .iter()
            .enumerate()
            .filter(|(_, f)| **f == Fragment::Forward)
            .fold(0, |m, (i, _)| m | (1 << i))
    }

    /// A call to a deployed program.
    pub fn program_call(&mut self) -> Option<()> {
        if self.registry.programs.is_empty() {
            return self.deploy_program();
        }
        let program = self.registry.programs[self.rng.index(self.registry.programs.len())].clone();
        let mut params = self.program_params(program.address);
        let mask = Program::random_mask(&mut self.rng, program.fragments.len());
        let ending = match self.rng.below(40) {
            0..=29 => Ending::Stop,
            30..=33 => Ending::Return,
            34..=36 => Ending::Revert,
            37 => Ending::Invalid,
            38 => Ending::SelfDestruct,
            _ => Ending::OutOfGas,
        };
        let mut tail = Vec::new();
        let mut label = "program".to_owned();
        if mask & self.forward_bits(&program.fragments) != 0 {
            let (to, value, input, forward_label) = self.forward();
            params[Param::ForwardTarget as usize - 1] = U256::from_be_slice(to.as_slice());
            params[Param::ForwardValue as usize - 1] = value;
            tail = input.to_vec();
            label = forward_label;
        }
        let input = Program::calldata(mask, ending, &params, &tail);
        let key = self.sender(None)?;
        let value =
            if self.rng.chance(20) { U256::from(self.rng.range(1, 1_000_000)) } else { U256::ZERO };
        let shape = self.shape();
        let auths = if shape == Shape::Eip7702 { self.authorizations(key) } else { Vec::new() };
        let access_list = self.access_list(program.address);
        self.send(
            key,
            shape,
            TxKind::Call(program.address),
            value,
            input,
            access_list,
            auths,
            None,
            true,
            label,
            None,
        )
    }

    /// Generates a program for this era.
    pub fn new_program(&mut self) -> Program {
        let mut targets = vec![WETH, Predeploys::L1_BLOCK_INFO, Predeploys::GAS_PRICE_ORACLE];
        targets.extend(self.registry.programs.iter().take(4).map(|p| p.address));
        targets.extend(self.registry.tokens.iter().take(2).map(|t| t.address));
        targets.push(Keys::get().address(1));
        let ctx = ProgramCtx { upgrade: self.ctx.upgrade, targets };
        Program::generate(&mut self.rng, &ctx)
    }

    /// Deploys a generated program with a creation transaction.
    pub fn deploy_program(&mut self) -> Option<()> {
        let program = self.new_program();
        self.deploy_runtime(
            program.runtime.clone(),
            program.fragments.clone(),
            program.created_runtimes,
            "deploy:program",
        )
    }

    /// Deploys `runtime` from a funded key.
    pub fn deploy_runtime(
        &mut self,
        runtime: Bytes,
        fragments: Vec<Fragment>,
        extra: Vec<Bytes>,
        label: &str,
    ) -> Option<()> {
        let key = self.sender(None)?;
        let sender = Keys::get().address(key);
        let nonce = self.nonce(&sender);
        let init = Asm::deployer(&runtime, self.push0());
        let deploy = PendingDeploy {
            address: sender.create(nonce),
            code_hash: keccak256(&runtime),
            fragments,
        };
        let value =
            if self.rng.chance(10) { U256::from(self.rng.range(1, 1_000_000)) } else { U256::ZERO };
        self.new_codes.push(runtime);
        self.new_codes.extend(extra);
        let shape =
            if self.rng.chance(20) { Shape::Legacy { eip155: true } } else { Shape::Eip1559 };
        let (intrinsic, floor) =
            Txs::intrinsic(self.ctx.upgrade, &init, true, &AccessList::default(), 0);
        let gas = (intrinsic.max(floor) + 200 * init.len() as u64 + 3_000_000).min(self.tx_cap());
        self.send(
            key,
            shape,
            TxKind::Create,
            value,
            init,
            AccessList::default(),
            Vec::new(),
            Some(gas),
            true,
            label.into(),
            Some(deploy),
        )
    }

    /// A direct transaction to a precompile.
    pub fn precompile_call(&mut self) -> Option<()> {
        let call = Precompiles::random(&mut self.rng, self.ctx.upgrade);
        if call.len() > 100_000 {
            return self.program_call();
        }
        let label = format!("precompile:{:#x}", call.address);
        // Now and then far more gas than any input needs, which a failing call consumes.
        let gas = self.rng.chance(5).then_some(1 << 28);
        self.call_gas(None, call.address, U256::ZERO, call.input().into(), label, gas)
    }

    /// A native-precompile call sent from the intended caller.
    pub fn native(&mut self, call: NativeCall) -> Option<()> {
        let label = format!("native:{}", call.label);
        match call.caller {
            Caller::Account(address) if Keys::get().index_of(&address).is_none() => {
                // Only a deposit can act as an address without a key.
                self.deposit_call(address, call.to, call.value, call.input, &label)
            }
            Caller::Account(address) => {
                self.call(Some(address), call.to, call.value, call.input, label)
            }
            Caller::Anyone => self.call(None, call.to, call.value, call.input, label),
        }
    }

    /// A random native-precompile call.
    pub fn native_call(&mut self) -> Option<()> {
        if !Native::active(self.ctx.upgrade) {
            return self.program_call();
        }
        if self.rng.chance(50) {
            let (target, selector, caller, mutation) = (
                self.rng.below(256) as u8,
                self.rng.below(1 << 16) as u16,
                self.rng.below(256) as u8,
                self.rng.below(256) as u8,
            );
            return self.native_abi(target, selector, caller, mutation);
        }
        let call = Native::random(&mut self.rng, self.ctx.upgrade, self.registry);
        self.native(call)
    }

    /// A selector-level native precompile call (see [`NativeAbi`]). `target` and `selector`
    /// pick the precompile family and one function of any of its interface versions (wrapping),
    /// `caller` the sender (the operator, which holds the seeded roles; any key; a known holder
    /// or policy admin; the chain-config activation admin by deposit; any key through the relay
    /// template with a random call scheme; or an address without a key by deposit), and
    /// `mutation` whether the ABI encoding is well formed (half the values) or mutated.
    pub fn native_abi(
        &mut self,
        target: u8,
        selector: u16,
        caller: u8,
        mutation: u8,
    ) -> Option<()> {
        if !Native::active(self.ctx.upgrade) {
            return self.program_call();
        }
        let mut target = NativeTarget::ALL[target as usize % NativeTarget::ALL.len()];
        if !target.active(self.ctx.upgrade) {
            target = NativeTarget::Factory;
        }
        let signatures = target.signatures();
        let signature = signatures[selector as usize % signatures.len()];
        let to = NativeAbi::address(&mut self.rng, self.registry, target);
        let mutation = match mutation % 8 {
            0..=3 => AbiMutation::None,
            4 => AbiMutation::Truncate,
            5 => AbiMutation::Append,
            6 => AbiMutation::Word,
            _ => AbiMutation::SelectorOnly,
        };
        let input = NativeAbi { rng: &mut self.rng, registry: self.registry, to }
            .calldata(signature, mutation);
        let value = if self.rng.chance(4) { U256::from(1) } else { U256::ZERO };
        let name = signature.split('(').next().unwrap_or_default();
        let label = format!("abi:{target:?}.{name}");
        let keys = Keys::get();
        match caller % 6 {
            0 => self.call(Some(keys.operator()), to, value, input, label),
            1 => self.call(None, to, value, input, label),
            2 => {
                let mut known: Vec<Address> =
                    self.registry.policies.iter().map(|p| p.admin).collect();
                for token in &self.registry.tokens {
                    known.extend(token.roles.values().flatten().copied());
                    known.extend(token.holders.iter().take(4).copied());
                }
                let from = if known.is_empty() {
                    keys.operator()
                } else {
                    known[self.rng.index(known.len())]
                };
                let call =
                    NativeCall { to, input, value, caller: Caller::Account(from), label: "" };
                match call.caller {
                    Caller::Account(address) if keys.index_of(&address).is_none() => {
                        self.deposit_call(address, to, value, call.input, &label)
                    }
                    _ => self.call(Some(from), to, value, call.input, label),
                }
            }
            3 => {
                self.deposit_call(MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS, to, value, input, &label)
            }
            4 => {
                let scheme = self.rng.below(4) | if self.rng.chance(50) { 4 } else { 0 };
                let mut relayed = word(U256::from(scheme)).to_vec();
                relayed.extend(word(U256::from_be_slice(to.as_slice())));
                relayed.extend(word(value));
                relayed.extend_from_slice(&input);
                self.call(None, RELAY, value, relayed.into(), format!("relay:{label}"))
            }
            _ => {
                let from = if self.rng.chance(50) { to } else { self.rng.address() };
                self.deposit_call(from, to, value, input, &label)
            }
        }
    }

    /// A call to the recursion template: depth up to and beyond what the gas allows, with each
    /// call forwarding all gas (the 63/64 rule bounds the depth) or a fixed amount, in each call
    /// scheme, optionally storing results and reverting every frame.
    pub fn deep_call(&mut self) -> Option<()> {
        let scheme = self.rng.below(4) | if self.rng.chance(15) { 4 } else { 0 };
        let depth = *self.rng.pick(&[1u64, 2, 3, 8, 16, 100, 127, 128, 300, 600, 1023, 1024, 1100]);
        let gas_each = match self.rng.below(4) {
            0 | 1 => 0,
            2 => self.rng.range(700, 50_000),
            _ => *self.rng.pick(&[2_300u64, 10_000, 1_000_000, u64::MAX >> 1]),
        };
        let store = u64::from(self.rng.chance(30));
        let mut input = Vec::new();
        for value in [scheme, depth, gas_each, store] {
            input.extend(word(U256::from(value)));
        }
        let gas = *self.rng.pick(&[200_000u64, 1_000_000, 5_000_000, 16_000_000, 30_000_000]);
        self.call_gas(None, RECURSER, U256::ZERO, input.into(), "deep".into(), Some(gas))
    }

    /// A call to a creation template: CREATE or CREATE2 of every init code kind, with value,
    /// repeated salts (collisions), padding past the init code limit, and calls into the new
    /// contracts; sometimes from the creator whose nonce is exhausted, and sometimes a creation
    /// transaction of a padded init code instead.
    pub fn create_case(&mut self) -> Option<()> {
        if self.rng.chance(25) {
            return self.create_tx();
        }
        let target = if self.rng.chance(10) { CREATOR_MAX_NONCE } else { CREATOR };
        let scheme = u64::from(self.rng.chance(50));
        let salt = U256::from(self.rng.below(4));
        let value = U256::from(*self.rng.pick(&[0u64, 0, 1, 1_000_000]));
        let count = self.rng.below(4);
        let kind = self.rng.below(16);
        let pad = match self.rng.below(8) {
            0 => 49_152 - self.rng.below(2) * 200,
            1 => 49_153,
            2 => self.rng.range(1, 2_000),
            3 => Templates::padded_len(&mut self.rng, 0) as u64,
            _ => 0,
        };
        let call_after = u64::from(kind >= 12 || self.rng.chance(40));
        let mut input = word(U256::from(scheme)).to_vec();
        input.extend(word(salt));
        input.extend(word(value));
        for value in [count, kind, pad, call_after] {
            input.extend(word(U256::from(value)));
        }
        let gas = *self.rng.pick(&[100_000u64, 500_000, 3_000_000, 10_000_000]);
        let tx_value = if value.is_zero() || self.rng.chance(50) { U256::ZERO } else { value };
        if self.rng.chance(30) {
            // One frame deeper, through the relay; DELEGATECALL and CALLCODE run the creator's
            // code as the relay.
            let scheme = *self.rng.pick(&[0u64, 2, 3]);
            let mut relayed = word(U256::from(scheme)).to_vec();
            relayed.extend(word(U256::from_be_slice(target.as_slice())));
            relayed.extend(word(tx_value));
            relayed.extend(input);
            let label = "create:relay".to_owned();
            return self.call_gas(None, RELAY, tx_value, relayed.into(), label, Some(gas));
        }
        self.call_gas(None, target, tx_value, input.into(), "create".into(), Some(gas))
    }

    /// A creation transaction of an init code kind (see [`Templates::init_code`]) with padding,
    /// so top-level creations of every outcome span the init code size buckets.
    pub fn create_tx(&mut self) -> Option<()> {
        let key = self.sender(None)?;
        let sender = Keys::get().address(key);
        let (mut init, runtime) = {
            let push0 = self.push0();
            Templates::init_code(&mut self.rng, push0, self.ctx.upgrade)
        };
        init.resize(Templates::padded_len(&mut self.rng, init.len()), 0);
        let deploy = runtime.map(|runtime| {
            let deploy = PendingDeploy {
                address: sender.create(self.nonce(&sender)),
                code_hash: keccak256(&runtime),
                fragments: Vec::new(),
            };
            self.new_codes.push(runtime);
            deploy
        });
        let value = self.value(&sender);
        let shape = self.shape();
        self.send(
            key,
            shape,
            TxKind::Create,
            value,
            init.into(),
            AccessList::default(),
            Vec::new(),
            None,
            true,
            "create:tx".into(),
            deploy,
        )
    }

    /// A self-destruct of an existing contract, to itself or to another account, with or
    /// without a balance (EIP-6780 keeps the account), possibly called through the relay with
    /// another scheme.
    pub fn destruct_case(&mut self) -> Option<()> {
        let beneficiary = match self.rng.below(4) {
            0 => Address::ZERO,
            1 => DESTRUCTOR_TO,
            2 => self.recipient(),
            _ => Predeploys::WETH9,
        };
        let value =
            if self.rng.chance(50) { U256::from(self.rng.range(1, 1_000_000)) } else { U256::ZERO };
        let input = word(U256::from_be_slice(beneficiary.as_slice())).to_vec();
        if self.rng.chance(25) {
            let scheme = self.rng.below(4);
            let mut relayed = word(U256::from(scheme)).to_vec();
            relayed.extend(word(U256::from_be_slice(DESTRUCTOR_TO.as_slice())));
            relayed.extend(word(value));
            relayed.extend(input);
            return self.call(None, RELAY, value, relayed.into(), "destruct:relay".into());
        }
        self.call(None, DESTRUCTOR_TO, value, input.into(), "destruct".into())
    }

    /// A call to the refund template: clears, restores, and dirty restores of slots with
    /// non-zero originals, with transient writes and reverts, at gas limits around the refund
    /// cap.
    pub fn refund_case(&mut self) -> Option<()> {
        let mut mode = self.rng.below(32);
        if !self.feature(BaseFeature::BeaconRoots) && self.rng.chance(90) {
            // TSTORE is invalid before Cancun (Ecotone).
            mode &= !8;
        }
        let start = self.rng.below(48);
        let count = self.rng.below(32);
        let mut input = Vec::new();
        for value in [mode, start, count] {
            input.extend(word(U256::from(value)));
        }
        let gas = match self.rng.below(3) {
            0 => None,
            1 => Some(self.rng.range(30_000, 200_000)),
            _ => Some(self.rng.range(200_000, 2_000_000)),
        };
        self.call_gas(None, REFUNDER, U256::ZERO, input.into(), "refund".into(), gas)
    }

    /// Calls into system contracts.
    pub fn system_call(&mut self) -> Option<()> {
        let key = self.sender(None)?;
        let sender = Keys::get().address(key);
        let (to, value, input, label): (Address, U256, Vec<u8>, &str) = match self.rng.below(12) {
            0 => (
                WETH,
                U256::from(self.rng.range(1, 1 << 50)),
                selector("deposit()"),
                "weth.deposit",
            ),
            1 => {
                let mut input = selector("withdraw(uint256)");
                // Below most seeded WETH balances (see `Genesis::build`), sometimes above.
                let bound = if self.rng.chance(85) { 1 << 40 } else { 1 << 52 };
                input.extend(word(U256::from(self.rng.range(0, bound))));
                (WETH, U256::ZERO, input, "weth.withdraw")
            }
            2 | 3 => {
                let mut input = selector("initiateWithdrawal(address,uint256,bytes)");
                input.extend(word(U256::from_be_slice(self.recipient().as_slice())));
                input.extend(word(U256::from(self.rng.range(0, 1_000_000))));
                input.extend(word(U256::from(96)));
                let data = self.rng.bytes_between(0, 79);
                input.extend(word(U256::from(data.len())));
                let mut padded = data.clone();
                padded.resize(data.len().div_ceil(32) * 32, 0);
                input.extend(padded);
                (
                    Predeploys::L2_TO_L1_MESSAGE_PASSER,
                    U256::from(self.rng.range(0, 1 << 40)),
                    input,
                    "messagePasser.initiateWithdrawal",
                )
            }
            4 => {
                let vault = *self.rng.pick(&[
                    Predeploys::SEQUENCER_FEE_VAULT,
                    Predeploys::BASE_FEE_VAULT,
                    Predeploys::L1_FEE_VAULT,
                    Predeploys::OPERATOR_FEE_VAULT,
                ]);
                (vault, U256::ZERO, selector("withdraw()"), "feeVault.withdraw")
            }
            5 => {
                let data = self.rng.bytes_between(0, 299);
                let sig = *self.rng.pick(&["getL1Fee(bytes)", "getL1GasUsed(bytes)"]);
                let mut input = selector(sig);
                input.extend(word(U256::from(32)));
                input.extend(word(U256::from(data.len())));
                let mut padded = data.clone();
                padded.resize(data.len().div_ceil(32) * 32, 0);
                input.extend(padded);
                (Predeploys::GAS_PRICE_ORACLE, U256::ZERO, input, "gpo.getL1Fee")
            }
            6 => {
                let sig = *self.rng.pick(&[
                    "getOperatorFee(uint256)",
                    "getL1FeeUpperBound(uint256)",
                    "baseFeeScalar()",
                    "blobBaseFee()",
                    "l1BaseFee()",
                    "isJovian()",
                ]);
                let mut input = selector(sig);
                input.extend(word(U256::from(self.rng.range(0, 1_000_000))));
                (Predeploys::GAS_PRICE_ORACLE, U256::ZERO, input, "gpo.view")
            }
            7 if self.feature(BaseFeature::Create2Deployer) => {
                // create2deployer.deploy(value, salt, code)
                let runtime = Templates::logger(self.push0());
                let init = Asm::deployer(&runtime, self.push0()).to_vec();
                self.new_codes.push(runtime);
                let mut input = selector("deploy(uint256,bytes32,bytes)");
                input.extend(word(U256::ZERO));
                input.extend(word(U256::from(self.rng.below(3))));
                input.extend(word(U256::from(96)));
                input.extend(word(U256::from(init.len())));
                let mut padded = init.clone();
                padded.resize(init.len().div_ceil(32) * 32, 0);
                input.extend(padded);
                (CREATE2_DEPLOYER, U256::ZERO, input, "create2deployer.deploy")
            }
            8 if self.feature(BaseFeature::SubsecondBlocks) => {
                let sig = *self.rng.pick(&[
                    "timestampMs()",
                    "timestampMillisPart()",
                    "setTimestampMillisPart(uint16)",
                ]);
                let mut input = selector(sig);
                input.extend(word(U256::from(200)));
                (Predeploys::BASE_TIME, U256::ZERO, input, "baseTime.call")
            }
            9 => {
                let sig = *self.rng.pick(&[
                    "number()",
                    "timestamp()",
                    "basefee()",
                    "hash()",
                    "blobBaseFee()",
                    "operatorFeeScalar()",
                    "setL1BlockValuesEcotone()",
                ]);
                (Predeploys::L1_BLOCK_INFO, U256::ZERO, selector(sig), "l1block.call")
            }
            10 => {
                // EIP-4788 / EIP-2935 direct reads.
                let (to, key) = if self.rng.chance(50) {
                    (
                        Predeploys::BEACON_BLOCK_ROOT,
                        U256::from(self.ctx.timestamp.saturating_sub(self.rng.below(30))),
                    )
                } else {
                    (HISTORY, U256::from(self.ctx.number.saturating_sub(self.rng.range(1, 300))))
                };
                (to, U256::ZERO, word(key).to_vec(), "system.historyRead")
            }
            _ => {
                let mut input = selector("sendMessage(address,bytes,uint32)");
                input.extend(word(U256::from_be_slice(self.recipient().as_slice())));
                input.extend(word(U256::from(96)));
                input.extend(word(U256::from(self.rng.range(0, 500_000))));
                input.extend(word(U256::ZERO));
                (
                    Predeploys::L2_CROSS_DOMAIN_MESSENGER,
                    U256::from(self.rng.below(1 << 30)),
                    input,
                    "messenger.sendMessage",
                )
            }
        };
        let value = value.min(self.available(&sender) / U256::from(4));
        let shape = self.shape();
        let shape = if shape == Shape::Eip7702 { Shape::Eip1559 } else { shape };
        self.send(
            key,
            shape,
            TxKind::Call(to),
            value,
            input.into(),
            AccessList::default(),
            Vec::new(),
            None,
            true,
            label.into(),
            None,
        )
    }

    // ── EIP-7702 ───────────────────────────────────────────────────────────

    /// A delegate for an authorization.
    fn delegate(&mut self) -> Address {
        match self.rng.below(10) {
            0..=4 if !self.registry.programs.is_empty() => {
                self.registry.programs[self.rng.index(self.registry.programs.len())].address
            }
            5 if !self.registry.templates.is_empty() => {
                *self.registry.templates.iter().next().expect("non-empty")
            }
            6 => Address::ZERO,
            7 => *self.rng.pick(&Precompiles::active(self.ctx.upgrade)),
            8 => {
                // Delegate to another delegated account: a delegation chain.
                let delegations = Registry::delegations(self.state);
                if delegations.is_empty() {
                    WETH
                } else {
                    delegations[self.rng.index(delegations.len())].0
                }
            }
            _ => WETH,
        }
    }

    /// Authorizations for a type-4 transaction sent by key `sender_key`.
    pub fn authorizations(
        &mut self,
        sender_key: usize,
    ) -> Vec<alloy_eips::eip7702::SignedAuthorization> {
        let sender = Keys::get().address(sender_key);
        let mut auths = Vec::new();
        for _ in 0..self.rng.range(1, 3) {
            let key = if self.rng.chance(25) { sender_key } else { self.rng.index(Keys::COUNT) };
            let authority = Keys::get().address(key);
            let mut nonce =
                self.auth_nonces.get(&authority).copied().unwrap_or_else(|| self.nonce(&authority));
            if authority == sender {
                // The sender's nonce is bumped before authorizations are processed.
                nonce = nonce.saturating_add(1);
            }
            let valid = self.rng.chance(80);
            let (chain_id, nonce) = if valid {
                (if self.rng.chance(20) { 0 } else { 8453 }, nonce)
            } else {
                match self.rng.below(3) {
                    0 => (1, nonce),
                    1 => (8453, nonce.saturating_add(1)),
                    _ => (8453, nonce.saturating_sub(1)),
                }
            };
            let delegate = self.delegate();
            if !delegate.is_zero() {
                let mut designator = vec![0xef, 0x01, 0x00];
                designator.extend_from_slice(delegate.as_slice());
                self.new_codes.push(designator.into());
            }
            auths.push(Txs::authorization(key, chain_id, delegate, nonce));
            if valid && !self.has_code(&authority) {
                if authority == sender {
                    // Applied after the transaction's own nonce increment.
                    self.self_bump = true;
                } else {
                    self.auth_nonces.insert(authority, nonce.saturating_add(1));
                }
            }
        }
        auths
    }

    /// An EIP-7702 transaction that installs delegations and calls through them.
    pub fn delegation(&mut self) -> Option<()> {
        if !self.feature(BaseFeature::Eip7702) {
            return self.transfer();
        }
        let key = self.sender(None)?;
        let auths = self.authorizations(key);
        // Call an authority (running its new delegated code) or a delegated account.
        let to = if self.rng.chance(60) {
            auths[self.rng.index(auths.len())]
                .recover_authority()
                .unwrap_or_else(|_| self.recipient())
        } else {
            self.recipient()
        };
        let params = self.program_params(to);
        let input = Program::calldata(self.rng.next_u64(), Ending::Stop, &params, &[]);
        self.send(
            key,
            Shape::Eip7702,
            TxKind::Call(to),
            U256::ZERO,
            input,
            AccessList::default(),
            auths,
            None,
            true,
            "eip7702".into(),
            None,
        )
    }

    // ── Deposits ───────────────────────────────────────────────────────────

    fn deposit_gas(&mut self, input: &[u8], create: bool) -> u64 {
        let (intrinsic, _) =
            Txs::intrinsic(self.ctx.upgrade, input, create, &AccessList::default(), 0);
        let gas = match self.rng.below(10) {
            0 => intrinsic.saturating_sub(self.rng.range(1, 1000)),
            1 => intrinsic,
            2 => 21_000,
            _ => intrinsic + *self.rng.pick(&[50_000u64, 300_000, 1_000_000, 3_000_000]),
        };
        gas.min(self.gas_left)
    }

    /// Adds a deposit, charging its gas to the block and bumping its sender's nonce.
    pub fn push_deposit(
        &mut self,
        deposit: TxDeposit,
        label: String,
        pending: Option<PendingDeploy>,
    ) -> Option<()> {
        if deposit.gas_limit > self.gas_left || deposit.gas_limit == 0 {
            return None;
        }
        self.gas_left -= deposit.gas_limit;
        self.deposits += 1;
        // From Regolith every deposit increments its sender's nonce, even when it fails; a
        // system-flagged deposit is invalid from Regolith and never included.
        if !deposit.is_system_transaction {
            let nonce = self.nonce(&deposit.from);
            self.nonces.insert(deposit.from, nonce.saturating_add(1));
        }
        self.out.push(PlannedTx { raw: Txs::deposit(deposit), label, deploy: pending });
        Some(())
    }

    /// A deposit acting as `from` (any address, including ones without keys).
    pub fn deposit_call(
        &mut self,
        from: Address,
        to: Address,
        value: U256,
        input: Bytes,
        label: &str,
    ) -> Option<()> {
        let gas_limit =
            (Txs::intrinsic(self.ctx.upgrade, &input, false, &AccessList::default(), 0).0
                + 1_000_000)
                .min(self.gas_left);
        let mint =
            if value > self.state.balance(&from) { value.saturating_to::<u128>() } else { 0 };
        let deposit = TxDeposit {
            source_hash: self.ctx.origin.deposit_source(1000 + self.deposits),
            from,
            to: TxKind::Call(to),
            mint,
            value,
            gas_limit,
            is_system_transaction: false,
            input,
        };
        self.push_deposit(deposit, label.to_owned(), None)
    }

    /// A random user deposit: mints, transfers, calls, creations, and failures.
    pub fn deposit(&mut self) -> Option<()> {
        let keys = Keys::get();
        let from = match self.rng.below(8) {
            0..=3 => keys.address(self.rng.index(Keys::COUNT)),
            4 => self.rng.address(),
            5 if !self.registry.programs.is_empty() => self.registry.programs[0].address,
            6 => MAINNET_BERYL_ACTIVATION_ADMIN_ADDRESS,
            _ => keys.address(self.rng.range(1, 4) as usize),
        };
        let create = self.rng.chance(12)
            && (self.max_nonce_deposit_creates || from != keys.address(Keys::MAX_NONCE));
        // A creation whose address is already taken.
        let from = if create && self.rng.chance(15) && self.nonce(&COLLIDER) < COLLIDER_NONCES {
            COLLIDER
        } else {
            from
        };
        let mint: u128 = match self.rng.below(6) {
            0 => 0,
            1 => 1,
            2 => 10u128.pow(24),
            _ => u128::from(self.rng.next_u64()),
        };
        let balance = self.state.balance(&from).saturating_add(U256::from(mint));
        let value = match self.rng.below(8) {
            0 => balance + U256::from(1),
            1 => U256::from(mint),
            2..=4 => U256::ZERO,
            _ => balance / U256::from(self.rng.range(2, 100)),
        };
        let (to, input, pending) = if create {
            let (mut init, runtime) = {
                let push0 = self.push0();
                Templates::init_code(&mut self.rng, push0, self.ctx.upgrade)
            };
            init.resize(Templates::padded_len(&mut self.rng, init.len()), 0);
            let pending = runtime.as_ref().map(|runtime| PendingDeploy {
                address: from.create(self.nonce(&from)),
                code_hash: keccak256(runtime),
                fragments: Vec::new(),
            });
            if let Some(runtime) = runtime {
                self.new_codes.push(runtime);
            }
            (TxKind::Create, Bytes::from(init), pending)
        } else {
            let to = self.recipient();
            let input = if self.registry.programs.iter().any(|p| p.address == to) {
                let params = self.program_params(to);
                Program::calldata(self.rng.next_u64(), Ending::Stop, &params, &[])
            } else if Native::active(self.ctx.upgrade) && self.rng.chance(20) {
                Native::random(&mut self.rng, self.ctx.upgrade, self.registry).input
            } else {
                Bytes::from(self.rng.bytes_between(0, 63))
            };
            (TxKind::Call(to), input, None)
        };
        let gas_limit = self.deposit_gas(&input, create);
        let deposit = TxDeposit {
            source_hash: self.ctx.origin.deposit_source(self.deposits),
            from,
            to,
            mint,
            value,
            gas_limit,
            // Derivation never produces flagged user deposits after Regolith, so clients may
            // treat them differently without a consensus difference; never generate them.
            is_system_transaction: false,
            input,
        };
        self.push_deposit(
            deposit,
            if create { "deposit:create".into() } else { "deposit".into() },
            pending,
        )
    }

    // ── Invalid transactions ──────────────────────────────────────────────

    /// A transaction that fails validity checks.
    pub fn invalid(&mut self) -> Option<()> {
        let key = self.sender(None)?;
        let sender = Keys::get().address(key);
        let nonce = self.nonce(&sender);
        let (max_fee, priority_fee) = self.fees();
        let to = self.recipient();
        // Most validation errors are checked for every transaction type, with type-specific
        // messages, so each kind is sent in a random shape.
        let shape = match self.rng.below(7) {
            0 => Shape::Legacy { eip155: false },
            1 => Shape::Legacy { eip155: true },
            2 => Shape::Eip2930,
            3 | 4 => Shape::Eip1559,
            5 => Shape::Eip7702,
            _ => Shape::Eip4844,
        };
        let authorizations = if shape == Shape::Eip7702 {
            // Some invalid kinds are still included, which applies the delegation, so the
            // designator must be a known code for the post state.
            let mut designator = vec![0xef, 0x01, 0x00];
            designator.extend_from_slice(WETH.as_slice());
            self.new_codes.push(designator.into());
            vec![Txs::authorization(self.rng.index(Keys::COUNT), 8453, WETH, 0)]
        } else {
            Vec::new()
        };
        let gas = if shape == Shape::Eip7702 { 60_000 } else { 21_000 };
        let mut spec = TxSpec {
            key,
            shape,
            chain_id: 8453,
            nonce,
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: gas,
            max_fee,
            priority_fee,
            access_list: AccessList::default(),
            authorizations,
            high_s: false,
        };
        let label = match self.rng.below(22) {
            0 => {
                spec.nonce = nonce.saturating_add(self.rng.range(1, 5));
                "invalid:nonceGap"
            }
            1 if nonce > 0 => {
                spec.nonce = nonce - 1;
                "invalid:nonceReplay"
            }
            2 => {
                spec.value = self.state.balance(&sender) + U256::from(1);
                "invalid:insufficientBalance"
            }
            3 => {
                spec.gas_limit = self.ctx.gas_limit + 1;
                "invalid:gasAboveBlock"
            }
            4 => {
                spec.gas_limit = (1 << 24) + 1;
                "invalid:gasAboveCap"
            }
            5 => {
                spec.gas_limit = gas - 1;
                "invalid:intrinsicTooLow"
            }
            6 => {
                spec.input = Bytes::from(vec![1u8; 2000]);
                let (intrinsic, floor) = Txs::intrinsic(
                    self.ctx.upgrade,
                    &spec.input,
                    false,
                    &AccessList::default(),
                    spec.authorizations.len(),
                );
                spec.gas_limit = intrinsic.max(floor) - 1;
                "invalid:floorTooLow"
            }
            7 => {
                spec.chain_id = *self.rng.pick(&[1u64, 84532, 0]);
                "invalid:chainId"
            }
            8 => {
                spec.max_fee = u128::from(self.ctx.base_fee.saturating_sub(1));
                spec.priority_fee = 0;
                "invalid:feeBelowBase"
            }
            9 => {
                spec.priority_fee = spec.max_fee + 1;
                "invalid:tipAboveMax"
            }
            10 => {
                spec.shape = Shape::Eip1559;
                spec.authorizations.clear();
                spec.to = TxKind::Create;
                spec.input = Bytes::from(vec![0u8; 49_153]);
                spec.gas_limit = 5_000_000.min(self.ctx.gas_limit);
                "invalid:initcodeTooLarge"
            }
            11 => {
                spec.high_s = true;
                "invalid:highS"
            }
            12 => {
                spec.shape = Shape::Eip7702;
                spec.authorizations = Vec::new();
                spec.gas_limit = 50_000;
                "invalid:emptyAuthorizationList"
            }
            13 => {
                // The account nonce is already at the maximum (EIP-2681).
                spec.key = Keys::MAX_NONCE;
                spec.nonce = u64::MAX;
                "invalid:nonceOverflow"
            }
            14 => {
                // The sender has code that is not a delegation (EIP-3607).
                spec.key = Keys::WITH_CODE;
                spec.nonce = self.state.nonce(&Keys::get().address(Keys::WITH_CODE));
                "invalid:senderHasCode"
            }
            15 => {
                // gas_limit * max_fee overflows.
                spec.max_fee = u128::MAX;
                spec.priority_fee = self.rng.below(2).into();
                "invalid:feeOverflow"
            }
            16 => {
                // Exactly enough for gas and value, not for the L1 data and operator fees.
                spec.input = Bytes::from(self.rng.bytes_between(100, 400));
                let (intrinsic, floor) = Txs::intrinsic(
                    self.ctx.upgrade,
                    &spec.input,
                    false,
                    &AccessList::default(),
                    spec.authorizations.len(),
                );
                spec.gas_limit = intrinsic.max(floor) + 1_000;
                let gas_cost = U256::from(spec.gas_limit) * U256::from(spec.max_fee);
                spec.value = self.state.balance(&sender).saturating_sub(gas_cost);
                "invalid:l1CostUnaffordable"
            }
            17 if self.gas_left < self.ctx.gas_limit => {
                spec.gas_limit = self.gas_left + 1;
                "invalid:gasAboveRemaining"
            }
            18 => {
                // Not a transaction at all, or a truncated one.
                let mut raw = Txs::sign(&spec).to_vec();
                let label = match self.rng.below(3) {
                    0 => {
                        raw.truncate(self.rng.index(raw.len()));
                        "invalid:truncated"
                    }
                    1 => {
                        raw[0] = *self.rng.pick(&[0x05u8, 0x7d, 0x7f, 0xc0]);
                        "invalid:unknownType"
                    }
                    _ => {
                        raw.push(0);
                        "invalid:trailingBytes"
                    }
                };
                self.out.push(PlannedTx { raw: raw.into(), label: label.into(), deploy: None });
                return Some(());
            }
            19 => {
                // A system-flagged deposit is invalid from Regolith.
                let deposit = TxDeposit {
                    source_hash: self.ctx.origin.deposit_source(5000 + self.deposits),
                    from: sender,
                    to: TxKind::Call(to),
                    mint: 0,
                    value: U256::ZERO,
                    gas_limit: 100_000,
                    is_system_transaction: true,
                    input: Bytes::new(),
                };
                self.out.push(PlannedTx {
                    raw: Txs::deposit(deposit),
                    label: "invalid:systemDeposit".into(),
                    deploy: None,
                });
                return Some(());
            }
            20 => {
                spec.shape = Shape::Eip4844;
                spec.gas_limit = 50_000;
                "invalid:blobTx"
            }
            _ => {
                // An access list or authorizations that push intrinsic gas above the limit.
                spec.access_list =
                    Txs::access_list(&[to, WETH], &[B256::ZERO, B256::repeat_byte(1)]);
                spec.gas_limit = 21_000 + 2_000;
                "invalid:accessListIntrinsic"
            }
        };
        self.out.push(PlannedTx { raw: Txs::sign(&spec), label: label.into(), deploy: None });
        Some(())
    }

    /// One random transaction, drawn from the traffic mix.
    pub fn random(&mut self, invalid_percent: u64) -> Option<()> {
        if self.rng.chance(invalid_percent) {
            return self.invalid();
        }
        let native = if Native::active(self.ctx.upgrade) { Native::weight() } else { 0 };
        let deposits = if self.ctx.deposits_allowed { 7 } else { 0 };
        let base_tx = if self.feature(BaseFeature::TxBase) { 25 } else { 0 };
        let weights = [10, 30, 6, 10, native, 9, 7, deposits, 3, 3, 2, 2, base_tx];
        match self.rng.weighted(&weights) {
            0 => self.transfer(),
            1 => self.program_call(),
            2 => self.deploy_program(),
            3 => self.precompile_call(),
            4 => self.native_call(),
            5 => self.system_call(),
            6 => self.delegation(),
            7 => self.deposit(),
            8 => self.deep_call(),
            9 => self.create_case(),
            10 => self.destruct_case(),
            11 => self.refund_case(),
            _ => self.base_tx(),
        }
    }

    /// One call of a Base transaction phase: a program, native or arbitrary call, a value
    /// transfer (often to a fresh account), or a read of the transaction context or the nonce
    /// manager.
    fn base_call(&mut self, sender: Address, nonce_key: U256) -> (Call, &'static str) {
        let selector = |s: &str| Bytes::from(selector(s));
        match self.rng.below(10) {
            0..=3 => {
                let (to, value, data, _) = self.forward();
                (Call { to, value, data }, "call")
            }
            4 | 5 => {
                let to = if self.rng.chance(50) { self.rng.address() } else { self.recipient() };
                let value = self.value(&sender);
                (Call { to, value, data: Bytes::new() }, "transfer")
            }
            6 => {
                let getter = *self.rng.pick(&[
                    "getTransactionSender()",
                    "getTransactionPayer()",
                    "getTransactionSenderActorId()",
                ]);
                let to = address!("813000000000000000000000000000000000aa02");
                (Call { to, value: U256::ZERO, data: selector(getter) }, "txContext")
            }
            7 => {
                let mut data = selector("getNonce(address,uint256)").to_vec();
                let account = if self.rng.chance(80) { sender } else { self.recipient() };
                data.extend_from_slice(B256::left_padding_from(account.as_slice()).as_slice());
                let key = match self.rng.below(4) {
                    0 => U256::ZERO,
                    1 => TxBaseConstants::NONCE_KEY_MAX,
                    _ => nonce_key,
                };
                data.extend_from_slice(&word(key));
                let value = if self.rng.chance(5) { U256::from(1) } else { U256::ZERO };
                (Call { to: NonceManagerSlots::ADDRESS, value, data: data.into() }, "getNonce")
            }
            8 => {
                let to = *self.rng.pick(&Precompiles::active(self.ctx.upgrade));
                (
                    Call { to, value: U256::ZERO, data: self.rng.bytes_between(0, 200).into() },
                    "precompile",
                )
            }
            _ => (Call { to: sender, value: self.value(&sender), data: Bytes::new() }, "self"),
        }
    }

    /// A Base transaction (type `0x79`, Everest): protocol, 2D or nonce-free nonce; recovered
    /// or named sender; self, named or open payer; an optional delegation change; and up to
    /// four phases of calls.
    pub fn base_tx(&mut self) -> Option<()> {
        let keys = Keys::get();
        let key = self.sender(None)?;
        let sender = keys.address(key);
        let (nonce_key, nonce_sequence, mode) = match self.rng.below(10) {
            0..=4 => (U256::ZERO, self.nonce(&sender), "protocol"),
            5..=7 => {
                let nonce_key = U256::from(self.rng.range(1, 4));
                let stored = NonceManagerSlots::nonce_slot(sender, nonce_key)
                    .map(|slot| self.state.storage(&NonceManagerSlots::ADDRESS, slot.into()))
                    .unwrap_or_default();
                let next = self
                    .base_nonces
                    .get(&(sender, nonce_key))
                    .copied()
                    .unwrap_or_else(|| u64::try_from(stored).unwrap_or(u64::MAX));
                (nonce_key, next, "2d")
            }
            _ => (TxBaseConstants::NONCE_KEY_MAX, 0, "nonceFree"),
        };
        // Nonce-free transactions must expire within the replay ring's window after the
        // block's millisecond time; the millisecond part is below 1000.
        let valid_before = if mode == "nonceFree" {
            self.ctx.timestamp * 1000 + self.rng.range(1_000, 20_001)
        } else if self.rng.chance(10) {
            self.ctx.timestamp + self.rng.range(1, 60)
        } else {
            0
        };
        let valid_after = if self.rng.chance(10) {
            self.ctx.timestamp.saturating_sub(self.rng.below(5))
        } else {
            0
        };
        let payer_key = match self.rng.below(10) {
            0..=5 => None,
            _ => self.sender(None).filter(|k| *k != key),
        };
        let open_payer = payer_key.is_some() && self.rng.chance(40);
        let payer = payer_key
            .map(|k| if open_payer { TxBaseConstants::OPEN_PAYER } else { keys.address(k) });
        let named_sender = self.rng.chance(30);
        // A delegation change needs the protocol nonce; one in ten tries another nonce anyway.
        let delegation_ok = mode == "protocol" || self.rng.chance(10);
        let account_changes = if delegation_ok && self.rng.chance(15) {
            vec![AccountChange::Delegation(Delegation { target: self.delegate() })]
        } else {
            Vec::new()
        };
        let mut labels = vec![mode];
        let mut calls = Vec::new();
        for _ in 0..self.rng.below(5) {
            let mut phase = Vec::new();
            for _ in 0..self.rng.range(1, 4) {
                let (call, label) = self.base_call(sender, nonce_key);
                labels.push(label);
                phase.push(call);
            }
            calls.push(phase);
        }
        let value: U256 = calls.iter().flatten().map(|c| c.value).sum();
        let (max_fee, priority_fee) = self.fees();
        let metadata = if self.rng.chance(15) {
            Bytes::from(self.rng.bytes_between(1, 64))
        } else {
            Bytes::new()
        };
        let mut tx = TxBase {
            chain_id: 8453,
            sender: named_sender.then_some(sender),
            nonce_key,
            nonce_sequence,
            valid_after,
            valid_before,
            max_priority_fee_per_gas: priority_fee,
            max_fee_per_gas: max_fee,
            gas_limit: 0,
            account_changes,
            calls,
            metadata,
            payer,
        };
        let sign = |tx: TxBase| -> TxBaseSigned {
            let signature = keys.signers[key]
                .sign_hash_sync(&tx.sender_signature_hash())
                .expect("signing never fails");
            let mut sender_auth = Vec::new();
            if named_sender {
                sender_auth.extend_from_slice(TxBaseConstants::K1_AUTHENTICATOR.as_slice());
            }
            sender_auth.extend_from_slice(&signature.as_bytes());
            let mut payer_auth = Vec::new();
            if let Some(payer_key) = payer_key {
                let signature = keys.signers[payer_key]
                    .sign_hash_sync(&tx.payer_signature_hash(sender))
                    .expect("signing never fails");
                if !open_payer {
                    payer_auth.extend_from_slice(TxBaseConstants::K1_AUTHENTICATOR.as_slice());
                }
                payer_auth.extend_from_slice(&signature.as_bytes());
            }
            TxBaseSigned::new(tx, sender_auth.into(), payer_auth.into())
        };
        let intrinsic = IntrinsicGas::compute(&sign(tx.clone()), sender);
        let gas_limit = self.gas_for(intrinsic.sender_intrinsic(), intrinsic.sender_floor(), true);
        if gas_limit > self.gas_left {
            return None;
        }
        tx.gas_limit = gas_limit;
        let fee = U256::from(gas_limit + intrinsic.payer_auth_cost) * U256::from(max_fee);
        let fee_payer = payer_key.map_or(sender, |k| keys.address(k));
        if fee + if fee_payer == sender { value } else { U256::ZERO } > self.available(&fee_payer)
            || value > self.available(&sender)
        {
            return None;
        }
        let signed = sign(tx);
        self.out.push(PlannedTx {
            raw: signed.encoded_2718().into(),
            label: format!("baseTx:{}", labels.join(":")),
            deploy: None,
        });
        match mode {
            "protocol" => {
                self.nonces.insert(sender, nonce_sequence.saturating_add(1));
            }
            "2d" => {
                self.base_nonces.insert((sender, nonce_key), nonce_sequence.saturating_add(1));
            }
            _ => {}
        }
        *self.spent.entry(fee_payer).or_default() += fee;
        *self.spent.entry(sender).or_default() += value;
        self.gas_left -= gas_limit;
        Some(())
    }

    /// Creates a well-formed token from the operator, grants it the mint role, and mints to
    /// `holders`, all in this block (for single-block cases).
    pub fn token_bootstrap(
        &mut self,
        salt: B256,
        variant: B20Variant,
        holders: &[Address],
    ) -> Option<()> {
        let operator = Keys::get().operator();
        let input = Native::seeded_token_input(salt, variant, holders, &[]);
        self.call(
            Some(operator),
            B20FactoryStorage::ADDRESS,
            U256::ZERO,
            input,
            "native:factory.createB20".into(),
        )
    }
}
