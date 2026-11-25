//! The Engine API the Base consensus node drives: `engine_newPayloadV2`–`V4`,
//! `engine_forkchoiceUpdatedV1`–`V3`, `engine_getPayloadV2`–`V5`, and the capability, client
//! version and payload body methods.
//!
//! Statuses and error codes follow base-reth: an unknown parent or head is `SYNCING`, a block
//! that breaks a rule is `INVALID` with the last valid ancestor, and malformed requests are
//! JSON-RPC errors (`-32602`, `-38002` invalid forkchoice state, `-38003` invalid payload
//! attributes, `-38005` unsupported fork, `-38001` unknown payload).

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
};

use alloy_consensus::{Block, BlockBody, Header};
use alloy_eips::{eip4895::Withdrawals, eip7685::Requests};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{
    BlobsBundleV1, BlobsBundleV2, CancunPayloadFields, ExecutionPayloadEnvelopeV2,
    ExecutionPayloadFieldV2, ExecutionPayloadInputV2, ExecutionPayloadV1, ExecutionPayloadV3,
    ForkchoiceState, PayloadId, PraguePayloadFields,
};
use alloy_trie::{
    Nibbles,
    nodes::{ExtensionNode, LeafNode, TrieNode},
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
/// Payloads buffered until they can be imported (about half an hour of 2 s blocks).
const MAX_PENDING: usize = 1024;
/// Rounds of fetching proofs for the state a block misses before its whole-block witness is used.
const MAX_PROOF_ROUNDS: usize = 8;

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
    /// The block reads state the partial datadir does not hold, and its execution witness is
    /// still being fetched (now requested).
    AwaitingWitness,
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
    /// Payloads that could not be imported yet (unknown parent, or a witness on its way), which
    /// the sync thread imports once they can be.
    pub pending: HashMap<B256, (Header, Vec<Bytes>)>,
    pending_order: VecDeque<B256>,
    /// The head of the latest forkchoice update.
    pub forkchoice_head: Option<B256>,
    payloads: VecDeque<(PayloadId, BuiltPayload)>,
    invalid: HashMap<B256, InvalidBlock>,
    invalid_order: VecDeque<B256>,
}

impl EngineState {
    fn keep_pending(&mut self, hash: B256, header: Header, transactions: Vec<Bytes>) {
        if self.pending.insert(hash, (header, transactions)).is_none() {
            self.pending_order.push_back(hash);
        }
        while self.pending_order.len() > MAX_PENDING {
            let oldest = self.pending_order.pop_front().expect("over capacity");
            self.pending.remove(&oldest);
        }
    }

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
        if method.starts_with("engine_newPayload") || method.starts_with("engine_forkchoiceUpdated") {
            self.persistence_backpressure();
        }
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
            Version::V1 if has_withdrawals => {
                return Err(invalid("withdrawals not supported in V1"));
            }
            Version::V1 if shanghai => return Err(invalid("no withdrawals post-Shanghai")),
            Version::V1 => {}
            _ if shanghai && !has_withdrawals => {
                return Err(invalid("no withdrawals post-Shanghai"));
            }
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
        match self.import_block(&mut engine, header.clone(), hash, &block.body.transactions)? {
            Imported::Valid | Imported::Known => Ok(status("VALID", Some(hash), None)),
            Imported::MissingParent | Imported::AwaitingWitness => {
                // As base-reth buffers a block it cannot validate yet: the sync thread imports it
                // from here (or downloads it) once its parent and witness are in.
                self.prewarm(&header, hash);
                engine.keep_pending(hash, header, block.body.transactions);
                drop(engine);
                self.request_sync(hash);
                Ok(status("SYNCING", None, None))
            }
            Imported::Invalid { latest_valid, error } => {
                Ok(status("INVALID", latest_valid, Some(&error)))
            }
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
        if let Some(imported) = Self::check_known_invalid(engine, &header, hash) {
            return Ok(imported);
        }
        match self.validate_block(&header, hash, transactions)? {
            Ok(validated) => self.store_validated(engine, header, hash, validated),
            Err(imported) => Ok(imported),
        }
    }

