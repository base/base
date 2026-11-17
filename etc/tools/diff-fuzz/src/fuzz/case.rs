//! Fuzz case descriptors: generator inputs plus mutations that regenerate a well-formed request.

use alloy_consensus::{Signed, transaction::SignerRecoverable};
use alloy_eips::Decodable2718;
use alloy_primitives::{Bytes, TxKind, U256, keccak256};
use base_common_consensus::{BaseTxEnvelope, TxDeposit};
use serde::{Deserialize, Serialize};

use crate::{
    Ending, Generator, GeneratorOptions, Keys, PlannedTx, Planner, Program, Rng, Shape, Template,
    TxSpec, Txs,
};

/// A fuzz case: the pre-state of single-block case `(seed, index)` and a list of transaction
/// slots planned against it.
///
/// The request is rebuilt from this descriptor by the generator, so the witness, nonces,
/// balances, and code hashes always match the transactions: mutating a descriptor can only
/// produce a well-formed request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FuzzCase {
    /// Generator seed.
    pub seed: u64,
    /// Generator case index; selects the fork, block, genesis, system config, and L1 origin.
    pub index: u64,
    /// Transactions, in planning order.
    pub slots: Vec<Slot>,
    /// Edits to the L1 attributes deposit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub l1: Vec<L1Tweak>,
}

/// One planned transaction (or a small group, for [`SlotKind::Mutant`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Slot {
    /// What to plan.
    pub kind: SlotKind,
    /// Seed of the slot's own random stream. Re-rolling it re-plans only this slot.
    pub salt: u64,
    /// Edits to the planned transactions, applied in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tweaks: Vec<Tweak>,
}

/// Which planner action a slot runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotKind {
    /// A draw from the generator's traffic mix.
    Random,
    /// A value transfer.
    Transfer,
    /// A call to a deployed program.
    ProgramCall,
    /// A program deployment.
    DeployProgram,
    /// A direct precompile call.
    Precompile,
    /// A native precompile call.
    Native,
    /// A system contract interaction.
    System,
    /// An EIP-7702 delegation.
    Delegation,
    /// A user deposit (planned only in blocks that may carry deposits).
    Deposit,
    /// A deliberately invalid transaction.
    Invalid,
    /// A selector-level native precompile call; each field indexes its option list (wrapping),
    /// see `Planner::native_abi`.
    NativeAbi {
        /// Precompile family.
        target: u8,
        /// Function, over every interface version of the family.
        selector: u16,
        /// Sender.
        caller: u8,
        /// ABI encoding mutation.
        mutation: u8,
    },
    /// A call into the recursion template.
    Deep,
    /// A call into a creation template.
    Create,
    /// A self-destruct of an existing contract.
    Destruct,
    /// A call into the refund template.
    Refund,
    /// Deploys a program with edited bytecode and calls it.
    Mutant {
        /// 0 for a freshly generated program, otherwise a known program (index `source - 1`,
        /// modulo the number of known programs) whose code is copied.
        source: u8,
        /// Bytecode edits.
        edits: Vec<CodeEdit>,
        /// Calls to make to the deployed copy.
        calls: u8,
    },
}

/// An edit to program bytecode. Positions wrap around the code length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodeEdit {
    /// Overwrites one byte.
    Set {
        /// Position.
        pos: u32,
        /// New byte.
        byte: u8,
    },
    /// Inserts bytes.
    Insert {
        /// Position.
        pos: u32,
        /// Bytes to insert.
        bytes: Bytes,
    },
    /// Deletes bytes.
    Delete {
        /// Position.
        pos: u32,
        /// Number of bytes.
        len: u8,
    },
}

/// An edit to one of a slot's planned transactions, which is then re-signed (or re-encoded for
/// deposits). `tx` wraps around the number of transactions the slot planned.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tweak {
    /// Changes the gas limit.
    Gas {
        /// Transaction within the slot.
        tx: u8,
        /// New gas limit.
        to: GasTarget,
    },
    /// Sets the value.
    Value {
        /// Transaction within the slot.
        tx: u8,
        /// New value.
        value: U256,
    },
    /// Edits the calldata.
    Calldata {
        /// Transaction within the slot.
        tx: u8,
        /// The edit.
        edit: ByteEdit,
    },
    /// Moves the fee caps to a boundary.
    Fees {
        /// Transaction within the slot.
        tx: u8,
        /// New fee caps.
        to: FeeTarget,
    },
    /// Sets a deposit's mint (deposits only).
    Mint {
        /// Transaction within the slot.
        tx: u8,
        /// New mint.
        mint: u128,
    },
}

