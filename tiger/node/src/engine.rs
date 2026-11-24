//! The Engine API the Base consensus node drives: `engine_newPayloadV2`–`V4`,
//! `engine_forkchoiceUpdatedV1`–`V3`, `engine_getPayloadV2`–`V5`, and the capability, client
//! version and payload body methods.
//!
//! Statuses and error codes follow base-reth: an unknown parent or head is `SYNCING`, a block
//! that breaks a rule is `INVALID` with the last valid ancestor, and malformed requests are
//! JSON-RPC errors (`-32602`, `-38002` invalid forkchoice state, `-38003` invalid payload
//! attributes, `-38005` unsupported fork, `-38001` unknown payload).

use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

use alloy_consensus::{Block, BlockBody, Header};
use alloy_eips::{eip7685::Requests, eip4895::Withdrawals};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, CancunPayloadFields, ExecutionPayloadEnvelopeV2,
    ExecutionPayloadFieldV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ForkchoiceState, PayloadId, PraguePayloadFields,
};
use base_common_consensus::{BaseTxEnvelope as SignedTx, Predeploys};
use base_common_genesis::BaseFeature;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadEnvelopeV3, BaseExecutionPayloadEnvelopeV4,
    BaseExecutionPayloadEnvelopeV5, BaseExecutionPayloadSidecar, BaseExecutionPayloadV4,
    BasePayloadAttributes,
};
use base_tiger_evm::{BlockEnvInput, BlockRules};
use serde_json::{Value, json};

use crate::{
    BLOCK_HASH_WINDOW, BlockExecution, Executed, Node, RpcError, Validation, WitnessNodes, WitnessState,
    chain::Forkchoice,
};

/// Most built payloads kept for `engine_getPayload`.
const MAX_PAYLOADS: usize = 16;
/// Most invalid blocks remembered, so descendants are rejected without execution.
const MAX_INVALID: usize = 512;

/// Engine API message versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    /// V1.
    V1 = 1,
    /// V2.
    V2 = 2,
    /// V3.
    V3 = 3,
    /// V4.
    V4 = 4,
    /// V5.
    V5 = 5,
}

/// The outcome of importing one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Imported {
    /// The block is valid and now stored.
    Valid,
    /// The block was already stored.
    Known,
    /// The parent is not stored.
    MissingParent,
    /// The block breaks a rule (or descends from a block that does).
    Invalid {
        /// The last valid ancestor.
        latest_valid: Option<B256>,
        /// Why the block is invalid.
        error: String,
    },
}

/// A block built from payload attributes.
#[derive(Debug, Clone)]
struct BuiltPayload {
    hash: B256,
    executed: Executed,
}

/// A rejected block: the last valid ancestor and why it was rejected.
#[derive(Debug, Clone)]
struct InvalidBlock {
    latest_valid: Option<B256>,
    error: String,
}

/// Engine API state beyond the chain: built payloads and rejected blocks.
#[derive(Debug, Default)]
pub struct EngineState {
    payloads: VecDeque<(PayloadId, BuiltPayload)>,
    invalid: HashMap<B256, InvalidBlock>,
    invalid_order: VecDeque<B256>,
}

impl EngineState {
    fn reject(&mut self, hash: B256, block: InvalidBlock) {
        if self.invalid.insert(hash, block).is_none() {
            self.invalid_order.push_back(hash);
        }
        while self.invalid_order.len() > MAX_INVALID {
            let oldest = self.invalid_order.pop_front().expect("over capacity");
            self.invalid.remove(&oldest);
        }
    }
}

fn status(status: &str, latest_valid: Option<B256>, error: Option<&str>) -> Value {
    json!({ "status": status, "latestValidHash": latest_valid, "validationError": error })
}

fn params<T: serde::de::DeserializeOwned>(params: &Value, index: usize) -> Result<T, RpcError> {
    let value = params.get(index).cloned().unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|error| RpcError::invalid_params(error.to_string()))
}

