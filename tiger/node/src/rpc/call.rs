//! `eth_call`, `eth_estimateGas` and `eth_createAccessList` as base-reth answers them: reth's
//! call environment (fee charging, EIP-3607 and the base fee check off), the RPC gas cap, block
//! and state overrides, reth's estimation search, and reth's error codes.

use std::sync::Arc;

use alloy_consensus::{TxEip1559, TxEip2930, TxEip4844, TxEip4844Variant, TxEip7702, TxLegacy};
use alloy_eips::{BlockId, eip2930::AccessList, eip4844::VERSIONED_HASH_VERSION_KZG};
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, TxKind, U64, U256, keccak256};
use alloy_rpc_types_eth::{BlockOverrides, TransactionRequest, state::StateOverride};
use base_common_genesis::BaseFeature;
use base_common_rpc_types::BaseTransactionRequest;
use base_tiger_evm::{BaseEvmTypes, BaseSpecId, BaseTxEnvelope, BaseTxHandlerHooks, BlockRules};
use evm2::{
    AccountInfo, Evm, EvmFeatures, SpecId, TxResult,
    bytecode::Bytecode,
    env::BlockEnv,
    ethereum::{LazyTxEip7702, TxEnvelope, eip4844},
    evm::Db,
    interpreter::InstrStop,
    registry::{HandlerError, TxRegistry},
};
use evm2_inspectors::access_list::AccessListInspector;
use serde_json::{Value, json};

use super::{
    param,
    shared::{LastStep, Shared},
};
use super::state::{RpcDb, RpcState};
use crate::{Chain, Node, RpcError, chain::StoredBlock};

/// reth's default `--rpc.gascap`.
pub(super) const RPC_GAS_CAP: u64 = 50_000_000;
/// base-reth's error for a Base transaction request at a block before Everest.
const BASE_TX_PRE_EVEREST_RPC_ERROR: &str = "Base transaction RPC features are not active before the Everest hard fork; the `nonce_key` parameter is not supported at this block";
/// Blocks `BLOCKHASH` reaches back.
const BLOCK_HASH_WINDOW: u64 = 256;
/// Gas a plain value transfer uses.
const TRANSFER_GAS: u64 = 21_000;
/// reth stops the estimation search when the range is below this fraction of the upper bound.
const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;
/// How many blocks below the head tiger takes reth to hold in memory, unpersisted. reth starts
/// persisting once more than `DEFAULT_PERSISTENCE_THRESHOLD` (50) are, down to 5 below the head,
/// but a lagging persistence lets the window run past 51: this is a middle guess, and answers
/// that depend on it can differ from base-reth's.
const RETH_MAX_UNPERSISTED_BLOCKS: u64 = 51;

/// Gas kept for a callee's stipend in the optimistic estimate.
const CALL_STIPEND_GAS: u64 = 2_300;

/// Which method an environment is for: they disable different checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallKind {
    /// `eth_call`, `eth_createAccessList`, `debug_traceCall`.
    Call,
    /// `eth_estimateGas`: keeps the block gas limit check and the EIP-7825 cap.
    Estimate,
}

/// The block a call runs on: the state after `parent`, and the block environment.
#[derive(Debug)]
pub(super) struct CallBlock {
    /// The block whose post-state the call reads.
    pub parent: Arc<StoredBlock>,
    /// The block environment.
    pub env: BlockEnv<BaseEvmTypes>,
    /// The timestamp that selects the spec: the block's own, never an overridden one.
    pub spec_timestamp: u64,
}

/// A request turned into transaction fields, as alloy-evm's `TryIntoTxEnv` does.
#[derive(Debug, Clone)]
pub(super) struct CallTx {
    ty: u8,
    gas_price: u128,
    priority_fee: Option<u128>,
    /// `None` when the request has no `gas`.
    pub gas: Option<u64>,
    chain_id: u64,
    /// The caller.
    pub from: Address,
    /// The call target, or create.
    pub kind: TxKind,
    value: U256,
    input: Bytes,
    access_list: AccessList,
    authorizations: Vec<alloy_eips::eip7702::SignedAuthorization>,
    blob_hashes: Vec<B256>,
    max_fee_per_blob_gas: u128,
    blob_checks: BlobChecks,
    /// Whether the request's chain id (if any) is the chain's.
    chain_id_valid: bool,
}

/// What revm's blob checks need besides the request.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlobChecks {
    /// The block's blob base fee.
    pub block_blob_fee: u128,
    /// The fork's blob limit per transaction; `None` before Cancun (Ecotone).
    pub max_blobs: Option<u64>,
}

impl CallTx {
    /// revm's validation of a blob request, in its order after the chain id: the blob fee, the
    /// blob count and the hash versions (`validate_eip4844_tx`).
    fn check_blob_create(&self) -> Result<(), RpcError> {
        let Some(max_blobs) = self.blob_checks.max_blobs else {
            return Err(RpcError::new(-32003, "transaction type not supported"));
        };
        if self.blob_checks.block_blob_fee > self.max_fee_per_blob_gas {
            return Err(RpcError::new(-32003, "max fee per blob gas less than block blob gas fee"));
        }
        if self.blob_hashes.is_empty() {
            return Err(RpcError::new(-32003, "blob transaction missing blob hashes"));
        }
        if self.blob_hashes.iter().any(|hash| hash[0] != VERSIONED_HASH_VERSION_KZG) {
            return Err(RpcError::new(-32003, "blob hash version mismatch"));
        }
        if self.blob_hashes.len() as u64 > max_blobs {
            return Err(RpcError::new(-32003, "too many blobs"));
        }
        Ok(())
    }