/// A gas limit relative to the transaction's intrinsic cost or its current limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GasTarget {
    /// The intrinsic gas (or calldata floor) plus this much.
    IntrinsicPlus(u64),
    /// The current limit minus this much.
    Minus(u64),
    /// The current limit scaled by this many thousandths.
    PerMille(u16),
    /// Exactly this limit.
    Exact(u64),
}

/// Fee caps at a base fee boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FeeTarget {
    /// Max fee equal to the block's base fee, no tip.
    AtBaseFee,
    /// Max fee one below the block's base fee.
    BelowBaseFee,
    /// Tip equal to the max fee.
    TipIsMax,
    /// Tip above the max fee.
    TipAboveMax,
    /// A very large max fee.
    Huge,
}

/// An edit to calldata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ByteEdit {
    /// Writes a 32-byte word at a byte offset (wrapping), extending the input if needed.
    Word {
        /// Byte offset.
        at: u32,
        /// Word.
        word: U256,
    },
    /// Flips one bit.
    Flip {
        /// Byte position.
        pos: u32,
        /// Bit.
        bit: u8,
    },
    /// Truncates to this many bytes (wrapping).
    Truncate {
        /// New length.
        len: u32,
    },
    /// Appends bytes.
    Append {
        /// Bytes.
        bytes: Bytes,
    },
}

/// An edit to one field of the L1 attributes deposit's calldata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct L1Tweak {
    /// Field index (wrapping) in the calldata layout of the block's fork.
    pub field: u8,
    /// New value, truncated to the field width.
    pub value: U256,
}

impl FuzzCase {
    /// Generator options for fuzz cases: the generator plans no traffic of its own (only system
    /// transactions and its scenario setup), so slots are the only traffic.
    pub fn generator_options(options: &GeneratorOptions) -> GeneratorOptions {
        GeneratorOptions { min_txs: 0, max_txs: 0, ..options.clone() }
    }

    /// A fresh case: `count` random slots on the pre-state of `(seed, index)`.
    pub fn fresh(seed: u64, index: u64, options: &GeneratorOptions) -> Self {
        let mut rng = Rng::new(&[seed, index, 0xf022]);
        let count = rng.range(options.min_txs as u64, options.max_txs as u64);
        let slots = (0..count)
            .map(|_| Slot { kind: SlotKind::Random, salt: rng.next_u64(), tweaks: Vec::new() })
            .collect();
        Self { seed, index, slots, l1: Vec::new() }
    }

    /// A short stable identifier of the descriptor.
    pub fn id(&self) -> String {
        let json = serde_json::to_vec(self).expect("cases serialize");
        let hash = keccak256(json);
        format!("fz-{:x}-{}-{}", self.seed, self.index, alloy_primitives::hex::encode(&hash[..5]))
    }

    /// Builds the request with `options` (as passed to the generator before
    /// [`Self::generator_options`]).
    pub fn build(&self, options: &GeneratorOptions) -> Template {
        let generator = Generator::new(self.seed, Self::generator_options(options));
        let invalid_percent = options.invalid_percent;
        let mut template = generator.template_with(self.index, &mut |planner, deposits| {
            if deposits {
                return;
            }
            for slot in &self.slots {
                slot.plan(planner, invalid_percent);
            }
        });
        if let Some(l1) = template.request.transactions.first_mut() {
            for tweak in &self.l1 {
                *l1 = tweak.apply(l1);
            }
        }
        template.request.id = self.id();
        template
    }
}