    /// The outcome of a block already rejected, or linking to one (which is rejected now).
    pub fn check_known_invalid(
        engine: &mut EngineState,
        header: &Header,
        hash: B256,
    ) -> Option<Imported> {
        if let Some(invalid) = engine.invalid.get(&hash) {
            return Some(Imported::Invalid {
                latest_valid: invalid.latest_valid,
                error: invalid.error.clone(),
            });
        }
        let invalid = engine.invalid.get(&header.parent_hash).cloned()?;
        engine.reject(hash, invalid.clone());
        Some(Imported::Invalid {
            latest_valid: invalid.latest_valid,
            error: "links to previously rejected block".into(),
        })
    }

    /// Validates and executes block `hash` without the Engine API state, so a long validation
    /// (a stateless node's proof rounds) does not hold up the Engine API. `Err` is an outcome
    /// with nothing to store.
    pub fn validate_block(
        &self,
        header: &Header,
        hash: B256,
        transactions: &[Bytes],
    ) -> Result<Result<(crate::Validated, Option<Validation>), Imported>, RpcError> {
        let parent_hash = header.parent_hash;
        let chain = self.chain.read().expect("chain lock");
        if chain.block(&hash).is_some() {
            return Ok(Err(Imported::Known));
        }
        let Some(parent) = chain.block(&parent_hash) else {
            return Ok(Err(Imported::MissingParent));
        };
        let missing = match self.validate_fetching_proofs(&chain, header, transactions, &parent)? {
            Ok(validated) => return Ok(Ok(validated)),
            Err(missing) => missing,
        };
        let witnesses = match &self.witnesses {
            Some(witnesses) if chain.db.is_partial() => witnesses,
            _ => {
                return Err(RpcError::internal(format!("state of block {parent_hash}: {missing}")));
            }
        };
        let Some(witness) = witnesses.take(&hash) else {
            witnesses.request(hash, header.number);
            return Ok(Err(Imported::AwaitingWitness));
        };
        let witness = witness.map_err(RpcError::internal)?;
        match self.validate_with_witness(&chain, witness, header, hash, transactions, &parent) {
            Ok(validated) => {
                witnesses.forget(&hash);
                Ok(Ok(validated))
            }
            // The upstream's canonical block at this number may be another one.
            Err(error) if witnesses.retry_by_hash(hash, header.number) => {
                eprintln!(
                    "tiger: the witness of block {} {hash} by number does not verify ({}), fetching it by hash",
                    header.number, error.message
                );
                Ok(Err(Imported::AwaitingWitness))
            }
            Err(error) => {
                witnesses.forget(&hash);
                Err(error)
            }
        }
    }

    /// Stores block `hash` that [`Self::validate_block`] validated, or records it as invalid.
    pub fn store_validated(
        &self,
        engine: &mut EngineState,
        header: Header,
        hash: B256,
        validated: (crate::Validated, Option<Validation>),
    ) -> Result<Imported, RpcError> {
        let parent_hash = header.parent_hash;
        let (validated, prepared) = validated;
        match (validated.invalid, validated.executed) {
            (None, Some(executed)) => {
                let mut stored = executed;
                stored.header = header;
                let mut chain = self.chain.write().expect("chain lock");
                // The Engine API may have pruned the parent while the block validated unlocked.
                if chain.block(&parent_hash).is_none() {
                    return Ok(Imported::MissingParent);
                }
                chain.insert(hash, &stored, prepared).map_err(RpcError::internal)?;
                Ok(Imported::Valid)
            }
            (invalid, _) => {
                let error = invalid.unwrap_or_else(|| "block did not execute".into());
                eprintln!("tiger: invalid block {} {hash}: {error}", header.number);
                engine.reject(
                    hash,
                    InvalidBlock { latest_valid: Some(parent_hash), error: error.clone() },
                );
                Ok(Imported::Invalid { latest_valid: Some(parent_hash), error })
            }
        }
    }