impl Node {
    /// Answers one `engine_*` method.
    pub fn engine(&self, method: &str, params_value: &Value) -> Result<Value, RpcError> {
        let p = params_value;
        match method {
            "engine_exchangeCapabilities" => Ok(json!(Self::CAPABILITIES)),
            "engine_getClientVersionV1" => Ok(json!([{
                "code": "TG",
                "name": "tiger",
                "version": env!("CARGO_PKG_VERSION"),
                "commit": "00000000",
            }])),
            "engine_newPayloadV2" => {
                let input: ExecutionPayloadInputV2 = params(p, 0)?;
                let payload = BaseExecutionPayload::v2(input);
                self.new_payload(Version::V2, payload, BaseExecutionPayloadSidecar::default())
            }
            "engine_newPayloadV3" => {
                let payload: ExecutionPayloadV3 = params(p, 0)?;
                let versioned_hashes: Vec<B256> = params(p, 1)?;
                let parent_beacon_block_root: B256 = params(p, 2)?;
                let sidecar = BaseExecutionPayloadSidecar::v3(CancunPayloadFields {
                    parent_beacon_block_root,
                    versioned_hashes,
                });
                self.new_payload(Version::V3, BaseExecutionPayload::V3(payload), sidecar)
            }
            "engine_newPayloadV4" => {
                let payload: BaseExecutionPayloadV4 = params(p, 0)?;
                let versioned_hashes: Vec<B256> = params(p, 1)?;
                let parent_beacon_block_root: B256 = params(p, 2)?;
                let requests: Vec<Bytes> = params(p, 3)?;
                let sidecar = BaseExecutionPayloadSidecar::v4(
                    CancunPayloadFields { parent_beacon_block_root, versioned_hashes },
                    PraguePayloadFields::new(Requests::new(requests)),
                );
                self.new_payload(Version::V4, BaseExecutionPayload::V4(payload), sidecar)
            }
            "engine_forkchoiceUpdatedV1" => self.forkchoice_updated(Version::V1, p),
            "engine_forkchoiceUpdatedV2" => self.forkchoice_updated(Version::V2, p),
            "engine_forkchoiceUpdatedV3" => self.forkchoice_updated(Version::V3, p),
            "engine_getPayloadV2" => self.get_payload(Version::V2, p),
            "engine_getPayloadV3" => self.get_payload(Version::V3, p),
            "engine_getPayloadV4" => self.get_payload(Version::V4, p),
            "engine_getPayloadV5" => self.get_payload(Version::V5, p),
            "engine_getPayloadBodiesByHashV1" => {
                let hashes: Vec<B256> = params(p, 0)?;
                if hashes.len() > 1024 {
                    return Err(RpcError::new(-38004, "Too large request"));
                }
                let chain = self.chain.read().expect("chain lock");
                Ok(Value::Array(
                    hashes.iter().map(|hash| Self::body_json(chain.block(hash).as_deref())).collect(),
                ))
            }
            "engine_getPayloadBodiesByRangeV1" => {
                let start: alloy_primitives::U64 = params(p, 0)?;
                let count: alloy_primitives::U64 = params(p, 1)?;
                let (start, count) = (start.to::<u64>(), count.to::<u64>());
                if start == 0 || count == 0 {
                    return Err(RpcError::invalid_params(format!("invalid start ({start}) or count ({count})")));
                }
                if count > 1024 {
                    return Err(RpcError::new(-38004, "Too large request"));
                }
                let chain = self.chain.read().expect("chain lock");
                let head = chain.head().number();
                let bodies: Vec<Value> = (start..start.saturating_add(count))
                    .take_while(|number| *number <= head)
                    .map(|number| Self::body_json(chain.canonical_block(number).as_deref()))
                    .collect();
                Ok(Value::Array(bodies))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    /// Methods `engine_exchangeCapabilities` reports.
    const CAPABILITIES: [&'static str; 13] = [
        "engine_forkchoiceUpdatedV1",
        "engine_forkchoiceUpdatedV2",
        "engine_forkchoiceUpdatedV3",
        "engine_getClientVersionV1",
        "engine_getPayloadBodiesByHashV1",
        "engine_getPayloadBodiesByRangeV1",
        "engine_getPayloadV2",
        "engine_getPayloadV3",
        "engine_getPayloadV4",
        "engine_getPayloadV5",
        "engine_newPayloadV2",
        "engine_newPayloadV3",
        "engine_newPayloadV4",
    ];

    fn body_json(block: Option<&crate::chain::StoredBlock>) -> Value {
        use alloy_eips::eip2718::Encodable2718;
        block.map_or(Value::Null, |block| {
            let transactions: Vec<Bytes> =
                block.transactions.iter().map(|tx| tx.encoded_2718().into()).collect();
            let withdrawals = block.header.withdrawals_root.map(|_| Vec::<Value>::new());
            json!({ "transactions": transactions, "withdrawals": withdrawals })
        })
    }

    /// The `-38005` / `-32602` checks of a message's fork-specific fields (reth's
    /// `validate_payload_timestamp`, withdrawals and parent beacon block root presence).
    fn check_version(
        &self,
        version: Version,
        timestamp: u64,
        has_withdrawals: bool,
        has_beacon_root: bool,
        attributes: bool,
    ) -> Result<(), RpcError> {
        let chain = &self.spec;
        let cancun = chain.is_active(BaseFeature::BlobHeader, timestamp);
        let prague = chain.is_active(BaseFeature::EmptyRequestsHash, timestamp);
        let osaka = chain.is_upgrade_active(base_common_genesis::BaseUpgrade::Azul, timestamp);
        let unsupported = (version == Version::V2 && cancun)
            || (version == Version::V3 && !cancun)
            || (version == Version::V4 && !prague)
            || (version == Version::V5 && !osaka);
        if unsupported {
            return Err(RpcError::new(-38005, "Unsupported fork"));
        }
        let shanghai = chain.is_active(BaseFeature::Withdrawals, timestamp);
        let what = if attributes { "payload attributes" } else { "payload" };
        let invalid = |message: &str| {
            if attributes {
                RpcError::new(-38003, format!("Invalid payload attributes: {message}"))
            } else {
                RpcError::invalid_params(message.to_owned())
            }
        };
        match version {
            Version::V1 if has_withdrawals => return Err(invalid("withdrawals not supported in V1")),
            Version::V1 if shanghai => return Err(invalid("no withdrawals post-Shanghai")),
            Version::V1 => {}
            _ if shanghai && !has_withdrawals => return Err(invalid("no withdrawals post-Shanghai")),
            _ if !shanghai && has_withdrawals => return Err(invalid("withdrawals pre-Shanghai")),
            _ => {}
        }
        if version >= Version::V3 && !has_beacon_root {
            return Err(invalid(&format!("no parent beacon block root post-Cancun in {what}")));
        }
        if version < Version::V3 && has_beacon_root {
            return Err(invalid(&format!("parent beacon block root not supported before V3 in {what}")));
        }
        Ok(())
    }

    fn new_payload(
        &self,
        version: Version,
        payload: BaseExecutionPayload,
        sidecar: BaseExecutionPayloadSidecar,
    ) -> Result<Value, RpcError> {
        let v1 = payload.as_v1();
        let timestamp = v1.timestamp;
        let expected_hash = v1.block_hash;
        let parent_hash = v1.parent_hash;
        let has_withdrawals = payload.as_v2().is_some();
        self.check_version(
            version,
            timestamp,
            has_withdrawals,
            sidecar.parent_beacon_block_root().is_some(),
            false,
        )?;
        let mut engine = self.engine.lock().expect("engine lock");
        let block = match payload.into_block_with_sidecar_raw(&sidecar) {
            Ok(block) => block,
            Err(error) => {
                let chain = self.chain.read().expect("chain lock");
                let latest_valid = chain.block(&parent_hash).map(|_| parent_hash);
                return Ok(status("INVALID", latest_valid, Some(&error.to_string())));
            }
        };
        let header = block.header;
        let hash = header.hash_slow();
        if hash != expected_hash {
            let error = format!("block hash mismatch: want {expected_hash}, got {hash}");
            return Ok(status("INVALID", None, Some(&error)));
        }
        match self.import_block(&mut engine, header, hash, &block.body.transactions)? {
            Imported::Valid | Imported::Known => Ok(status("VALID", Some(hash), None)),
            Imported::MissingParent => {
                self.request_sync(parent_hash);
                Ok(status("SYNCING", None, None))
            }
            Imported::Invalid { latest_valid, error } => Ok(status("INVALID", latest_valid, Some(&error))),
        }
    }

    /// Validates, executes and stores one block whose hash is `hash`.
    pub fn import_block(
        &self,
        engine: &mut EngineState,
        header: Header,
        hash: B256,
        transactions: &[Bytes],
    ) -> Result<Imported, RpcError> {
        let parent_hash = header.parent_hash;
        if let Some(invalid) = engine.invalid.get(&hash) {
            return Ok(Imported::Invalid { latest_valid: invalid.latest_valid, error: invalid.error.clone() });
        }
        if let Some(invalid) = engine.invalid.get(&parent_hash).cloned() {
            engine.reject(hash, invalid.clone());
            return Ok(Imported::Invalid {
                latest_valid: invalid.latest_valid,
                error: "links to previously rejected block".into(),
            });
        }
        let validated = {
            let chain = self.chain.read().expect("chain lock");
            if chain.block(&hash).is_some() {
                return Ok(Imported::Known);
            }
            let Some(parent) = chain.block(&parent_hash) else {
                return Ok(Imported::MissingParent);
            };
            let state = chain.state_after(&parent).map_err(RpcError::internal)?;
            match (BlockExecution { chain: &self.spec }).validate(&header, transactions, &parent.header, &state) {
                Ok(validated) => (validated, state.take_prepared().map(|prepared| Validation::Prepared(Box::new(prepared)))),
                Err(missing) => match &self.upstream {
                    Some(upstream) if chain.db.is_partial() => {
                        drop(state);
                        self.validate_with_witness(&chain, upstream, &header, hash, transactions, &parent)?
                    }
                    _ => return Err(RpcError::internal(format!("state of block {parent_hash}: {missing}"))),
                },
            }
        };
        let (validated, prepared) = validated;
        match (validated.invalid, validated.executed) {
            (None, Some(executed)) => {
                let mut stored = executed;
                stored.header = header;
                self.chain.write().expect("chain lock").insert(hash, &stored, prepared).map_err(RpcError::internal)?;
                Ok(Imported::Valid)
            }
            (invalid, _) => {
                let error = invalid.unwrap_or_else(|| "block did not execute".into());
                eprintln!("tiger: invalid block {} {hash}: {error}", header.number);
                engine.reject(hash, InvalidBlock { latest_valid: Some(parent_hash), error: error.clone() });
                Ok(Imported::Invalid { latest_valid: Some(parent_hash), error })
            }
        }
    }

    /// Validates block `hash` on a partial state over the upstream's execution witness, which
    /// is verified node by node against the parent's state root and header by header against
    /// the parent's ancestry.
    fn validate_with_witness(
        &self,
        chain: &crate::Chain,
        upstream: &crate::Upstream,
        header: &Header,
        hash: B256,
        transactions: &[Bytes],
        parent: &crate::StoredBlock,
    ) -> Result<(crate::Validated, Option<Validation>), RpcError> {
        let witness = upstream.execution_witness(hash, header.number).map_err(RpcError::internal)?;
        let mut hashes = chain.block_hashes(parent);
        // Ancestors below the datadir's first block come from the witness headers, each linked
        // by its hash to the next one up.
        let low = parent.number().saturating_sub(BLOCK_HASH_WINDOW - 1);
        let mut lowest = parent.number();
        while lowest > low && hashes.contains_key(&(lowest - 1)) {
            lowest -= 1;
        }
        if lowest > low {
            let headers: HashMap<B256, Header> = witness
                .headers
                .iter()
                .filter_map(|rlp| alloy_rlp::decode_exact::<Header>(rlp).ok())
                .map(|header| (header.hash_slow(), header))
                .collect();
            let mut next = chain.block(&hashes[&lowest]).map(|block| block.header.parent_hash);
            let mut number = lowest - 1;
            while let Some(ancestor) = next {
                hashes.insert(number, ancestor);
                if number == low {
                    break;
                }
                next = headers.get(&ancestor).map(|header| header.parent_hash);
                number -= 1;
            }
        }
        let message_passer_root = parent
            .header
            .withdrawals_root
            .filter(|_| self.spec.is_active(BaseFeature::L2ToL1MessageRoot, parent.header.timestamp))
            .map(|root| (Predeploys::L2_TO_L1_MESSAGE_PASSER, root));
        let state = Rc::new(WitnessState::new(
            parent.header.state_root,
            Arc::new(WitnessNodes::new(&witness.state, &witness.codes)),
            hashes,
            message_passer_root,
        ));
        let validated = BlockExecution { chain: &self.spec }
            .validate(header, transactions, &parent.header, &state)
            .map_err(|missing| {
                RpcError::internal(format!("the upstream witness of block {} {hash} is incomplete: {missing}", header.number))
            })?;
        Ok((validated, Some(Validation::Witness(witness))))
    }

    fn forkchoice_updated(&self, version: Version, p: &Value) -> Result<Value, RpcError> {
        let state: ForkchoiceState = params(p, 0)?;
        let attributes: Option<BasePayloadAttributes> = params(p, 1)?;
        if let Some(attributes) = &attributes {
            let inner = &attributes.payload_attributes;
            self.check_version(
                version,
                inner.timestamp,
                inner.withdrawals.is_some(),
                inner.parent_beacon_block_root.is_some(),
                true,
            )?;
            self.check_attributes(attributes)?;
        }
        let mut engine = self.engine.lock().expect("engine lock");
        let head = state.head_block_hash;
        if head.is_zero() {
            return Err(RpcError::new(-38002, "Invalid forkchoice state"));
        }
        if let Some(invalid) = engine.invalid.get(&head) {
            let payload_status = status("INVALID", invalid.latest_valid, Some(&invalid.error));
            return Ok(json!({ "payloadStatus": payload_status, "payloadId": null }));
        }
        let mut chain = self.chain.write().expect("chain lock");
        let Some(head_block) = chain.block(&head) else {
            drop(chain);
            self.request_sync(head);
            return Ok(json!({ "payloadStatus": status("SYNCING", None, None), "payloadId": null }));
        };
        for hash in [state.safe_block_hash, state.finalized_block_hash] {
            if !hash.is_zero() && chain.block(&hash).is_none() {
                return Ok(json!({ "payloadStatus": status("SYNCING", None, None), "payloadId": null }));
            }
        }
        let update = Forkchoice {
            head,
            safe: state.safe_block_hash,
            finalized: state.finalized_block_hash,
        };
        let previous = chain.forkchoice();
        // As in base-reth, the head moves before safe and finalized are checked against it.
        chain.set_canonical(head).map_err(RpcError::internal)?;
        for hash in [update.safe, update.finalized] {
            if !hash.is_zero() && !chain.is_canonical(&hash) {
                chain
                    .update_forkchoice(Forkchoice { safe: B256::ZERO, finalized: B256::ZERO, ..update })
                    .map_err(RpcError::internal)?;
                return Err(RpcError::new(-38002, "Invalid forkchoice state"));
            }
        }
        chain.update_forkchoice(update).map_err(RpcError::internal)?;
        if previous.head != head {
            eprintln!("tiger: head {} {head}", head_block.number());
        }
        let payload_status = status("VALID", Some(head), None);
        let Some(attributes) = attributes else {
            return Ok(json!({ "payloadStatus": payload_status, "payloadId": null }));
        };
        let timestamp = attributes.payload_attributes.timestamp;
        let subsecond = self.spec.is_active(BaseFeature::SubsecondBlocks, timestamp);
        let timestamp_ok = if subsecond {
            timestamp >= head_block.header.timestamp
        } else {
            timestamp > head_block.header.timestamp
        };
        if !timestamp_ok {
            return Err(RpcError::new(-38003, "Invalid payload attributes"));
        }
        let id = attributes.payload_id(&head, version as u8);
        if !engine.payloads.iter().any(|(existing, _)| *existing == id) {
            let built = {
                let state = chain.state_after(&head_block).map_err(RpcError::internal)?;
                (self.build(&head_block.header, &attributes, &state)?, state.take_prepared().map(|prepared| Validation::Prepared(Box::new(prepared))))
            };
            drop(chain);
            let built = self.store_built(built.0, built.1)?;
            engine.payloads.push_back((id, built));
            while engine.payloads.len() > MAX_PAYLOADS {
                engine.payloads.pop_front();
            }
        }
        Ok(json!({ "payloadStatus": payload_status, "payloadId": id }))
    }

    /// base-reth's `ensure_well_formed_attributes` beyond the version checks.
    fn check_attributes(&self, attributes: &BasePayloadAttributes) -> Result<(), RpcError> {
        let timestamp = attributes.payload_attributes.timestamp;
        let invalid = |message: &str| RpcError::new(-38003, format!("Invalid payload attributes: {message}"));
        if attributes.gas_limit.is_none() {
            return Err(invalid("MissingGasLimitInPayloadAttributes"));
        }
        if self.spec.is_active(BaseFeature::DynamicBaseFeeParameters, timestamp) {
            let (elasticity, denominator) = attributes
                .decode_eip_1559_params()
                .ok_or_else(|| invalid("MissingEip1559ParamsInPayloadAttributes"))?;
            if elasticity != 0 && denominator == 0 {
                return Err(invalid("Eip1559ParamsDenominatorZero"));
            }
            if denominator != 0 && elasticity == 0 {
                return Err(invalid("Eip1559ParamsElasticityZero"));
            }
        }
        match (self.spec.is_active(BaseFeature::MinBaseFee, timestamp), attributes.min_base_fee) {
            (true, None) => Err(invalid("MissingMinBaseFeeInPayloadAttributes")),
            (false, Some(_)) => Err(invalid("MinBaseFeeNotAllowedBeforeJovian")),
            _ => Ok(()),
        }
    }

    /// Builds the block the attributes describe on `parent`, as base-reth's payload builder
    /// does (without a transaction pool: the attribute transactions are the whole block).
    fn build(
        &self,
        parent: &Header,
        attributes: &BasePayloadAttributes,
        state: &impl crate::ParentState,
    ) -> Result<BuiltPayload, RpcError> {
        let inner = &attributes.payload_attributes;
        let timestamp = inner.timestamp;
        let params = self.spec.base_fee_params(timestamp);
        let extra_data = if self.spec.is_active(BaseFeature::MinBaseFee, timestamp) {
            attributes.get_jovian_extra_data(params)
        } else if self.spec.is_active(BaseFeature::DynamicBaseFeeParameters, timestamp) {
            attributes.get_holocene_extra_data(params)
        } else {
            Ok(Bytes::new())
        }
        .map_err(|error| RpcError::new(-38003, format!("Invalid payload attributes: {error:?}")))?;
        let base_fee = BlockRules { chain: &self.spec }
            .next_base_fee(parent, timestamp)
            .ok_or_else(|| RpcError::internal("parent extra data does not decode".into()))?;
        let env = BlockEnvInput {
            number: parent.number + 1,
            timestamp,
            coinbase: inner.suggested_fee_recipient,
            prev_randao: inner.prev_randao,
            gas_limit: attributes.gas_limit.unwrap_or(parent.gas_limit),
            base_fee,
            parent_hash: parent.hash_slow(),
            parent_beacon_block_root: inner.parent_beacon_block_root,
        };
        let transactions = attributes.transactions.clone().unwrap_or_default();
        let skip_invalid = !attributes.no_tx_pool.unwrap_or_default();
        let built = BlockExecution { chain: &self.spec }
            .build(&env, extra_data, None, &transactions, skip_invalid, state)
            .map_err(|error| RpcError::internal(format!("building payload: {error}")))?;
        let Some(executed) = built.executed else {
            let reason = built.invalid.unwrap_or_default();
            return Err(RpcError::internal(format!("building payload: {reason}")));
        };
        let hash = executed.header.hash_slow();
        Ok(BuiltPayload { hash, executed })
    }

    /// Stores a built payload's block: the consensus node inserts the payload with newPayload,
    /// and storing it now makes that a no-op, as base-reth's built blocks are already in its tree.
    fn store_built(&self, built: BuiltPayload, prepared: Option<Validation>) -> Result<BuiltPayload, RpcError> {
        self.chain.write().expect("chain lock").insert(built.hash, &built.executed, prepared).map_err(RpcError::internal)?;
        Ok(built)
    }

    fn get_payload(&self, version: Version, p: &Value) -> Result<Value, RpcError> {
        let id: PayloadId = params(p, 0)?;
        let engine = self.engine.lock().expect("engine lock");
        let Some((_, built)) = engine.payloads.iter().find(|(existing, _)| *existing == id) else {
            return Err(RpcError::new(-38001, "Unknown payload"));
        };
        let header = &built.executed.header;
        self.check_version(
            version,
            header.timestamp,
            header.withdrawals_root.is_some(),
            version >= Version::V3,
            false,
        )
        .map_err(|error| if error.code == -38005 { error } else { RpcError::new(-38005, "Unsupported fork") })?;
        let block = Block {
            header: header.clone(),
            body: BlockBody::<SignedTx> {
                transactions: built.executed.transactions.clone(),
                ommers: Vec::new(),
                withdrawals: header.withdrawals_root.map(|_| Withdrawals::default()),
            },
        };
        let hash = built.hash;
        let value = U256::ZERO;
        let beacon = header.parent_beacon_block_root.unwrap_or_default();
        let withdrawals_root = header.withdrawals_root.unwrap_or_default();
        let json = match version {
            Version::V1 => json!(ExecutionPayloadV1::from_block_unchecked(hash, &block)),
            Version::V2 => json!(ExecutionPayloadEnvelopeV2 {
                execution_payload: ExecutionPayloadFieldV2::from_block_unchecked(hash, &block),
                block_value: value,
            }),
            Version::V3 => json!(BaseExecutionPayloadEnvelopeV3 {
                execution_payload: ExecutionPayloadV3::from_block_unchecked(hash, &block),
                block_value: value,
                blobs_bundle: BlobsBundleV1 { blobs: vec![], commitments: vec![], proofs: vec![] },
                should_override_builder: false,
                parent_beacon_block_root: beacon,
            }),
            Version::V4 => json!(BaseExecutionPayloadEnvelopeV4 {
                execution_payload: BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                    ExecutionPayloadV3::from_block_unchecked(hash, &block),
                    withdrawals_root,
                ),
                block_value: value,
                blobs_bundle: BlobsBundleV1 { blobs: vec![], commitments: vec![], proofs: vec![] },
                should_override_builder: false,
                parent_beacon_block_root: beacon,
                execution_requests: vec![],
            }),
            Version::V5 => json!(BaseExecutionPayloadEnvelopeV5 {
                execution_payload: BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                    ExecutionPayloadV3::from_block_unchecked(hash, &block),
                    withdrawals_root,
                ),
                block_value: value,
                blobs_bundle: BlobsBundleV2::default(),
                should_override_builder: false,
                execution_requests: vec![],
            }),
        };
        Ok(json)
    }
}