impl Slot {
    /// Plans the slot's transactions and applies its tweaks.
    pub fn plan(&self, planner: &mut Planner<'_>, invalid_percent: u64) {
        planner.rng = Rng::new(&[self.salt]);
        let before = planner.out.len();
        match &self.kind {
            SlotKind::Transfer => planner.transfer(),
            SlotKind::ProgramCall => planner.program_call(),
            SlotKind::DeployProgram => planner.deploy_program(),
            SlotKind::Precompile => planner.precompile_call(),
            SlotKind::Native => planner.native_call(),
            SlotKind::System => planner.system_call(),
            SlotKind::Delegation => planner.delegation(),
            SlotKind::Deposit if planner.ctx.deposits_allowed => planner.deposit(),
            SlotKind::Random | SlotKind::Deposit => planner.random(invalid_percent),
            SlotKind::Invalid => planner.invalid(),
            SlotKind::NativeAbi { target, selector, caller, mutation } => {
                planner.native_abi(*target, *selector, *caller, *mutation)
            }
            SlotKind::Deep => planner.deep_call(),
            SlotKind::Create => planner.create_case(),
            SlotKind::Destruct => planner.destruct_case(),
            SlotKind::Refund => planner.refund_case(),
            SlotKind::Mutant { source, edits, calls } => {
                Self::mutant(planner, *source, edits, *calls)
            }
        };
        let planned = planner.out.len() - before;
        if planned == 0 {
            return;
        }
        for tweak in &self.tweaks {
            let index = before + tweak.tx() as usize % planned;
            let base_fee = planner.ctx.base_fee;
            if let Some(raw) = tweak.apply(&planner.out[index], planner, base_fee) {
                planner.out[index].raw = raw;
            }
        }
    }

    fn mutant(planner: &mut Planner<'_>, source: u8, edits: &[CodeEdit], calls: u8) -> Option<()> {
        let programs = &planner.registry.programs;
        let (mut code, fragments, extra) = if source == 0 || programs.is_empty() {
            let program = planner.new_program();
            (program.runtime.to_vec(), program.fragments, program.created_runtimes)
        } else {
            let known = programs[(source as usize - 1) % programs.len()].clone();
            let code = planner.state.code(&known.address).cloned().unwrap_or_default();
            (code.to_vec(), known.fragments, Vec::new())
        };
        for edit in edits {
            edit.apply(&mut code);
        }
        planner.deploy_runtime(code.into(), fragments.clone(), extra, "fuzz:mutantDeploy")?;
        let address = planner.out.last()?.deploy.as_ref()?.address;
        for _ in 0..calls.max(1) {
            let params = planner.program_params(address);
            let mask = Program::random_mask(&mut planner.rng, fragments.len().max(1));
            let ending = *planner.rng.pick(&[
                Ending::Stop,
                Ending::Stop,
                Ending::Return,
                Ending::Revert,
                Ending::Invalid,
                Ending::SelfDestruct,
                Ending::OutOfGas,
            ]);
            let input = Program::calldata(mask, ending, &params, &[]);
            planner.call_gas(None, address, U256::ZERO, input, "fuzz:mutantCall".into(), None);
        }
        Some(())
    }
}

impl CodeEdit {
    /// Applies the edit to `code`.
    pub fn apply(&self, code: &mut Vec<u8>) {
        let at = |pos: u32, len: usize| if len == 0 { 0 } else { pos as usize % len };
        match self {
            Self::Set { pos, byte } if !code.is_empty() => {
                let i = at(*pos, code.len());
                code[i] = *byte;
            }
            Self::Set { .. } => code.push(0),
            Self::Insert { pos, bytes } => {
                let i = at(*pos, code.len() + 1);
                code.splice(i..i, bytes.iter().copied());
            }
            Self::Delete { pos, len } => {
                let i = at(*pos, code.len());
                let end = (i + *len as usize).min(code.len());
                code.drain(i..end);
            }
        }
    }
}

impl Tweak {
    /// Index of the transaction within the slot.
    pub const fn tx(&self) -> u8 {
        match self {
            Self::Gas { tx, .. }
            | Self::Value { tx, .. }
            | Self::Calldata { tx, .. }
            | Self::Fees { tx, .. }
            | Self::Mint { tx, .. } => *tx,
        }
    }