    /// Validates `header` over the database state after `parent`. On a stateless node, the
    /// accounts, slots and code the block reads that the partial state does not hold are fetched
    /// from the upstream as proofs at the stored tip and stored (each node only when it links by
    /// hash to the tip's state root), over a few rounds of execution. The inner `Err` is the
    /// state still missing after them (a trie node, such as the sibling a deletion collapses
    /// into, or an ancestor's block hash), for the whole-block witness to supply.
    fn validate_fetching_proofs(
        &self,
        chain: &crate::Chain,
        header: &Header,
        transactions: &[Bytes],
        parent: &crate::StoredBlock,
    ) -> Result<Result<(crate::Validated, Option<Validation>), String>, RpcError> {
        // Proofs are fetched at the stored tip, so they serve a block on the tip. A block on a side
        // branch uses its witness, which is stored at commit, after the unwind to its fork point.
        let stateless = chain.db.is_partial() && self.upstream.is_some() && parent.hash == chain.head().hash;
        let mut previous: Option<String> = None;
        for round in 0..=MAX_PROOF_ROUNDS {
            let state = chain.state_after(parent).map_err(RpcError::internal)?;
            let state = if stateless { state.collecting() } else { state };
            let missing = match (BlockExecution { chain: &self.spec }).validate(
                header,
                transactions,
                &parent.header,
                &state,
            ) {
                Ok(validated) => {
                    let prepared = state
                        .take_prepared()
                        .map(|prepared| Validation::Prepared(Box::new(prepared)));
                    return Ok(Ok((validated, prepared)));
                }
                Err(missing) => missing,
            };
            let misses = state.take_misses();
            drop(state);
            // The same first miss after a fetch means the upstream supplied nothing that links.
            let stalled = previous.as_ref() == Some(&missing);
            if !stateless
                || round == MAX_PROOF_ROUNDS
                || stalled
                || (misses.is_empty() && misses.deleted.is_empty())
            {
                return Ok(Err(missing));
            }
            let fetched = if misses.is_empty() {
                self.collapse_siblings(chain.head().hash, header.number, &misses.deleted)
                    .and_then(|(proofs, _)| Self::absorb(chain, &proofs))
            } else {
                self.absorb_proofs(chain, &misses)
            };
            if let Err(error) = fetched {
                return Ok(Err(format!("{missing} ({error})")));
            }
            previous = Some(missing);
        }
        unreachable!("the last round returns")
    }

    /// The trie nodes block `number`'s deletions may collapse into, which no read loads, and
    /// whether one of them is a branch, which no proof reveals (its witness has it). A deleted
    /// key's proof in the block's post-state ends at the leaf or extension its collapsed sibling
    /// became; that node, re-keyed at each deeper path it covers, is the sibling's pre-state form
    /// at one of them. The candidates come with the keys' proofs at `tip`, so storing them keeps
    /// only those that link by hash to the tip's state root: nothing the upstream returns is
    /// trusted.
    pub fn collapse_siblings(
        &self,
        tip: B256,
        number: u64,
        deleted: &BTreeMap<alloy_primitives::Address, std::collections::BTreeSet<U256>>,
    ) -> Result<(Proofs, bool), String> {
        let upstream = self.upstream.as_ref().expect("a stateless node has an upstream");
        let post = json!(format!("{number:#x}"));
        let mut calls = Vec::with_capacity(deleted.len() * 2);
        for (address, slots) in deleted {
            let slots: Vec<B256> = slots.iter().map(|slot| B256::from(*slot)).collect();
            calls.push(("eth_getProof", json!([address, slots, tip])));
            calls.push(("eth_getProof", json!([address, slots, post])));
        }
        let results = upstream.call_batch(&calls)?;
        let bytes = |value: &Value| {
            serde_json::from_value::<Bytes>(value.clone()).map_err(|error| error.to_string())
        };
        let proof = |value: &Value| {
            value.as_array().into_iter().flatten().map(bytes).collect::<Result<Vec<_>, _>>()
        };
        let (mut nodes, mut branch) = (Vec::new(), false);
        let mut add = |target: B256, proof: &[Bytes], nodes: &mut Vec<Bytes>| {
            let (candidates, is_branch) = collapse_candidates(target, proof);
            nodes.extend(candidates);
            branch |= is_branch;
        };
        for ((address, _), pair) in deleted.iter().zip(results.chunks_exact(2)) {
            let (pre, post) = (&pair[0], &pair[1]);
            nodes.extend(proof(&pre["accountProof"])?);
            add(alloy_primitives::keccak256(address), &proof(&post["accountProof"])?, &mut nodes);
            for storage in pre["storageProof"].as_array().into_iter().flatten() {
                nodes.extend(proof(&storage["proof"])?);
            }
            for storage in post["storageProof"].as_array().into_iter().flatten() {
                let key: U256 = serde_json::from_value(storage["key"].clone())
                    .map_err(|error| error.to_string())?;
                add(
                    alloy_primitives::keccak256(B256::from(key)),
                    &proof(&storage["proof"])?,
                    &mut nodes,
                );
            }
        }
        let keys =
            Proofs::key_paths(deleted.iter().map(|(address, slots)| (address, slots.iter())));
        Ok((Proofs { nodes, codes: Vec::new(), keys }, branch))
    }