    /// The transaction with `gas` and `nonce`, as the evm2 envelope the registry dispatches.
    pub(super) fn recovered(&self, gas: u64, nonce: u64) -> Result<alloy_consensus::transaction::Recovered<BaseTxEnvelope>, RpcError> {
        // revm checks the chain id first, before gating the transaction type by fork.
        if !self.chain_id_valid {
            return Err(RpcError::new(-32000, "invalid chain ID"));
        }
        let chain_id = self.chain_id;
        let tx = match self.ty {
            0 => TxEnvelope::Legacy(TxLegacy {
                chain_id: Some(chain_id),
                nonce,
                gas_price: self.gas_price,
                gas_limit: gas,
                to: self.kind,
                value: self.value,
                input: self.input.clone(),
            }),
            1 => TxEnvelope::Eip2930(TxEip2930 {
                chain_id,
                nonce,
                gas_price: self.gas_price,
                gas_limit: gas,
                to: self.kind,
                value: self.value,
                input: self.input.clone(),
                access_list: self.access_list.clone(),
            }),
            2 => TxEnvelope::Eip1559(TxEip1559 {
                chain_id,
                nonce,
                gas_limit: gas,
                max_fee_per_gas: self.gas_price,
                max_priority_fee_per_gas: self.priority_fee.unwrap_or_default(),
                to: self.kind,
                value: self.value,
                access_list: self.access_list.clone(),
                input: self.input.clone(),
            }),
            3 => {
                let TxKind::Call(to) = self.kind else {
                    // evm2's blob transaction always has a callee, but revm runs a blob request
                    // without one as a create after its blob checks: make those, then run the
                    // create as an EIP-1559 transaction with the same fees.
                    self.check_blob_create()?;
                    let tx = TxEip1559 {
                        chain_id,
                        nonce,
                        gas_limit: gas,
                        max_fee_per_gas: self.gas_price,
                        max_priority_fee_per_gas: self.priority_fee.unwrap_or_default(),
                        to: TxKind::Create,
                        value: self.value,
                        access_list: self.access_list.clone(),
                        input: self.input.clone(),
                    };
                    let envelope = BaseTxEnvelope::standard(TxEnvelope::Eip1559(tx), Bytes::from(vec![2]));
                    return Ok(alloy_consensus::transaction::Recovered::new_unchecked(envelope, self.from));
                };
                TxEnvelope::Eip4844(TxEip4844Variant::TxEip4844(TxEip4844 {
                    chain_id,
                    nonce,
                    gas_limit: gas,
                    max_fee_per_gas: self.gas_price,
                    max_priority_fee_per_gas: self.priority_fee.unwrap_or_default(),
                    to,
                    value: self.value,
                    access_list: self.access_list.clone(),
                    blob_versioned_hashes: self.blob_hashes.clone(),
                    max_fee_per_blob_gas: self.max_fee_per_blob_gas,
                    input: self.input.clone(),
                }))
            }
            4 => {
                let TxKind::Call(to) = self.kind else {
                    return Err(RpcError::new(-32003, "EIP-7702 transaction cannot create"));
                };
                TxEnvelope::Eip7702(LazyTxEip7702::from_signed_authorizations(TxEip7702 {
                    chain_id,
                    nonce,
                    gas_limit: gas,
                    max_fee_per_gas: self.gas_price,
                    max_priority_fee_per_gas: self.priority_fee.unwrap_or_default(),
                    to,
                    value: self.value,
                    access_list: self.access_list.clone(),
                    authorization_list: self.authorizations.clone(),
                    input: self.input.clone(),
                }))
            }
            _ => return Err(RpcError::new(-32003, "transaction type not supported")),
        };
        // A call pays no L1 fee (fee charging is off); the envelope bytes only need to exist.
        let envelope = BaseTxEnvelope::standard(tx, Bytes::from(vec![self.ty]));
        Ok(alloy_consensus::transaction::Recovered::new_unchecked(envelope, self.from))
    }
}