    /// The re-signed (or re-encoded) transaction, or `None` when the tweak does not apply.
    pub fn apply(
        &self,
        planned: &PlannedTx,
        planner: &Planner<'_>,
        base_fee: u64,
    ) -> Option<Bytes> {
        let envelope = BaseTxEnvelope::decode_2718(&mut planned.raw.as_ref()).ok()?;
        if let BaseTxEnvelope::Deposit(deposit) = &envelope {
            let mut deposit: TxDeposit = deposit.inner().clone();
            match self {
                Self::Gas { to, .. } => {
                    let (intrinsic, _) = Txs::intrinsic(
                        planner.ctx.upgrade,
                        &deposit.input,
                        deposit.to.is_create(),
                        &Default::default(),
                        0,
                    );
                    deposit.gas_limit = to.resolve(deposit.gas_limit, intrinsic);
                }
                Self::Value { value, .. } => deposit.value = *value,
                Self::Calldata { edit, .. } => deposit.input = edit.apply(&deposit.input),
                Self::Mint { mint, .. } => deposit.mint = *mint,
                Self::Fees { .. } => return None,
            }
            return Some(Txs::deposit(deposit));
        }
        let mut spec = Self::spec(&envelope)?;
        match self {
            Self::Gas { to, .. } => {
                let (intrinsic, floor) = Txs::intrinsic(
                    planner.ctx.upgrade,
                    &spec.input,
                    spec.to.is_create(),
                    &spec.access_list,
                    if spec.shape == Shape::Eip7702 { spec.authorizations.len() } else { 0 },
                );
                spec.gas_limit = to.resolve(spec.gas_limit, intrinsic.max(floor));
            }
            Self::Value { value, .. } => spec.value = *value,
            Self::Calldata { edit, .. } => spec.input = edit.apply(&spec.input),
            Self::Fees { to, .. } => {
                let base = u128::from(base_fee);
                (spec.max_fee, spec.priority_fee) = match to {
                    FeeTarget::AtBaseFee => (base, 0),
                    FeeTarget::BelowBaseFee => (base.saturating_sub(1), 0),
                    FeeTarget::TipIsMax => (spec.max_fee, spec.max_fee),
                    FeeTarget::TipAboveMax => (spec.max_fee, spec.max_fee + 1),
                    FeeTarget::Huge => (u128::from(u64::MAX), spec.priority_fee),
                };
            }
            Self::Mint { .. } => return None,
        }
        Some(Txs::sign(&spec))
    }

    /// The signing spec of a signed transaction from a generated key.
    fn spec(envelope: &BaseTxEnvelope) -> Option<TxSpec> {
        let key = Keys::get().index_of(&envelope.recover_signer().ok()?)?;
        let base =
            |shape, to, nonce, value, input: &Bytes, gas_limit, max_fee, priority_fee| TxSpec {
                key,
                shape,
                chain_id: 8453,
                nonce,
                to,
                value,
                input: input.clone(),
                gas_limit,
                max_fee,
                priority_fee,
                access_list: Default::default(),
                authorizations: Vec::new(),
                high_s: false,
            };
        let spec = match envelope {
            BaseTxEnvelope::Legacy(tx) => {
                let tx = Signed::tx(tx);
                let mut spec = base(
                    Shape::Legacy { eip155: tx.chain_id.is_some() },
                    tx.to,
                    tx.nonce,
                    tx.value,
                    &tx.input,
                    tx.gas_limit,
                    tx.gas_price,
                    0,
                );
                spec.chain_id = tx.chain_id.unwrap_or(8453);
                spec
            }
            BaseTxEnvelope::Eip2930(tx) => {
                let tx = Signed::tx(tx);
                let mut spec = base(
                    Shape::Eip2930,
                    tx.to,
                    tx.nonce,
                    tx.value,
                    &tx.input,
                    tx.gas_limit,
                    tx.gas_price,
                    0,
                );
                spec.chain_id = tx.chain_id;
                spec.access_list = tx.access_list.clone();
                spec
            }
            BaseTxEnvelope::Eip1559(tx) => {
                let tx = Signed::tx(tx);
                let mut spec = base(
                    Shape::Eip1559,
                    tx.to,
                    tx.nonce,
                    tx.value,
                    &tx.input,
                    tx.gas_limit,
                    tx.max_fee_per_gas,
                    tx.max_priority_fee_per_gas,
                );
                spec.chain_id = tx.chain_id;
                spec.access_list = tx.access_list.clone();
                spec
            }
            BaseTxEnvelope::Eip7702(tx) => {
                let tx = Signed::tx(tx);
                let mut spec = base(
                    Shape::Eip7702,
                    TxKind::Call(tx.to),
                    tx.nonce,
                    tx.value,
                    &tx.input,
                    tx.gas_limit,
                    tx.max_fee_per_gas,
                    tx.max_priority_fee_per_gas,
                );
                spec.chain_id = tx.chain_id;
                spec.access_list = tx.access_list.clone();
                spec.authorizations = tx.authorization_list.clone();
                spec
            }
            _ => return None,
        };
        Some(spec)
    }
}

