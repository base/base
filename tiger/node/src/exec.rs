//! Block building and block validation over a parent state, shared by the node (Engine API) and
//! the diff adapter.

use alloy_consensus::Header;
use alloy_primitives::{Address, Bytes};
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope as SignedTx, Predeploys};
use base_common_genesis::BaseFeature;
use base_tiger_evm::{
    BaseBlockExecutor, BlockEnvInput, BlockExecutionResult, BlockRules, ChainSpec, ComputedHeader,
    Transactions, assemble_header, computed_header,
};
use alloy_primitives::B256;
use evm2::{BlockStateAccumulator, registry::HandlerError};

use crate::ParentState;

/// One executed block: what the chain stores and what RPC serves.
#[derive(Debug, Clone)]
pub struct Executed {
    /// The assembled header (for validation, the header the execution computed).
    pub header: Header,
    /// The included transactions, in block order.
    pub transactions: Vec<SignedTx>,
    /// The sender of each transaction (the `from` of a deposit).
    pub senders: Vec<Address>,
    /// One receipt per transaction.
    pub receipts: Vec<BaseReceiptEnvelope>,
    /// Each transaction's return data on success, revert bytes on `REVERT`, empty on a halt.
    pub outputs: Vec<Bytes>,
    /// The block's state changes.
    pub changes: BlockStateAccumulator,
    /// The state root after the block.
    pub state_root: B256,
}

/// The outcome of building a block from a list of candidate transactions.
#[derive(Debug)]
pub struct Built {
    /// Why the block could not be built (a fatal transaction or pre-execution failure).
    pub invalid: Option<String>,
    /// Candidate transactions that were left out, by index, with the reason.
    pub rejected: Vec<(usize, String)>,
    /// The built block, unless `invalid` is set.
    pub executed: Option<Executed>,
}

/// The outcome of validating a block.
#[derive(Debug)]
pub struct Validated {
    /// The first rule the block breaks.
    pub invalid: Option<String>,
    /// The block's execution, when every transaction executed (also reported for an invalid
    /// block whose failure came after execution, such as a wrong state root).
    pub executed: Option<Executed>,
}

/// Block building and validation for one chain.
#[derive(Debug, Clone, Copy)]
pub struct BlockExecution<'a> {
    /// The chain.
    pub chain: &'a ChainSpec,
}