    /// Fetches proofs of `misses` at the stored tip from the upstream and stores what links to
    /// the tip's state root.
    fn absorb_proofs(&self, chain: &crate::Chain, misses: &crate::Misses) -> Result<(), String> {
        Self::absorb(chain, &self.fetch_proofs(chain.head().hash, misses)?)
    }

    /// Stores what `proofs` reveal of the stored tip's state: their nodes that link by hash to
    /// its state root, also through stored nodes when the proofs were taken at an older tip.
    pub fn absorb(chain: &crate::Chain, proofs: &Proofs) -> Result<(), String> {
        let mut w = chain.db.write().map_err(|error| error.to_string())?;
        chain
            .db
            .absorb_proofs(&mut w, &proofs.nodes, &proofs.codes, &proofs.keys)
            .map_err(|error| error.to_string())?;
        w.commit().map_err(|error| error.to_string())
    }

    /// The upstream's proofs of `misses` at block `block`, with the missed code (only code
    /// whose hash was missed; code no read account holds is skipped).
    pub fn fetch_proofs(&self, block: B256, misses: &crate::Misses) -> Result<Proofs, String> {
        let upstream = self.upstream.as_ref().expect("a stateless node has an upstream");
        let mut keys: BTreeMap<alloy_primitives::Address, Vec<U256>> =
            misses.accounts.iter().map(|address| (*address, Vec::new())).collect();
        for (address, slots) in &misses.storage {
            keys.entry(*address).or_default().extend(slots.iter().copied());
        }
        let mut owners = Vec::with_capacity(misses.codes.len());
        for owner in misses.codes.values().flatten() {
            keys.entry(*owner).or_default();
            owners.push(*owner);
        }
        let keys: Vec<_> = keys.into_iter().collect();
        let (nodes, codes) = upstream.proofs(block, &keys, &owners)?;
        let codes = codes
            .into_iter()
            .filter(|code| misses.codes.contains_key(&alloy_primitives::keccak256(code)))
            .collect();
        Ok(Proofs {
            nodes,
            codes,
            keys: Proofs::key_paths(
                keys.iter().map(|(address, slots)| (address, slots.as_slice())),
            ),
        })
    }