impl Node {
    /// Answers the call methods; `None` for other methods.
    pub(super) fn rpc_call(&self, method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            "eth_call" => self.eth_call(p),
            "eth_estimateGas" => self.estimate_gas(p),
            "eth_createAccessList" => self.create_access_list(p),
            _ => return None,
        })
    }

    /// The block environment of a call at block `tag`. `pending` is the next block on the head,
    /// with the Denim schedule's timestamp when `forecast` is set (as base-reth's `eth_call` and
    /// `eth_estimateGas` do) and twelve seconds later otherwise.
    pub(super) fn call_block(&self, tag: Option<&Value>, forecast: bool) -> Result<CallBlock, RpcError> {
        if tag.and_then(Value::as_str) == Some("pending") {
            let parent = self.chain.read().expect("chain lock").head();
            let header = &parent.header;
            let number = header.number + 1;
            let timestamp = match self.spec.denim_schedule().filter(|_| forecast) {
                Some(schedule) => schedule.block_timestamp_parts(number).0,
                None => header.timestamp + 12,
            };
            let base_fee = BlockRules { chain: &self.spec }.next_base_fee(header, timestamp).unwrap_or_default();
            let env = BlockEnv::<BaseEvmTypes> {
                number: U256::from(number),
                beneficiary: header.beneficiary,
                timestamp: U256::from(timestamp),
                gas_limit: U256::from(header.gas_limit),
                basefee: U256::from(base_fee),
                prevrandao: U256::from_be_bytes(Self::random_word().0),
                ..BlockEnv::<BaseEvmTypes>::default()
            };
            return Ok(CallBlock { parent, env, spec_timestamp: timestamp });
        }
        let parent = self.find_block(tag)?.ok_or_else(|| Self::block_not_found(tag))?;
        let header = &parent.header;
        let env = BlockEnv::<BaseEvmTypes> {
            number: U256::from(header.number),
            beneficiary: header.beneficiary,
            timestamp: U256::from(header.timestamp),
            gas_limit: U256::from(header.gas_limit),
            basefee: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            prevrandao: U256::from_be_bytes(header.mix_hash.0),
            ..BlockEnv::<BaseEvmTypes>::default()
        };
        let spec_timestamp = header.timestamp;
        Ok(CallBlock { parent, env, spec_timestamp })
    }

    /// reth's `-32001 block not found: <id>` for a block a request named, printed as alloy's
    /// `BlockId` prints it: `0x1a`, a tag, or `[canonical ]hash 0x…`. A missing tag is `latest`.
    pub(super) fn block_not_found(tag: Option<&Value>) -> RpcError {
        let id = tag.and_then(|tag| serde_json::from_value::<BlockId>(tag.clone()).ok()).unwrap_or_default();
        RpcError::new(-32001, format!("block not found: {id}"))
    }

    /// A fresh unpredictable word, as reth's pending `prevrandao`.
    fn random_word() -> B256 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        keccak256(nanos.as_nanos().to_le_bytes())
    }

    /// An EVM for calls on `block`'s environment over the state after its parent block.
    pub(super) fn call_evm<'a>(
        &self,
        chain: &Chain,
        state: &'a RpcState<'_>,
        block: &CallBlock,
        kind: CallKind,
    ) -> Evm<'a, BaseEvmTypes> {
        let spec = self.spec.spec_at(block.spec_timestamp);
        let config = BaseEvmTypes::execution_config(spec, self.spec.chain_id);
        let mut version = *config.version();
        for feature in [
            EvmFeatures::EIP3607,
            EvmFeatures::BASE_FEE_CHECK,
            EvmFeatures::FEE_CHARGE,
            EvmFeatures::BALANCE_CHECK,
            EvmFeatures::BALANCE_TOP_UP,
        ] {
            version.features.remove(feature);
        }
        if kind == CallKind::Call {
            version.features.remove(EvmFeatures::BLOCK_GAS_LIMIT_CHECK);
            version.tx_gas_limit_cap = u64::MAX;
        }
        let mut evm = Evm::new_with_execution_config(
            config.with_version(version),
            spec,
            block.env,
            Self::call_registry(spec),
            Db::new(RpcDb(state)),
            BaseEvmTypes::precompiles(spec, &self.spec),
        );
        // The state's `BLOCKHASH` window is the one for the block after `parent`; a call at
        // `parent`'s own number also sees the block 256 below it.
        let number = block.parent.number();
        if block.env.number == U256::from(number) && number >= BLOCK_HASH_WINDOW {
            let at = number - BLOCK_HASH_WINDOW;
            evm.overlay_db_mut().insert_block_hash(&U256::from(at), &chain.canonical_hash(at).unwrap_or_default());
        }
        evm
    }

    /// The transaction types calls run: blocks reject EIP-4844 on Base, but base-reth's call
    /// path executes a blob request once Cancun (Ecotone) is active.
    fn call_registry(spec: BaseSpecId) -> TxRegistry<BaseEvmTypes, TxResult<BaseEvmTypes>> {
        let mut registry = BaseEvmTypes::tx_registry(spec);
        if SpecId::from(spec).enables(SpecId::CANCUN) {
            registry.register(
                3,
                |tx: &BaseTxEnvelope| tx.as_standard().and_then(TxEnvelope::as_eip4844),
                eip4844::handle_with_hooks::<BaseEvmTypes, BaseTxHandlerHooks>,
            );
        }
        registry
    }

    /// Applies block overrides to an environment, as alloy-evm does (the spec stays the
    /// block's).
    pub(super) fn apply_block_overrides(env: &mut BlockEnv<BaseEvmTypes>, overrides: &BlockOverrides) {
        if let Some(number) = overrides.number {
            env.number = number;
        }
        if let Some(difficulty) = overrides.difficulty {
            env.difficulty = difficulty;
        }
        if let Some(time) = overrides.time {
            env.timestamp = U256::from(time);
        }
        if let Some(gas_limit) = overrides.gas_limit {
            env.gas_limit = U256::from(gas_limit);
        }
        if let Some(coinbase) = overrides.coinbase {
            env.beneficiary = coinbase;
        }
        if let Some(random) = overrides.random {
            env.prevrandao = U256::from_be_bytes(random.0);
        }
        if let Some(base_fee) = overrides.base_fee {
            env.basefee = U256::from(base_fee.saturating_to::<u64>());
        }
        if let Some(blob_base_fee) = overrides.blob_base_fee {
            env.blob_basefee = U256::from(blob_base_fee.saturating_to::<u128>());
        }
    }

    /// Applies block-hash overrides to the EVM's overlay. An overridden block number moves the
    /// `BLOCKHASH` window: reth answers it from the state provider at `state_block`, which knows
    /// the canonical hashes up to that block, plus every block reth has persisted to its
    /// database. Later blocks still in reth's memory are zero. Which recent blocks are persisted
    /// depends on reth's persistence timing; tiger takes the blocks at least
    /// [`RETH_MAX_UNPERSISTED_BLOCKS`] below the head as persisted and newer ones as in memory.
    pub(super) fn apply_block_hash_overrides(
        chain: &Chain,
        evm: &mut Evm<'_, BaseEvmTypes>,
        overrides: &BlockOverrides,
        state_block: u64,
    ) {
        if let Some(number) = overrides.number {
            let number = number.saturating_to::<u64>();
            let head = chain.head().number();
            for at in number.saturating_sub(BLOCK_HASH_WINDOW)..number {
                let persisted = at.saturating_add(RETH_MAX_UNPERSISTED_BLOCKS) <= head;
                let hash = if at <= state_block || persisted { chain.canonical_hash(at).unwrap_or_default() } else { B256::ZERO };
                evm.overlay_db_mut().insert_block_hash(&U256::from(at), &hash);
            }
        }
        for (number, hash) in overrides.block_hash.iter().flatten() {
            evm.overlay_db_mut().insert_block_hash(&U256::from(*number), hash);
        }
    }

    /// Applies state overrides to the EVM's overlay, as alloy-evm's `apply_state_overrides`.
    pub(super) fn apply_state_overrides(
        evm: &mut Evm<'_, BaseEvmTypes>,
        overrides: &StateOverride,
    ) -> Result<(), RpcError> {
        for (address, account) in overrides {
            let mut info = evm.read_account_info(address).map_err(|error| RpcError::internal(format!("{error:?}")))?.unwrap_or_default();
            if let Some(nonce) = account.nonce {
                info.nonce = nonce;
            }
            if let Some(code) = &account.code {
                let bytecode = Bytecode::new_raw_checked(code.clone())
                    .map_err(|error| RpcError::invalid_params(format!("Invalid bytecode: {error:?}")))?;
                info.code_hash = keccak256(code);
                info.code = Some(bytecode);
            }
            if let Some(balance) = account.balance {
                info.balance = balance;
            }
            if account.state.is_some() && account.state_diff.is_some() {
                return Err(RpcError::invalid_params(format!(
                    "account {address:?} has both 'state' and 'stateDiff'"
                )));
            }
            let overlay = evm.overlay_db_mut();
            overlay.insert_account_info(address, info);
            if let Some(state) = &account.state {
                overlay.cache.storage.entry(*address).or_default().wipe();
                for (slot, value) in state {
                    overlay.insert_account_storage(address, &U256::from_be_bytes(slot.0), &U256::from_be_bytes(value.0));
                }
            }
            if let Some(diff) = &account.state_diff {
                for (slot, value) in diff {
                    overlay.insert_account_storage(address, &U256::from_be_bytes(slot.0), &U256::from_be_bytes(value.0));
                }
            }
        }
        Ok(())
    }

    /// The request as transaction fields: fee resolution, type inference and the input check,
    /// in alloy-evm's order. `base_fee` and `blob_base_fee` are the block's after overrides.
    pub(super) fn call_tx(
        &self,
        request: &TransactionRequest,
        base_fee: u128,
        blob_base_fee: u128,
        max_blobs: Option<u64>,
        from: Address,
    ) -> Result<CallTx, RpcError> {
        let blob_hashes = request.blob_versioned_hashes.as_ref();
        if blob_hashes.is_some_and(Vec::is_empty) {
            return Err(RpcError::new(-32003, "blob transaction missing blob hashes"));
        }
        let ty = if request.authorization_list.is_some() {
            4
        } else if blob_hashes.is_some() || request.sidecar.is_some() {
            3
        } else if request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some() {
            2
        } else if request.access_list.is_some() {
            1
        } else {
            0
        };
        let has_blob_hashes = blob_hashes.is_some_and(|hashes| !hashes.is_empty());
        let effective = |max_fee: Option<u128>, priority: Option<u128>| -> Result<u128, RpcError> {
            match max_fee {
                Some(max_fee) => {
                    let priority = priority.unwrap_or_default();
                    if !(max_fee == 0 && priority == 0) && max_fee < base_fee {
                        return Err(RpcError::new(-32000, "max fee per gas less than block base fee"));
                    }
                    if max_fee < priority {
                        return Err(RpcError::new(-32003, "max priority fee per gas higher than max fee per gas"));
                    }
                    let tip = base_fee
                        .checked_add(priority)
                        .ok_or_else(|| RpcError::new(-32003, "max priority fee per gas higher than 2^256-1"))?;
                    Ok(max_fee.min(tip))
                }
                None => base_fee
                    .checked_add(priority.unwrap_or_default())
                    .ok_or_else(|| RpcError::new(-32003, "max priority fee per gas higher than 2^256-1")),
            }
        };
        let (gas_price, priority_fee) = match (
            request.gas_price,
            request.max_fee_per_gas,
            request.max_priority_fee_per_gas,
            request.max_fee_per_blob_gas,
        ) {
            (gas_price, None, None, None) => (gas_price.unwrap_or_default(), None),
            (None, max_fee, priority, None) => (effective(max_fee, priority)?, priority),
            (None, max_fee, priority, Some(_)) => {
                let price = effective(max_fee, priority)?;
                if !has_blob_hashes {
                    return Err(RpcError::new(-32003, "blob transaction missing blob hashes"));
                }
                (price, priority)
            }
            _ => {
                return Err(RpcError::invalid_params(
                    "both gasPrice and (maxFeePerGas or maxPriorityFeePerGas) specified".into(),
                ));
            }
        };
        let input = request.input.clone().try_into_unique_input().map_err(|_| {
            RpcError::invalid_params(
                "both \"data\" and \"input\" are set and not equal. Please use \"input\" to pass transaction call data"
                    .into(),
            )
        })?;
        Ok(CallTx {
            ty,
            gas_price,
            priority_fee,
            gas: request.gas,
            chain_id: request.chain_id.unwrap_or(self.spec.chain_id),
            from,
            kind: request.to.unwrap_or(TxKind::Create),
            value: request.value.unwrap_or_default(),
            input: input.unwrap_or_default(),
            access_list: request.access_list.clone().unwrap_or_default(),
            authorizations: request.authorization_list.clone().unwrap_or_default(),
            blob_hashes: blob_hashes.cloned().unwrap_or_default(),
            // An absent blob fee cap is the block's blob fee, so it always passes the check.
            max_fee_per_blob_gas: request.max_fee_per_blob_gas.unwrap_or(blob_base_fee),
            blob_checks: BlobChecks {
                block_blob_fee: blob_base_fee,
                max_blobs,
            },
            chain_id_valid: request.chain_id.is_none_or(|chain_id| chain_id == self.spec.chain_id),
        })
    }

    /// The nonce and balance of `address` in the EVM's current (overridden) state.
    fn account_now(evm: &mut Evm<'_, BaseEvmTypes>, address: &Address) -> Result<AccountInfo, RpcError> {
        Ok(evm.read_account_info(address).map_err(|error| RpcError::internal(format!("{error:?}")))?.unwrap_or_default())
    }

    /// `(balance - value) / gas_price`, or reth's insufficient funds error.
    fn caller_allowance(evm: &mut Evm<'_, BaseEvmTypes>, tx: &CallTx) -> Result<u64, RpcError> {
        let balance = Self::account_now(evm, &tx.from)?.balance;
        if tx.value > balance {
            return Err(RpcError::new(
                -32003,
                format!("insufficient funds for gas * price + value: have {balance} want {}", tx.value),
            ));
        }
        Ok(((balance - tx.value) / U256::from(tx.gas_price)).saturating_to())
    }

    /// Parses a request and rejects what base-reth routes elsewhere: Base transaction requests
    /// (`0x79` or account-abstraction fields).
    pub(super) fn plain_request(&self, p: &Value, block: Option<&Value>, method: &str) -> Result<TransactionRequest, RpcError> {
        let request: BaseTransactionRequest = param(p, 0)?;
        if request.as_base_tx().is_some() {
            if method == "eth_createAccessList" {
                return Err(RpcError::invalid_params(
                    "Base transaction requests are supported only by eth_call and eth_estimateGas".into(),
                ));
            }
            return Err(self.base_tx_unsupported(block));
        }
        Ok(request.into())
    }

    /// base-reth's `TxBaseEverestGate`: the block `tag` names (`pending` is the head), which must
    /// exist and be at or after Everest, or `-32602`.
    pub(super) fn everest_gate(&self, tag: Option<&Value>) -> Result<Arc<StoredBlock>, RpcError> {
        let pending = tag.and_then(Value::as_str) == Some("pending");
        let block = self
            .find_block(if pending { None } else { tag })?
            .ok_or_else(|| RpcError::invalid_params(Self::block_not_found(tag).message))?;
        if !self.spec.is_active(BaseFeature::TxBase, block.header.timestamp) {
            return Err(RpcError::invalid_params(BASE_TX_PRE_EVEREST_RPC_ERROR.into()));
        }
        Ok(block)
    }

    /// The error for a Base transaction request: before Everest base-reth rejects it with
    /// `-32602`; simulating one after Everest is not wired into tiger's RPC yet.
    fn base_tx_unsupported(&self, block: Option<&Value>) -> RpcError {
        match self.everest_gate(block) {
            Ok(_) => RpcError::internal("Base transaction simulation is not supported yet".into()),
            Err(error) => error,
        }
    }

    /// Prepares an `eth_call`-style execution: the EVM with overrides applied, the transaction
    /// fields, and the gas limit, following reth's `prepare_call_env`.
    pub(super) fn prepare_call<'a>(
        &self,
        chain: &Chain,
        state: &'a RpcState<'_>,
        request: &TransactionRequest,
        block: &mut CallBlock,
        state_overrides: Option<&StateOverride>,
        block_overrides: Option<&BlockOverrides>,
    ) -> Result<(Evm<'a, BaseEvmTypes>, CallTx, u64, u64), RpcError> {
        if let Some(overrides) = block_overrides {
            Self::apply_block_overrides(&mut block.env, overrides);
        }
        let mut evm = self.call_evm(chain, state, block, CallKind::Call);
        if let Some(overrides) = block_overrides {
            Self::apply_block_hash_overrides(chain, &mut evm, overrides, block.parent.number());
        }
        if let Some(overrides) = state_overrides {
            Self::apply_state_overrides(&mut evm, overrides)?;
        }
        let from = request.from.unwrap_or_default();
        let nonce = Self::account_now(&mut evm, &from)?.nonce;
        let max_blobs = self.blob_params(block.spec_timestamp).map(|params| params.max_blobs_per_tx);
        let tx = self.call_tx(request, block.env.basefee.saturating_to(), evm.block().blob_basefee.saturating_to(), max_blobs, from)?;
        let mut gas = tx.gas.map_or(RPC_GAS_CAP, |gas| gas.min(RPC_GAS_CAP));
        if tx.gas_price == 0 {
            let mut env = *evm.block();
            env.basefee = U256::ZERO;
            evm.set_block(env);
        }
        if tx.gas.is_none() && tx.gas_price > 0 {
            let allowance = Self::caller_allowance(&mut evm, &tx)?;
            gas = allowance.min(gas).min(evm.block().gas_limit.saturating_to());
        }
        Ok((evm, tx, gas, nonce))
    }

    /// Runs `tx` at `gas`, discarding its state.
    pub(super) fn run(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &CallTx,
        gas: u64,
        nonce: u64,
    ) -> Result<TxResult<BaseEvmTypes>, RpcError> {
        let recovered = tx.recovered(gas, nonce)?;
        evm.call_tx(&recovered).map_err(Self::handler_error)
    }

    /// reth's JSON-RPC error for a transaction evm2 rejected before executing it.
    pub(super) fn handler_error(error: HandlerError) -> RpcError {
        let (code, message) = match &error {
            HandlerError::InvalidChainId { .. } | HandlerError::MissingChainId => (-32000, "invalid chain ID".to_owned()),
            HandlerError::IntrinsicGasTooLow { .. } => (-32000, "intrinsic gas too low".to_owned()),
            HandlerError::GasLimitMoreThanBlock { .. } | HandlerError::TxGasLimitGreaterThanCap { .. } => {
                (-32000, "intrinsic gas too high".to_owned())
            }
            HandlerError::InvalidNonce { expected, got } if got < expected => {
                (-32000, format!("nonce too low: next nonce {expected}, tx nonce {got}"))
            }
            HandlerError::InvalidNonce { .. } => (-32000, "nonce too high".to_owned()),
            HandlerError::FeeCapLessThanBaseFee { .. } => (-32000, "max fee per gas less than block base fee".to_owned()),
            HandlerError::NonceOverflow => (-32003, "nonce has max value".to_owned()),
            HandlerError::CreateInitCodeSizeLimit { .. } => (-32003, "max initcode size exceeded".to_owned()),
            HandlerError::PriorityFeeGreaterThanMaxFee => {
                (-32003, "max priority fee per gas higher than max fee per gas".to_owned())
            }
            HandlerError::UnsupportedTransactionType(_) | HandlerError::WrongTransactionType { .. } => {
                (-32003, "transaction type not supported".to_owned())
            }
            HandlerError::EmptyAuthorizationList => (-32003, "EIP-7702 authorization list has invalid fields".to_owned()),
            HandlerError::InsufficientFunds => (-32003, "insufficient funds for gas * price + value".to_owned()),
            HandlerError::RejectCallerWithCode => (-32003, "sender is not an EOA".to_owned()),
            HandlerError::BlobFeeCapLessThanBlobBaseFee { .. } => {
                (-32003, "max fee per blob gas less than block blob gas fee".to_owned())
            }
            HandlerError::EmptyBlobs => (-32003, "blob transaction missing blob hashes".to_owned()),
            HandlerError::TooManyBlobs { have, .. } => {
                (-32003, format!("blob transaction exceeds max blobs per block; got {have}"))
            }
            HandlerError::BlobVersionNotSupported => (-32003, "blob hash version mismatch".to_owned()),
            HandlerError::Fatal(_) => (-32603, error.to_string()),
            _ => (-32003, error.to_string()),
        };
        RpcError::new(code, message)
    }

    /// Whether evm2 reports a halt (anything but success or `REVERT`).
    fn is_revert(result: &TxResult<BaseEvmTypes>) -> bool {
        result.stop == InstrStop::Revert
    }

    /// reth's error for a failed execution: code 3 with the revert data, or `-32003` for a halt.
    pub(super) fn failure(result: &TxResult<BaseEvmTypes>, gas: u64) -> RpcError {
        if Self::is_revert(result) {
            let mut error = RpcError::new(3, "execution reverted");
            if !result.output.is_empty() {
                error.data = Some(json!(result.output));
                // reth's `RevertError`: `Error(string)`, `Panic(uint256)` or UTF-8 output.
                if let Some(reason) = alloy_sol_types::RevertReason::decode(&result.output) {
                    let text = reason.to_string();
                    let text = if reason.is_revert() { text.trim_start_matches("revert: ") } else { text.as_str() };
                    error.message = format!("execution reverted: {text}");
                }
            }
            return error;
        }
        RpcError::new(-32003, Self::halt_message(result.stop, gas))
    }

    /// [`Self::failure`] for a result of `tx` on `evm`, telling evm2's single `InvalidOpcode`
    /// apart as revm does: re-running the call finds the halting opcode.
    pub(super) fn failure_on(
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &alloy_consensus::transaction::Recovered<BaseTxEnvelope>,
        result: &TxResult<BaseEvmTypes>,
        gas: u64,
    ) -> RpcError {
        if result.stop != InstrStop::InvalidOpcode {
            return Self::failure(result, gas);
        }
        let last = Shared::new(LastStep::default());
        evm.set_inspector(last.handle());
        let rerun = evm.call_tx(tx);
        evm.clear_inspector();
        drop(rerun);
        // The top frame's halt ends the call, so its opcode is the last one stepped.
        let message = match last.into_inner().0 {
            Some((0xfe, _)) => "EVM error: InvalidFEOpcode".to_owned(),
            Some((opcode, remaining)) if Self::revm_defines(opcode) => {
                // revm charges a gated opcode's static gas before it checks the fork.
                if remaining < Self::gated_static_gas(opcode) {
                    format!("out of gas: gas required exceeds: {gas}")
                } else {
                    "EVM error: NotActivated".to_owned()
                }
            }
            _ => "EVM error: OpcodeNotFound".to_owned(),
        };
        RpcError::new(-32003, message)
    }

    /// Whether revm 43 defines `opcode` on some fork: evm2 halts with `InvalidOpcode` on those
    /// when the fork has not activated them (revm `NotActivated`), and on every other byte (revm
    /// `OpcodeNotFound`).
    fn revm_defines(opcode: u8) -> bool {
        !matches!(
            opcode,
            0x0c..=0x0f | 0x1f | 0x21..=0x2f | 0x4c..=0x4f | 0xa5..=0xe5 | 0xe9..=0xef | 0xf6..=0xf9 | 0xfb | 0xfc
        )
    }

    /// revm's static gas of an opcode a Base fork can leave inactive (Cancun and later; Base
    /// starts at Bedrock).
    fn gated_static_gas(opcode: u8) -> u64 {
        match opcode {
            0x1e => 5,
            0x49 | 0x5e | 0xe6..=0xe8 => 3,
            0x5c | 0x5d => 100,
            0x4a | 0x4b => 2,
            _ => 0,
        }
    }

    /// reth's message for a halt.
    pub(super) fn halt_message(stop: InstrStop, gas: u64) -> String {
        match stop {
            InstrStop::OutOfGas | InstrStop::ReentrancySentryOOG => format!("out of gas: gas required exceeds: {gas}"),
            InstrStop::MemoryOOG => format!("out of gas: gas exhausted during memory expansion: {gas}"),
            InstrStop::MemoryLimitOOG => "out of memory: memory limit exceeded during memory expansion".to_owned(),
            InstrStop::PrecompileOOG => format!("out of gas: gas exhausted during precompiled contract execution: {gas}"),
            InstrStop::InvalidOperandOOG => format!("out of gas: invalid operand to an opcode: {gas}"),
            InstrStop::NonceOverflow => "nonce has max value".to_owned(),
            other => format!("EVM error: {other:?}"),
        }
    }

    fn eth_call(&self, p: &Value) -> Result<Value, RpcError> {
        let request = self.plain_request(p, p.get(1), "eth_call")?;
        let state_overrides: Option<StateOverride> = param(p, 2)?;
        let block_overrides: Option<BlockOverrides> = param(p, 3)?;
        let mut block = self.call_block(p.get(1), true)?;
        let chain = self.chain.read().expect("chain lock");
        let state = self.rpc_state(&chain, &block.parent)?;
        let (mut evm, tx, gas, nonce) =
            self.prepare_call(&chain, &state, &request, &mut block, state_overrides.as_ref(), block_overrides.as_ref())
                .map_err(|error| state.read_error(error))?;
        let recovered = tx.recovered(gas, nonce)?;
        let result = evm.call_tx(&recovered).map_err(Self::handler_error).map_err(|error| state.read_error(error))?;
        if result.status {
            return Ok(json!(result.output));
        }
        Err(Self::failure_on(&mut evm, &recovered, &result, result.tx_gas_used()))
    }

    fn estimate_gas(&self, p: &Value) -> Result<Value, RpcError> {
        let request = self.plain_request(p, p.get(1), "eth_estimateGas")?;
        let state_overrides: Option<StateOverride> = param(p, 2)?;
        let block_overrides: Option<BlockOverrides> = param(p, 3)?;
        let mut block = self.call_block(p.get(1), true)?;
        if let Some(overrides) = &block_overrides {
            Self::apply_block_overrides(&mut block.env, overrides);
        }
        let chain = self.chain.read().expect("chain lock");
        let state = self.rpc_state(&chain, &block.parent)?;
        let estimate = || -> Result<Value, RpcError> {
            let mut evm = self.call_evm(&chain, &state, &block, CallKind::Estimate);
            if let Some(overrides) = &block_overrides {
                Self::apply_block_hash_overrides(&chain, &mut evm, overrides, block.parent.number());
            }
            if let Some(overrides) = &state_overrides {
                Self::apply_state_overrides(&mut evm, overrides)?;
            }
            let block_gas_limit: u64 = evm.block().gas_limit.saturating_to();
            let max_gas_limit = evm.version().tx_gas_limit_cap.min(block_gas_limit).min(RPC_GAS_CAP);
            let mut highest = request.gas.unwrap_or(max_gas_limit).min(max_gas_limit);
            let from = request.from.unwrap_or_default();
            let nonce = Self::account_now(&mut evm, &from)?.nonce;
            let max_blobs = self.blob_params(block.spec_timestamp).map(|params| params.max_blobs_per_tx);
            let tx = self.call_tx(&request, evm.block().basefee.saturating_to(), evm.block().blob_basefee.saturating_to(), max_blobs, from)?;
            let mut gas = tx.gas.unwrap_or(block_gas_limit);
            let basic_transfer = tx.input.is_empty()
                && match tx.kind {
                    TxKind::Call(to) => evm
                        .read_account_info(&to)
                        .ok()
                        .is_some_and(|info| info.is_none_or(|info| info.code_hash == KECCAK256_EMPTY)),
                    TxKind::Create => false,
                };
            if tx.gas_price > 0 {
                highest = highest.min(Self::caller_allowance(&mut evm, &tx)?);
            }
            gas = gas.min(highest);
            if basic_transfer
                && let Ok(result) = Self::run(&mut evm, &tx, TRANSFER_GAS.min(max_gas_limit), nonce)
                && result.status
            {
                return Ok(json!(U64::from(result.tx_gas_used())));
            }
            let explicit = request.gas.is_some() || request.gas_price.is_some();
            let result = match Self::run(&mut evm, &tx, gas, nonce) {
                Err(error) if error.message == "intrinsic gas too high" && explicit => {
                    return self.out_of_gas_error(&mut evm, &tx, gas, max_gas_limit, nonce);
                }
                Err(error) if error.message == "intrinsic gas too low" => {
                    return Err(RpcError::new(-32000, format!("gas required exceeds allowance ({gas})")));
                }
                Err(error) => return Err(error),
                Ok(result) => result,
            };
            if !result.status {
                if Self::is_revert(&result) && explicit {
                    return self.out_of_gas_error(&mut evm, &tx, gas, max_gas_limit, nonce);
                }
                return Err(Self::failure_on(&mut evm, &tx.recovered(gas, nonce)?, &result, gas));
            }
            let refund = Self::final_refund(&result);
            highest = gas;
            let mut gas_used = result.tx_gas_used();
            let mut lowest = gas_used.saturating_sub(1);
            let optimistic = (u128::from(gas_used) + u128::from(refund) + u128::from(CALL_STIPEND_GAS)) * 64 / 63;
            if optimistic < u128::from(highest) {
                let optimistic = optimistic as u64;
                let result = Self::run(&mut evm, &tx, optimistic, nonce)?;
                gas_used = result.tx_gas_used();
                if result.status {
                    highest = optimistic;
                } else {
                    lowest = optimistic;
                }
            }
            let mut mid = u64::try_from((u128::from(highest) + u128::from(lowest)) / 2)
                .expect("the mean of two u64 fits")
                .min(gas_used.saturating_mul(3));
            while lowest + 1 < highest {
                if ((highest - lowest) as f64 / highest as f64) < ESTIMATE_GAS_ERROR_RATIO {
                    break;
                }
                match Self::run(&mut evm, &tx, mid, nonce) {
                    Err(error) if error.message == "intrinsic gas too high" => highest = mid,
                    Err(error) if error.message == "intrinsic gas too low" => lowest = mid,
                    Err(error) => return Err(error),
                    Ok(result) if result.status => highest = mid,
                    Ok(_) => lowest = mid,
                }
                mid = u64::try_from((u128::from(highest) + u128::from(lowest)) / 2).expect("the mean of two u64 fits");
            }
            Ok(json!(U64::from(highest)))
        };
        estimate().map_err(|error| state.read_error(error))
    }

    /// The refund the result applied: none when the EIP-7623 floor decided the gas.
    fn final_refund(result: &TxResult<BaseEvmTypes>) -> u64 {
        if result.total_gas_spent.saturating_sub(result.refunded) >= result.floor_gas { result.refunded } else { 0 }
    }

    /// reth's `map_out_of_gas_err`: retries at the largest limit to tell an out-of-gas from a
    /// revert.
    fn out_of_gas_error(
        &self,
        evm: &mut Evm<'_, BaseEvmTypes>,
        tx: &CallTx,
        gas: u64,
        max_gas_limit: u64,
        nonce: u64,
    ) -> Result<Value, RpcError> {
        let result = Self::run(evm, tx, max_gas_limit, nonce)?;
        if result.status {
            return Err(RpcError::new(-32003, format!("out of gas: gas required exceeds: {gas}")));
        }
        Err(Self::failure_on(evm, &tx.recovered(max_gas_limit, nonce)?, &result, gas))
    }

    fn create_access_list(&self, p: &Value) -> Result<Value, RpcError> {
        // reth resolves the block before the request turns into a transaction environment.
        param::<BaseTransactionRequest>(p, 0)?;
        let state_overrides: Option<StateOverride> = param(p, 2)?;
        let mut block = self.call_block(p.get(1), false)?;
        let request = self.plain_request(p, p.get(1), "eth_createAccessList")?;
        let initial = request.access_list.clone().unwrap_or_default();
        let chain = self.chain.read().expect("chain lock");
        let state = self.rpc_state(&chain, &block.parent)?;
        let access_list = || -> Result<Value, RpcError> {
            let (mut evm, mut tx, gas, nonce) = self.prepare_call(&chain, &state, &request, &mut block, state_overrides.as_ref(), None)?;
            let recovered = tx.recovered(gas, nonce)?;
            // revm-inspectors drops the caller, the callee (or created address), the precompiles and
            // the valid authorities from the request's list; evm2's inspector only skips them later.
            let mut excluded: std::collections::HashSet<Address> = evm.precompiles().addresses().into_iter().collect();
            excluded.insert(tx.from);
            excluded.insert(match tx.kind {
                TxKind::Call(to) => to,
                TxKind::Create => tx.from.create(nonce),
            });
            // Only valid authorizations' authorities: chain id 0 or the chain's, a nonce below
            // u64::MAX, and a signature that recovers (revm-inspectors `AccessListInspector`).
            excluded.extend(
                tx.authorizations
                    .iter()
                    .filter(|authorization| {
                        (authorization.chain_id.is_zero() || authorization.chain_id == U256::from(self.spec.chain_id))
                            && authorization.nonce != u64::MAX
                    })
                    .filter_map(|authorization| authorization.recover_authority().ok()),
            );
            let mut seeded = Self::dedup(initial);
            seeded.0.retain(|item| !excluded.contains(&item.address));
            let inspector = AccessListInspector::new(seeded).with_excluded(excluded.iter().copied());
            let inspector = Shared::new(inspector);
            evm.set_inspector(inspector.handle());
            let first = evm.call_tx(&recovered).map_err(Self::handler_error);
            evm.clear_inspector();
            let first = first?;
            let mut list = inspector.into_inner().into_access_list();
            list.0.sort_by_key(|item| item.address);
            list.0.iter_mut().for_each(|item| item.storage_keys.sort());
            if !first.status {
                return Ok(json!({
                    "accessList": list,
                    "gasUsed": U256::from(first.tx_gas_used()),
                    "error": Self::failure_on(&mut evm, &recovered, &first, first.tx_gas_used()).message,
                }));
            }
            tx.access_list = list.clone();
            if tx.ty == 0 {
                tx.ty = 1;
            }
            let recovered = tx.recovered(gas, nonce)?;
            let second = evm.call_tx(&recovered).map_err(Self::handler_error)?;
            let mut out = json!({ "accessList": list, "gasUsed": U256::from(second.tx_gas_used()) });
            if !second.status {
                out["error"] = json!(Self::failure_on(&mut evm, &recovered, &second, second.tx_gas_used()).message);
            }
            Ok(out)
        };
        access_list().map_err(|error| state.read_error(error))
    }

    /// Merges duplicate addresses and keys of an access list, as alloy's `dedup`.
    fn dedup(list: AccessList) -> AccessList {
        let mut merged: Vec<alloy_eips::eip2930::AccessListItem> = Vec::new();
        for item in list.0 {
            match merged.iter_mut().find(|existing| existing.address == item.address) {
                Some(existing) => existing.storage_keys.extend(item.storage_keys),
                None => merged.push(item),
            }
        }
        for item in &mut merged {
            let mut seen = std::collections::HashSet::new();
            item.storage_keys.retain(|key| seen.insert(*key));
        }
        AccessList(merged)
    }
}