impl BlockExecution<'_> {
    /// Builds a block as the payload builder does. With `skip_invalid`, transactions that fail
    /// validity checks are left out; without it (`noTxPool` attributes), any failing
    /// transaction makes the build fail. `Err` is a state read the parent state could not serve.
    pub fn build(
        &self,
        env: &BlockEnvInput,
        extra_data: Bytes,
        excess_blob_gas: Option<u64>,
        candidates: &[Bytes],
        skip_invalid: bool,
        state: &impl ParentState,
    ) -> Result<Built, String> {
        let invalid = |reason: String| Built { invalid: Some(reason), rejected: Vec::new(), executed: None };
        let database = state.database();
        let mut executor = match BaseBlockExecutor::for_block(self.chain, env, database) {
            Ok(executor) => executor,
            Err(error) => {
                if let Some(missing) = state.missing() {
                    return Err(missing);
                }
                return Ok(invalid(format!("pre-execution changes: {error}")));
            }
        };
        let mut rejected = Vec::new();
        let mut transactions = Vec::new();
        let mut senders = Vec::new();
        for (index, raw) in candidates.iter().enumerate() {
            let (signed, tx) = match Transactions::decode(raw) {
                Ok(decoded) => decoded,
                Err(error) if skip_invalid => {
                    rejected.push((index, error));
                    continue;
                }
                Err(error) => return Ok(invalid(format!("transaction {index}: {error}"))),
            };
            match executor.execute_transaction(&tx) {
                Ok(()) => {
                    senders.push(tx.signer());
                    transactions.push(signed);
                }
                Err(HandlerError::Fatal(code)) => {
                    if let Some(missing) = state.missing() {
                        return Err(missing);
                    }
                    return Ok(invalid(format!("transaction {index}: fatal {code:?}")));
                }
                Err(error) if skip_invalid => rejected.push((index, error.to_string())),
                Err(error) => return Ok(invalid(format!("transaction {index}: {error}"))),
            }
        }
        if let Some(missing) = state.missing() {
            return Err(missing);
        }
        let (_, result, changes) = executor.finish();
        let transactions_root = alloy_consensus::proofs::calculate_transaction_root(&transactions);
        let (computed, state_root) = self.computed(env.timestamp, transactions_root, &result, &changes, state)?;
        let header = assemble_header(self.chain, env, extra_data, excess_blob_gas, &computed);
        Ok(Built {
            invalid: None,
            rejected,
            executed: Some(Executed {
                header,
                transactions,
                senders,
                receipts: result.receipts,
                outputs: result.outputs,
                changes,
                state_root,
            }),
        })
    }

    /// Validates `header` with transactions `raw` on top of `parent`, as the node imports a
    /// block: header checks, checks against the parent, pre-execution checks, execution and
    /// post-execution checks. The first failure decides validity; execution still runs when
    /// it can, so every computed field is reported. `Err` is a state read the parent state
    /// could not serve.
    pub fn validate(
        &self,
        header: &Header,
        raw: &[Bytes],
        parent: &Header,
        state: &impl ParentState,
    ) -> Result<Validated, String> {
        let mut signed = Vec::with_capacity(raw.len());
        let mut recovered = Vec::with_capacity(raw.len());
        for (index, decoded) in Transactions::decode_all(raw).into_iter().enumerate() {
            match decoded {
                Ok((tx, sender)) => {
                    signed.push(tx);
                    recovered.push(sender);
                }
                Err(error) => {
                    return Ok(Validated {
                        invalid: Some(format!("transaction {index}: {error}")),
                        executed: None,
                    });
                }
            }
        }
        let mut invalid: Option<String> = None;
        let mut note = |stage: &str, result: Result<(), String>| {
            if let Err(error) = result {
                invalid.get_or_insert_with(|| format!("{stage}: {error}"));
            }
        };
        let rules = BlockRules { chain: self.chain };
        note("header", rules.validate_header(header));
        note("header against parent", rules.validate_against_parent(header, parent));
        let withdrawals = self.chain.is_active(BaseFeature::Withdrawals, header.timestamp).then(Vec::new);
        let transactions_root = alloy_consensus::proofs::calculate_transaction_root(&signed);
        note(
            "pre-execution",
            rules.validate_pre_execution(header, &signed, transactions_root, withdrawals.as_deref()),
        );

        let env = Self::env(header);
        let database = state.database();
        let mut executor = match BaseBlockExecutor::for_block(self.chain, &env, database) {
            Ok(executor) => executor,
            Err(error) => {
                if let Some(missing) = state.missing() {
                    return Err(missing);
                }
                let invalid = invalid.unwrap_or_else(|| format!("execution: pre-execution: {error}"));
                return Ok(Validated { invalid: Some(invalid), executed: None });
            }
        };
        for (index, tx) in recovered.iter().enumerate() {
            if let Err(error) = executor.execute_transaction(tx) {
                if let Some(missing) = state.missing() {
                    return Err(missing);
                }
                let invalid =
                    invalid.unwrap_or_else(|| format!("execution: transaction {index}: {error}"));
                return Ok(Validated { invalid: Some(invalid), executed: None });
            }
        }
        if let Some(missing) = state.missing() {
            return Err(missing);
        }
        let (_, result, changes) = executor.finish();
        let (computed, state_root) = self.computed(env.timestamp, transactions_root, &result, &changes, state)?;
        if let Err(error) = rules.validate_post_execution(header, &computed) {
            invalid.get_or_insert_with(|| format!("post-execution: {error}"));
        }
        let assembled = assemble_header(
            self.chain,
            &env,
            header.extra_data.clone(),
            header.excess_blob_gas,
            &computed,
        );
        Ok(Validated {
            invalid,
            executed: Some(Executed {
                header: assembled,
                transactions: signed,
                senders: recovered.iter().map(|tx| tx.signer()).collect(),
                receipts: result.receipts,
                outputs: result.outputs,
                changes,
                state_root,
            }),
        })
    }

    /// The execution inputs a header carries.
    pub fn env(header: &Header) -> BlockEnvInput {
        BlockEnvInput {
            number: header.number,
            timestamp: header.timestamp,
            coinbase: header.beneficiary,
            prev_randao: header.mix_hash,
            gas_limit: header.gas_limit,
            base_fee: header.base_fee_per_gas.unwrap_or_default(),
            parent_hash: header.parent_hash,
            parent_beacon_block_root: header.parent_beacon_block_root,
        }
    }

    fn computed(
        &self,
        timestamp: u64,
        transactions_root: B256,
        result: &BlockExecutionResult,
        changes: &BlockStateAccumulator,
        state: &impl ParentState,
    ) -> Result<(ComputedHeader, B256), String> {
        let state_root = state.state_root_after(changes)?;
        let computed = computed_header(self.chain, timestamp, transactions_root, result, state_root, || {
            state.storage_root_after(Predeploys::L2_TO_L1_MESSAGE_PASSER, changes)
        })?;
        Ok((computed, state_root))
    }
}