    /// Validates block `hash` on a partial state over the upstream's execution witness, which
    /// is verified node by node against the parent's state root and header by header against
    /// the parent's ancestry.
    fn validate_with_witness(
        &self,
        chain: &crate::Chain,
        witness: crate::ExecutionWitness,
        header: &Header,
        hash: B256,
        transactions: &[Bytes],
        parent: &crate::StoredBlock,
    ) -> Result<(crate::Validated, Option<Validation>), RpcError> {
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
            .filter(|_| {
                self.spec.is_active(BaseFeature::L2ToL1MessageRoot, parent.header.timestamp)
            })
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
        engine.forkchoice_head = Some(head);
        let mut chain = self.chain.write().expect("chain lock");
        let Some(head_block) = chain.block(&head) else {
            drop(chain);
            self.request_sync(head);
            return Ok(
                json!({ "payloadStatus": status("SYNCING", None, None), "payloadId": null }),
            );
        };
        for hash in [state.safe_block_hash, state.finalized_block_hash] {
            if !hash.is_zero() && chain.block(&hash).is_none() {
                return Ok(
                    json!({ "payloadStatus": status("SYNCING", None, None), "payloadId": null }),
                );
            }
        }
        let update =
            Forkchoice { head, safe: state.safe_block_hash, finalized: state.finalized_block_hash };
        let previous = chain.forkchoice();
        if head != chain.head().hash && chain.is_canonical(&head) {
            // base-reth on Base: a canonical ancestor keeps the head, safe and finalized; it only
            // checks that safe and finalized are canonical at or below it, and may build on it.
            for hash in [update.safe, update.finalized] {
                let below = chain.block(&hash).is_some_and(|block| block.number() <= head_block.number());
                if !hash.is_zero() && !(below && chain.is_canonical(&hash)) {
                    return Err(RpcError::new(-38002, "Invalid forkchoice state"));
                }
            }
        } else {
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
                (
                    self.build(&head_block.header, &attributes, &state)?,
                    state.take_prepared().map(|prepared| Validation::Prepared(Box::new(prepared))),
                )
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
    fn store_built(
        &self,
        built: BuiltPayload,
        prepared: Option<Validation>,
    ) -> Result<BuiltPayload, RpcError> {
        self.chain
            .write()
            .expect("chain lock")
            .insert(built.hash, &built.executed, prepared)
            .map_err(RpcError::internal)?;
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
        .map_err(|error| {
            if error.code == -38005 { error } else { RpcError::new(-38005, "Unsupported fork") }
        })?;
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

/// Proof nodes and code from the upstream, with the keys they prove as trie paths.
#[derive(Debug, Default)]
pub struct Proofs {
    /// Trie nodes, untrusted until they link by hash to a known root.
    pub nodes: Vec<Bytes>,
    /// Bytecode, stored by its hash.
    pub codes: Vec<Bytes>,
    /// The proved keys: hashed addresses in the account trie (`None`), hashed slots by hashed
    /// address.
    pub keys: BTreeMap<Option<B256>, Vec<Nibbles>>,
}

impl Proofs {
    /// The trie paths of accounts with their storage slots.
    pub fn key_paths<'a, S: IntoIterator<Item = &'a U256>>(
        keys: impl Iterator<Item = (&'a alloy_primitives::Address, S)>,
    ) -> BTreeMap<Option<B256>, Vec<Nibbles>> {
        let mut paths: BTreeMap<Option<B256>, Vec<Nibbles>> = BTreeMap::new();
        for (address, slots) in keys {
            let hashed = alloy_primitives::keccak256(address);
            paths.entry(None).or_default().push(Nibbles::unpack(hashed));
            let storage = paths.entry(Some(hashed)).or_default();
            storage.extend(
                slots
                    .into_iter()
                    .map(|slot| Nibbles::unpack(alloy_primitives::keccak256(B256::from(*slot)))),
            );
        }
        paths
    }
}

/// The nodes a deletion's collapsed sibling may have been before the block: `proof` is the
/// post-state proof (root first) of hashed key `target`, and its last node on the key's path, a
/// leaf or an extension at path `p` with key `k`, is re-keyed as the node at each longer path
/// `p ++ k[..i]` it covers, with key `k[i..]`. Only nodes referenced by hash (32 bytes of RLP or
/// more) are returned; the caller keeps those that link to a known root. The flag is set when
/// the end is an extension: the sibling may be the branch it points to, which no candidate is.
fn collapse_candidates(target: B256, proof: &[Bytes]) -> (Vec<Bytes>, bool) {
    let target = Nibbles::unpack(target);
    let mut depth = 0;
    let mut end = None;
    for rlp in proof {
        let Ok(node) = TrieNode::decode(&mut &rlp[..]) else { return (Vec::new(), false) };
        match node {
            TrieNode::Branch(_) if depth < target.len() => depth += 1,
            TrieNode::Extension(extension) if target.slice(depth..).starts_with(&extension.key) => {
                depth += extension.key.len();
            }
            TrieNode::Leaf(_) | TrieNode::Extension(_) => {
                end = Some(node);
                break;
            }
            TrieNode::Branch(_) | TrieNode::EmptyRoot => break,
        }
    }
    let mut candidates = Vec::new();
    let key = match &end {
        Some(TrieNode::Leaf(leaf)) => leaf.key,
        Some(TrieNode::Extension(extension)) => extension.key,
        _ => return (candidates, false),
    };
    assert!(depth + key.len() <= 64, "a trie path is at most 64 nibbles");
    for split in 1..key.len() {
        let rest = key.slice(split..);
        let rlp = match &end {
            Some(TrieNode::Leaf(leaf)) => {
                alloy_rlp::encode(LeafNode::new(rest, leaf.value.clone()))
            }
            Some(TrieNode::Extension(extension)) => {
                alloy_rlp::encode(ExtensionNode::new(rest, extension.child.clone()))
            }
            _ => unreachable!("the end node is a leaf or an extension"),
        };
        if rlp.len() >= 32 {
            candidates.push(Bytes::from(rlp));
        }
    }
    (candidates, matches!(end, Some(TrieNode::Extension(_))))
}

#[cfg(test)]
mod tests {
    use alloy_trie::{HashBuilder, proof::ProofRetainer};

    use super::*;

    /// The nodes of a trie over `leaves` (sorted) on the path of `target`, root first.
    fn proof(leaves: &[(B256, Vec<u8>)], target: B256) -> Vec<(Nibbles, Bytes)> {
        let mut builder = HashBuilder::default()
            .with_proof_retainer(ProofRetainer::new(vec![Nibbles::unpack(target)]));
        for (key, value) in leaves {
            builder.add_leaf(Nibbles::unpack(key), value);
        }
        builder.root();
        builder.take_proof_nodes().into_nodes_sorted()
    }

    /// Deleting a leaf collapses its branch into the sibling leaf, which moves up and merges
    /// with the extension above; the deleted key's post-state proof yields the sibling's
    /// pre-state node; a branch sibling, which no proof reveals, is flagged instead.
    #[test]
    fn collapse_candidates_recover_the_pre_state_sibling() {
        let key = |hex: &str| {
            B256::from_slice(&alloy_primitives::hex::decode(format!("{hex:0<64}")).unwrap())
        };
        let (deleted, sibling, other) = (key("12a1"), key("12b7"), key("5c"));
        let value = |byte: u8| alloy_rlp::encode(U256::from(byte) << 200);
        let before = [(deleted, value(1)), (sibling, value(2)), (other, value(3))];
        let after = [(sibling, value(2)), (other, value(3))];
        let pre_sibling = proof(&before, sibling).pop().expect("the sibling leaf");
        assert_eq!(pre_sibling.0.len(), 3, "the sibling sits below the branch at 0x12");

        let (candidates, branch) = collapse_candidates(
            deleted,
            &proof(&after, deleted).into_iter().map(|(_, n)| n).collect::<Vec<_>>(),
        );
        assert!(!branch);
        assert!(candidates.contains(&pre_sibling.1), "the pre-state sibling is a candidate");

        // A sibling that is a branch: the post-state ends at an extension into it.
        let after = [(key("12b1"), value(2)), (key("12b2"), value(4)), (other, value(3))];
        let (_, branch) = collapse_candidates(
            deleted,
            &proof(&after, deleted).into_iter().map(|(_, n)| n).collect::<Vec<_>>(),
        );
        assert!(branch, "a branch sibling is flagged for the witness");
    }
}