impl GasTarget {
    /// The gas limit this target gives a transaction with limit `current` and intrinsic cost
    /// `intrinsic`.
    pub fn resolve(self, current: u64, intrinsic: u64) -> u64 {
        match self {
            Self::IntrinsicPlus(extra) => intrinsic.saturating_add(extra),
            Self::Minus(less) => current.saturating_sub(less),
            Self::PerMille(per_mille) => (u128::from(current) * u128::from(per_mille) / 1000)
                .min(u128::from(u64::MAX)) as u64,
            Self::Exact(gas) => gas,
        }
    }
}

impl ByteEdit {
    /// The edited input.
    pub fn apply(&self, input: &[u8]) -> Bytes {
        let mut out = input.to_vec();
        match self {
            Self::Word { at, word } => {
                let at = *at as usize % (out.len() + 1);
                if out.len() < at + 32 {
                    out.resize(at + 32, 0);
                }
                out[at..at + 32].copy_from_slice(&word.to_be_bytes::<32>());
            }
            Self::Flip { pos, bit } if !out.is_empty() => {
                let i = *pos as usize % out.len();
                out[i] ^= 1 << (bit % 8);
            }
            Self::Flip { .. } => out.push(1),
            Self::Truncate { len } => out.truncate(*len as usize % (out.len() + 1)),
            Self::Append { bytes } => out.extend_from_slice(bytes),
        }
        out.into()
    }
}

impl L1Tweak {
    /// Calldata fields (offset, width) of the packed L1 attributes layout (Ecotone onward).
    pub const PACKED_FIELDS: [(usize, usize); 12] = [
        (4, 4),
        (8, 4),
        (12, 8),
        (20, 8),
        (28, 8),
        (36, 32),
        (68, 32),
        (100, 32),
        (132, 32),
        (164, 4),
        (168, 8),
        (176, 2),
    ];

    /// Bedrock's ABI-encoded L1 attributes calldata length.
    pub const BEDROCK_LEN: usize = 4 + 8 * 32;

    /// The fields (offset, width) of an L1 attributes calldata of this length.
    pub fn fields(len: usize) -> Vec<(usize, usize)> {
        if len == Self::BEDROCK_LEN {
            (0..8).map(|i| (4 + 32 * i, 32)).collect()
        } else {
            Self::PACKED_FIELDS.iter().copied().filter(|(at, width)| at + width <= len).collect()
        }
    }

    /// The L1 attributes deposit with the field set.
    pub fn apply(&self, raw: &Bytes) -> Bytes {
        let Ok(BaseTxEnvelope::Deposit(deposit)) = BaseTxEnvelope::decode_2718(&mut raw.as_ref())
        else {
            return raw.clone();
        };
        let mut deposit: TxDeposit = deposit.inner().clone();
        let fields = Self::fields(deposit.input.len());
        if fields.is_empty() {
            return raw.clone();
        }
        let (at, width) = fields[self.field as usize % fields.len()];
        let mut input = deposit.input.to_vec();
        input[at..at + width].copy_from_slice(&self.value.to_be_bytes::<32>()[32 - width..]);
        deposit.input = input.into();
        Txs::deposit(deposit)
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::transaction::SignerRecoverable;
    use alloy_eips::Decodable2718;
    use base_common_consensus::BaseTxEnvelope;

    use super::FuzzCase;
    use crate::{GeneratorOptions, Mutator, Rng};

    /// Mutated descriptors rebuild deterministically into requests whose transactions decode and
    /// recover, so mutation never produces malformed input.
    #[test]
    fn mutated_cases_rebuild_deterministically_and_well_formed() {
        let options = GeneratorOptions::default();
        let mut mutator = Mutator { rng: Rng::new(&[7]), max_slots: 48, index_space: 1 << 32 };
        let mut case = FuzzCase::fresh(3, 0, &options);
        for round in 0..40 {
            let (child, _) = mutator.mutate(&case, Some(&FuzzCase::fresh(3, round, &options)));
            let template = child.build(&options);
            assert_eq!(template.request, child.build(&options).request);
            assert_eq!(template.request.id, child.id());
            for (raw, label) in template.request.transactions.iter().zip(&template.labels) {
                if label.starts_with("invalid:") {
                    continue;
                }
                let tx = BaseTxEnvelope::decode_2718(&mut raw.as_ref())
                    .unwrap_or_else(|e| panic!("{label} does not decode: {e}"));
                if !tx.is_deposit() {
                    tx.recover_signer().unwrap_or_else(|e| panic!("{label}: {e}"));
                }
            }
            case = child;
        }
    }
}