#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::test_utils::{REVERTER, node, reply};

    fn call(node: &crate::Node, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        reply(node, &body.to_string()).expect("a request is answered")
    }

    #[test]
    fn calls_report_reverts_and_estimates_as_reth() {
        let node = node();
        let reverted = call(&node, "eth_call", json!([{ "to": REVERTER }]));
        assert_eq!(reverted["error"]["code"], json!(3));
        assert_eq!(reverted["error"]["data"], json!("0xaa"));
        let reverted = call(&node, "eth_estimateGas", json!([{ "to": REVERTER }]));
        assert_eq!((reverted["error"]["code"].clone(), reverted["error"]["data"].clone()), (json!(3), json!("0xaa")));
        // A plain transfer is 21000 without searching; a value above the balance is a halt.
        let transfer = json!({ "from": "0x0000000000000000000000000000000000000001", "to": "0x000000000000000000000000000000000000beef", "value": "0x1" });
        assert_eq!(call(&node, "eth_estimateGas", json!([transfer]))["result"], json!("0x5208"));
        let broke = json!({ "from": "0x0000000000000000000000000000000000000003", "to": "0x000000000000000000000000000000000000beef", "value": "0x1" });
        assert_eq!(call(&node, "eth_call", json!([broke]))["error"]["code"], json!(-32003));
        // A fee cap below the base fee is rejected before execution, unless both fees are zero.
        let underpriced = json!({ "to": REVERTER, "maxFeePerGas": "0x0", "maxPriorityFeePerGas": "0x0" });
        assert_eq!(call(&node, "eth_call", json!([underpriced]))["error"]["code"], json!(3));
        let underpriced = json!({ "to": REVERTER, "maxFeePerGas": "0x0", "maxPriorityFeePerGas": "0x0", "gasPrice": "0x1" });
        assert_eq!(call(&node, "eth_call", json!([underpriced]))["error"]["code"], json!(-32602));
        // A blob request without a callee runs as a plain create.
        let blob_hashes = json!([format!("0x01{}", "00".repeat(31))]);
        let blob_create = json!({ "input": "0x", "blobVersionedHashes": blob_hashes });
        assert_eq!(call(&node, "eth_call", json!([blob_create]))["result"], json!("0x"));
        // It still gets revm's blob checks: the blob fee cap first, then the hash versions.
        let bad_version = json!({ "input": "0x", "blobVersionedHashes": [format!("0x02{}", "00".repeat(31))] });
        assert_eq!(call(&node, "eth_call", json!([bad_version]))["error"]["code"], json!(-32003));
        let underpaid = json!({ "input": "0x", "blobVersionedHashes": blob_hashes, "maxFeePerBlobGas": "0x0" });
        assert_eq!(call(&node, "eth_call", json!([underpaid]))["error"]["code"], json!(-32003));
                // An unknown block is -32001.
        assert_eq!(call(&node, "eth_call", json!([{ "to": REVERTER }, "0x5"]))["error"]["code"], json!(-32001));
    }

    #[test]
    fn failures_carry_reths_messages() {
        let node = node();
        let callee = "0x000000000000000000000000000000000000c0de";
        let message = |method: &str, code: &str| {
            let params = json!([{ "to": callee }, "latest", { callee: { "code": code } }]);
            call(&node, method, params)["error"]["message"].clone()
        };
        // evm2's one `InvalidOpcode` is split by the byte as revm does: 0xFE, an opcode a later
        // fork adds (SLOTNUM at Jovian), the same with too little gas for it, and an undefined byte.
        assert_eq!(message("eth_call", "0xfe"), json!("EVM error: InvalidFEOpcode"));
        assert_eq!(message("eth_call", "0x4b"), json!("EVM error: NotActivated"));
        assert_eq!(message("eth_estimateGas", "0x0c"), json!("EVM error: OpcodeNotFound"));
        let starved = json!([{ "to": callee, "gas": "0x5209" }, "latest", { callee: { "code": "0x4b" } }]);
        assert_eq!(call(&node, "eth_call", starved)["error"]["message"], json!("out of gas: gas required exceeds: 21001"));
        // Revert output that is UTF-8 is the reason.
        assert_eq!(message("eth_call", "0x6468656c6c6f5f526005601bfd"), json!("execution reverted: hello"));
    }

    #[test]
    fn access_lists_exclude_only_valid_authorities() {
        let node = node();
        // Key 0x11…11 signs an authorization (to REVERTER, nonce 0) for chain 0 and for chain 1.
        let authority = "0x19e7e376e7c213b7e7e7e46cc70a5dd086daff2a";
        let authorization = |chain_id: &str, r: &str, s: &str| {
            json!({ "address": REVERTER, "chainId": chain_id, "nonce": "0x0", "yParity": "0x0", "r": r, "s": s })
        };
        let any_chain = authorization(
            "0x0",
            "0xb2eabe2cec138312f83ac334b8e6e8dc8d71164b626e0bc5472330aa4b067ee4",
            "0x61a7f5edbad872106f00170ebde6049b1a2679d1dddc5a8dcda47fd6d04c8f7f",
        );
        let other_chain = authorization(
            "0x1",
            "0xc228c60faa2dc984c07e5106195d29de90bad6810f12236272b5e290e720ae6c",
            "0x2f0af96840630e39b266aa5732b3ea97f5c3a86c447b9b3810e56b4e26cadbd1",
        );
        // The callee reads the authority's balance: PUSH20 authority, BALANCE, STOP.
        let callee = "0x000000000000000000000000000000000000c0de";
        let code = format!("0x73{}3100", &authority[2..]);
        let listed = |authorization: Value| {
            let request = json!({
                "from": "0x0000000000000000000000000000000000000001",
                "to": callee,
                "authorizationList": [authorization],
            });
            let list = call(&node, "eth_createAccessList", json!([request, "latest", { callee: { "code": code } }]));
            list["result"]["accessList"].as_array().expect("an access list").iter().any(|item| item["address"] == json!(authority))
        };
        // revm-inspectors excludes the authority of a valid authorization only; chain 1 is
        // invalid on Base, so its authority stays in the list.
        assert!(!listed(any_chain));
        assert!(listed(other_chain));
    }
}
