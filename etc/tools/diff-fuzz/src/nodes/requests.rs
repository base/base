//! Generated JSON-RPC requests over what a followed chain holds.

use alloy_consensus::Transaction;
use alloy_eips::Typed2718;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use base_common_consensus::BaseTxEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    Keys, Rng, WorldState,
    nodes::{ChainIndex, ChainTx},
};

/// One generated request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcCase {
    /// Method family the case exercises, for statistics.
    pub family: String,
    /// The JSON-RPC method (for a malformed case, the method it was derived from).
    pub method: String,
    /// The exact request body; malformed cases may not be valid JSON.
    pub body: String,
    /// Coarse parameter shape, the coverage key counted per method (see [`Self::shape_of`]).
    pub shape: String,
}

impl RpcCase {
    /// Most keys of an object listed in a shape before it is truncated.
    const MAX_OBJECT_KEYS: usize = 6;

    /// A well-formed single request.
    pub fn new(family: &str, method: &str, params: Value) -> Self {
        let shape = Self::shape_of(&params);
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        Self { family: family.to_owned(), method: method.to_owned(), body: body.to_string(), shape }
    }

    /// The coarse shape of a parameter list: one kind per parameter, joined with `,`. Block tags
    /// keep their name; quantities, hashes, and addresses become `number`, `hash`, `address`;
    /// tracer configs become `tracer:<name>` (`tracer:struct` for the opcode logger) with the
    /// tracer options set to true and `+so` / `+bo` when they carry state or block overrides;
    /// call objects become `call` or `create`; override objects become `stateOverrides` or
    /// `blockOverrides`; `eth_simulateV1` payloads list their true flags; other objects (log
    /// filters, EIP-1898 block parameters) list their sorted keys.
    pub fn shape_of(params: &Value) -> String {
        match params {
            Value::Array(items) => items.iter().map(Self::kind).collect::<Vec<_>>().join(","),
            other => Self::kind(other),
        }
    }

    fn kind(value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Number(_) => "int".into(),
            Value::String(text) => match text.as_str() {
                "latest" | "earliest" | "pending" | "safe" | "finalized" => text.clone(),
                t if t.len() == 66 && t.starts_with("0x") => "hash".into(),
                t if t.len() == 42 && t.starts_with("0x") => "address".into(),
                t if t.starts_with("0x") && t.len() <= 18 => "number".into(),
                t if t.starts_with("0x") => "data".into(),
                _ => "string".into(),
            },
            Value::Array(items) if items.is_empty() => "[]".into(),
            Value::Array(items) => format!("[{}]", Self::kind(&items[0])),
            Value::Object(object) => {
                if object.contains_key("blockStateCalls") {
                    let flags: Vec<&str> =
                        ["validation", "traceTransfers", "returnFullTransactions"]
                            .into_iter()
                            .filter(|f| object.get(*f) == Some(&json!(true)))
                            .collect();
                    return format!("simulate[{}]", flags.join("+"));
                }
                let tracer =
                    object.get("tracer").and_then(Value::as_str).map(str::to_owned).or_else(|| {
                        ["disableStack", "disableStorage", "enableMemory", "enableReturnData"]
                            .iter()
                            .any(|k| object.contains_key(*k))
                            .then(|| "struct".to_owned())
                    });
                if let Some(tracer) = tracer {
                    let mut out = format!("tracer:{tracer}");
                    if let Some(Value::Object(config)) = object.get("tracerConfig") {
                        for (key, value) in config {
                            if value == &json!(true) {
                                out.push_str(&format!("+{key}"));
                            }
                        }
                    }
                    if object.contains_key("stateOverrides") {
                        out.push_str("+so");
                    }
                    if object.contains_key("blockOverrides") {
                        out.push_str("+bo");
                    }
                    return out;
                }
                if !object.is_empty() && object.keys().all(|k| k.len() == 42 && k.starts_with("0x"))
                {
                    return "stateOverrides".into();
                }
                const BLOCK_OVERRIDES: [&str; 11] = [
                    "number",
                    "time",
                    "gasLimit",
                    "feeRecipient",
                    "prevRandao",
                    "baseFeePerGas",
                    "blobBaseFee",
                    "coinbase",
                    "random",
                    "baseFee",
                    "difficulty",
                ];
                if !object.is_empty()
                    && object.keys().all(|k| BLOCK_OVERRIDES.contains(&k.as_str()))
                {
                    return "blockOverrides".into();
                }
                if object.contains_key("input")
                    || object.contains_key("data")
                    || object.contains_key("from")
                {
                    return if object.contains_key("to") { "call".into() } else { "create".into() };
                }
                let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
                keys.sort();
                if keys.len() > Self::MAX_OBJECT_KEYS {
                    keys.truncate(Self::MAX_OBJECT_KEYS);
                    keys.push("...");
                }
                format!("{{{}}}", keys.join(","))
            }
        }
    }

    /// The parameters of a well-formed case.
    pub fn params(&self) -> Option<Value> {
        serde_json::from_str::<Value>(&self.body).ok()?.get("params").cloned()
    }
}

/// Method families and their relative weights.
pub const FAMILIES: [(&str, u64); 13] = [
    ("state", 14),
    ("call", 14),
    ("estimate", 8),
    ("accessList", 6),
    ("simulate", 6),
    ("logs", 8),
    ("blocks", 8),
    ("txs", 8),
    ("fees", 4),
    ("trace", 10),
    ("base", 3),
    ("misc", 2),
    ("malformed", 9),
];

/// Generates requests from the chain index and the head state.
#[derive(Debug)]
pub struct RpcGen<'a> {
    /// Random stream.
    pub rng: &'a mut Rng,
    /// The chain the nodes follow.
    pub index: &'a ChainIndex,
    /// State at the head.
    pub state: &'a WorldState,
}

impl<'a> RpcGen<'a> {
    /// A hex quantity.
    pub fn q(value: u64) -> String {
        format!("{value:#x}")
    }

    /// A random case from a weighted family; `only` restricts the families.
    pub fn case(&mut self, only: &[String]) -> RpcCase {
        let families: Vec<(&str, u64)> = FAMILIES
            .iter()
            .copied()
            .filter(|(name, _)| only.is_empty() || only.iter().any(|o| o == name))
            .collect();
        let weights: Vec<u64> = families.iter().map(|(_, w)| *w).collect();
        let family = families[self.rng.weighted(&weights)].0;
        self.family(family)
    }

    /// A random case of one family.
    pub fn family(&mut self, family: &str) -> RpcCase {
        match family {
            "state" => self.state_read(),
            "call" => {
                let (call, block, valid) = self.call_and_block();
                let mut params = vec![call, block];
                if self.rng.chance(40) {
                    params.push(self.overrides(valid));
                    if self.rng.chance(50) {
                        params.push(self.block_overrides_for(valid));
                    }
                } else if self.rng.chance(8) {
                    params.push(Value::Null);
                    params.push(self.block_overrides_for(valid));
                }
                RpcCase::new(family, "eth_call", Value::Array(params))
            }
            "estimate" => {
                let (call, block, valid) = self.call_and_block();
                let mut params = vec![call];
                if self.rng.chance(85) {
                    params.push(block);
                    if self.rng.chance(30) {
                        params.push(self.overrides(valid));
                        if self.rng.chance(40) {
                            params.push(self.block_overrides_for(valid));
                        }
                    }
                }
                RpcCase::new(family, "eth_estimateGas", Value::Array(params))
            }
            "accessList" => {
                let (call, block, valid) = self.call_and_block();
                let mut params = vec![call];
                if self.rng.chance(85) {
                    params.push(block);
                    if self.rng.chance(20) {
                        params.push(self.overrides(valid));
                    }
                }
                RpcCase::new(family, "eth_createAccessList", Value::Array(params))
            }
            "simulate" => self.simulate(),
            "logs" => RpcCase::new(family, "eth_getLogs", json!([self.log_filter()])),
            "blocks" => self.block_lookup(),
            "txs" => self.tx_lookup(),
            "fees" => self.fees(),
            "trace" => self.trace(),
            "base" => self.base(),
            "misc" => {
                // `eth_accounts` and `eth_protocolVersion` are left out: their answers are each
                // node's own (keystore, devp2p version), not chain data.
                let method = *self.rng.pick(&[
                    "eth_chainId",
                    "net_version",
                    "eth_syncing",
                    "eth_blockNumber",
                    "eth_config",
                ]);
                RpcCase::new(family, method, json!([]))
            }
            "malformed" => self.malformed(),
            other => panic!("unknown family {other}"),
        }
    }

    // ---- building blocks -------------------------------------------------------------------

    fn genesis(&self) -> u64 {
        self.index.blocks[0].number
    }

    fn head(&self) -> u64 {
        self.index.head().number
    }

    /// A block number from just before genesis to just past the head, mostly inside. A share sits
    /// just below the head, where nodes still hold the blocks in memory.
    fn block_number(&mut self) -> u64 {
        let (genesis, head) = (self.genesis(), self.head());
        match self.rng.below(20) {
            0 => genesis.saturating_sub(self.rng.range(1, 3)),
            1 => head + self.rng.range(1, 3),
            2 => 0,
            3 => genesis,
            4 | 5 => head,
            6..9 => head.saturating_sub(self.rng.range(1, 64)).max(genesis),
            _ => self.rng.range(genesis, head),
        }
    }

    fn block_hash(&mut self) -> B256 {
        if self.rng.chance(8) {
            return self.rng.b256();
        }
        let i = self.rng.index(self.index.blocks.len());
        self.index.blocks[i].hash
    }

    /// A block number or tag (for methods that take no block hash).
    fn block_number_or_tag(&mut self) -> Value {
        match self.rng.below(12) {
            0 => json!("latest"),
            1 => json!("earliest"),
            2 => json!("pending"),
            3 => json!("safe"),
            4 => json!("finalized"),
            _ => json!(Self::q(self.block_number())),
        }
    }

    /// An EIP-1898 block parameter.
    fn block_param(&mut self) -> Value {
        match self.rng.below(16) {
            0 => json!({ "blockNumber": Self::q(self.block_number()) }),
            1 => json!({ "blockHash": self.block_hash() }),
            2 => json!({ "blockHash": self.block_hash(), "requireCanonical": self.rng.chance(50) }),
            _ => self.block_number_or_tag(),
        }
    }

    fn address(&mut self) -> Address {
        match self.rng.below(100) {
            0..40 if !self.state.accounts.is_empty() => {
                let i = self.rng.index(self.state.accounts.len());
                *self.state.accounts.keys().nth(i).expect("in range")
            }
            40..55 => Keys::get().address(self.rng.index(Keys::COUNT)),
            55..63 => {
                let n = *self
                    .rng
                    .pick(&[1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 0x100]);
                Address::from_word(B256::from(U256::from(n)))
            }
            63..73 if !self.index.log_addresses.is_empty() => {
                let i = self.rng.index(self.index.log_addresses.len());
                *self.index.log_addresses.iter().nth(i).expect("in range")
            }
            73..85 if !self.index.txs.is_empty() => {
                let tx = &self.index.txs[self.rng.index(self.index.txs.len())];
                match ChainIndex::decode(tx).and_then(|t| t.to()) {
                    Some(to) if self.rng.chance(50) => to,
                    _ => tx.from,
                }
            }
            85..88 => Address::ZERO,
            _ => self.rng.address(),
        }
    }

    fn slot(&mut self, address: &Address) -> B256 {
        if let Some(account) = self.state.account(address)
            && !account.storage.is_empty()
            && self.rng.chance(70)
        {
            let i = self.rng.index(account.storage.len());
            return *account.storage.keys().nth(i).expect("in range");
        }
        match self.rng.below(4) {
            0 => B256::from(U256::from(self.rng.below(16))),
            1 => keccak256(self.rng.b256()),
            2 => B256::from(U256::MAX),
            _ => self.rng.b256(),
        }
    }

    fn tx(&mut self) -> Option<&ChainTx> {
        if self.index.txs.is_empty() {
            return None;
        }
        Some(&self.index.txs[self.rng.index(self.index.txs.len())])
    }

    fn quantity_u256(&mut self) -> String {
        format!(
            "{:#x}",
            match self.rng.below(6) {
                0 => U256::ZERO,
                1 => self.rng.edge_u256(),
                2 => U256::from(self.rng.below(1 << 40)),
                _ =>
                    U256::from(self.rng.below(1000))
                        * U256::from(10).pow(U256::from(self.rng.range(9, 20))),
            }
        )
    }

    fn data(&mut self) -> Bytes {
        match self.rng.below(5) {
            0 => Bytes::new(),
            1 if !self.state.codes.is_empty() => {
                let i = self.rng.index(self.state.codes.len());
                self.state.codes.values().nth(i).expect("in range").clone()
            }
            _ => {
                let len = *self.rng.pick(&[4usize, 4, 36, 68, 100, 7, 200]);
                Bytes::from(self.rng.bytes(len))
            }
        }
    }

    /// A transaction call object: mostly a transaction of the chain turned into a call, with
    /// fields dropped or changed.
    fn call_object(&mut self) -> Value {
        let mut call = Map::new();
        let from_tx = self.tx().and_then(ChainIndex::decode).zip(self.tx().map(|t| t.from));
        if let Some((tx, from)) = from_tx.filter(|_| self.rng.chance(60)) {
            call.insert("from".into(), json!(from));
            if let Some(to) = tx.to() {
                call.insert("to".into(), json!(to));
            }
            call.insert("input".into(), json!(tx.input()));
            call.insert("value".into(), json!(format!("{:#x}", tx.value())));
            call.insert("gas".into(), json!(Self::q(tx.gas_limit())));
            if !matches!(tx, BaseTxEnvelope::Deposit(_)) {
                call.insert("nonce".into(), json!(Self::q(tx.nonce())));
                if tx.is_dynamic_fee() {
                    call.insert(
                        "maxFeePerGas".into(),
                        json!(format!("{:#x}", tx.max_fee_per_gas())),
                    );
                    call.insert(
                        "maxPriorityFeePerGas".into(),
                        json!(format!("{:#x}", tx.max_priority_fee_per_gas().unwrap_or_default())),
                    );
                } else {
                    call.insert("gasPrice".into(), json!(format!("{:#x}", tx.max_fee_per_gas())));
                }
                call.insert("type".into(), json!(Self::q(u64::from(tx.ty()))));
                if let Some(list) = tx.access_list() {
                    call.insert("accessList".into(), json!(list));
                }
                if let Some(auths) = tx.authorization_list() {
                    call.insert("authorizationList".into(), json!(auths));
                }
                call.insert("chainId".into(), json!(Self::q(8453)));
            }
        } else {
            if self.rng.chance(80) {
                call.insert("from".into(), json!(self.address()));
            }
            if self.rng.chance(85) {
                call.insert("to".into(), json!(self.address()));
            }
            let data = self.data();
            call.insert(if self.rng.chance(70) { "input" } else { "data" }.into(), json!(data));
            if self.rng.chance(40) {
                call.insert("value".into(), json!(self.quantity_u256()));
            }
            if self.rng.chance(50) {
                let gas = *self.rng.pick(&[
                    21_000u64,
                    0,
                    1,
                    100_000,
                    1_000_000,
                    30_000_000,
                    u64::MAX,
                    50_000_000_000,
                ]);
                call.insert("gas".into(), json!(Self::q(gas)));
            }
            match self.rng.below(4) {
                0 => {
                    call.insert("gasPrice".into(), json!(self.quantity_u256()));
                }
                1 => {
                    call.insert("maxFeePerGas".into(), json!(self.quantity_u256()));
                    if self.rng.chance(70) {
                        call.insert("maxPriorityFeePerGas".into(), json!(self.quantity_u256()));
                    }
                }
                _ => {}
            }
        }
        // Mutations.
        if self.rng.chance(10) {
            call.remove("gas");
        }
        if self.rng.chance(8) {
            call.remove("nonce");
        }
        if self.rng.chance(5) {
            call.remove("from");
        }
        if self.rng.chance(4) {
            let input = call.get("input").cloned().unwrap_or_else(|| json!("0x"));
            let data = if self.rng.chance(50) { input } else { json!(self.data()) };
            call.insert("data".into(), data);
        }
        if self.rng.chance(4) {
            call.insert("gasPrice".into(), json!(self.quantity_u256()));
        }
        if self.rng.chance(4) {
            call.insert("blobVersionedHashes".into(), json!([self.rng.b256()]));
            if self.rng.chance(50) {
                call.insert("maxFeePerBlobGas".into(), json!(self.quantity_u256()));
            }
        }
        if self.rng.chance(4) {
            call.insert("value".into(), json!(self.quantity_u256()));
        }
        if self.rng.chance(3) {
            call.insert("nonce".into(), json!(Self::q(self.rng.below(100))));
        }
        if self.rng.chance(3) {
            let ty = *self.rng.pick(&[0u64, 1, 2, 3, 4, 0x7e, 0x79, 0x55]);
            call.insert("type".into(), json!(Self::q(ty)));
        }
        if self.rng.chance(2) {
            call.insert("chainId".into(), json!(Self::q(self.rng.below(100_000))));
        }
        if self.rng.chance(4) {
            let address = self.address();
            let keys: Vec<B256> = (0..self.rng.below(3)).map(|_| self.slot(&address)).collect();
            call.insert("accessList".into(), json!([{ "address": address, "storageKeys": keys }]));
        }
        Value::Object(call)
    }

    fn state_overrides(&mut self) -> Value {
        let mut overrides = Map::new();
        for _ in 0..self.rng.range(1, 3) {
            let address = self.address();
            let mut entry = Map::new();
            if self.rng.chance(40) {
                entry.insert("balance".into(), json!(self.quantity_u256()));
            }
            if self.rng.chance(20) {
                let nonce = *self.rng.pick(&[0u64, 1, 7, u64::MAX, u64::MAX - 1]);
                entry.insert("nonce".into(), json!(Self::q(nonce)));
            }
            if self.rng.chance(30) {
                entry.insert("code".into(), json!(self.data()));
            }
            let slots = |g: &mut Self| {
                let mut map = Map::new();
                for _ in 0..g.rng.range(1, 3) {
                    let slot = g.slot(&address);
                    let value = if g.rng.chance(30) { B256::ZERO } else { g.rng.b256() };
                    map.insert(slot.to_string(), json!(value));
                }
                Value::Object(map)
            };
            match self.rng.below(10) {
                0 | 1 => {
                    entry.insert("state".into(), slots(self));
                }
                2 | 3 => {
                    entry.insert("stateDiff".into(), slots(self));
                }
                4 => {
                    entry.insert("state".into(), slots(self));
                    entry.insert("stateDiff".into(), slots(self));
                }
                _ => {}
            }
            if self.rng.chance(4) {
                let to = self.rng.address();
                entry.insert("movePrecompileToAddress".into(), json!(to));
            }
            overrides.insert(format!("{address:#x}"), Value::Object(entry));
        }
        Value::Object(overrides)
    }

    fn block_overrides(&mut self) -> Value {
        let mut overrides = Map::new();
        let head = self.head();
        let timestamp = self.index.head().timestamp;
        if self.rng.chance(40) {
            let number = *self.rng.pick(&[head + 1, head, head + 1000, 0, u64::MAX]);
            overrides.insert("number".into(), json!(Self::q(number)));
        }
        if self.rng.chance(40) {
            let time =
                *self.rng.pick(&[timestamp + 2, timestamp, 0, timestamp + 1_000_000, u64::MAX]);
            overrides.insert("time".into(), json!(Self::q(time)));
        }
        if self.rng.chance(30) {
            let gas = *self.rng.pick(&[0u64, 21_000, 30_000_000, u64::MAX]);
            overrides.insert("gasLimit".into(), json!(Self::q(gas)));
        }
        if self.rng.chance(30) {
            overrides.insert("feeRecipient".into(), json!(self.address()));
        }
        if self.rng.chance(30) {
            overrides.insert("prevRandao".into(), json!(self.rng.b256()));
        }
        if self.rng.chance(30) {
            overrides.insert("baseFeePerGas".into(), json!(self.quantity_u256()));
        }
        if self.rng.chance(20) {
            overrides.insert("blobBaseFee".into(), json!(self.quantity_u256()));
        }
        if self.rng.chance(8) {
            let alias = *self.rng.pick(&["coinbase", "random", "baseFee", "difficulty"]);
            overrides.insert(alias.into(), json!(Self::q(self.rng.below(1 << 30))));
        }
        Value::Object(overrides)
    }

    // ---- requests meant to succeed -------------------------------------------------------------
    //
    // Fuzzed call objects mostly fail validation (unfunded senders, fee fields past 2^128, blocks
    // outside the chain), so the success paths of the call methods would see little traffic. A
    // share of cases is built to pass: chain transactions replayed on their parent block, and
    // funded accounts calling contracts near the head.

    /// Share of call-method cases (percent) built to succeed.
    const VALID_SHARE: u64 = 60;

    /// A call object and block parameter, and whether they were built to succeed.
    fn call_and_block(&mut self) -> (Value, Value, bool) {
        if self.rng.chance(Self::VALID_SHARE) {
            let (call, block) = self.valid_call();
            (call, block, true)
        } else {
            (self.call_object(), self.block_param(), false)
        }
    }

    /// State overrides: well-formed ones for a valid case.
    fn overrides(&mut self, valid: bool) -> Value {
        if valid { self.valid_state_overrides() } else { self.state_overrides() }
    }

    /// Block overrides: well-formed ones for a valid case.
    fn block_overrides_for(&mut self, valid: bool) -> Value {
        if valid { self.valid_block_overrides() } else { self.block_overrides() }
    }

    fn valid_call(&mut self) -> (Value, Value) {
        if self.rng.chance(70)
            && let Some(pair) = self.replay_call()
        {
            return pair;
        }
        self.funded_call()
    }

    /// A block parameter naming block `number` of the chain, in any form: a quantity, an
    /// EIP-1898 object by number or by hash (with or without `requireCanonical`), or the tag that
    /// names it.
    fn block_ref(&mut self, number: u64) -> Value {
        let head = self.head();
        let hash = self.index.blocks[(number - self.genesis()) as usize].hash;
        match self.rng.below(10) {
            0 => json!({ "blockNumber": Self::q(number) }),
            1 => json!({ "blockHash": hash }),
            2 => json!({ "blockHash": hash, "requireCanonical": self.rng.chance(50) }),
            3 | 4 if number == head => json!(*self.rng.pick(&["latest", "pending"])),
            3 | 4 if number + 4 == head => json!("safe"),
            3 | 4 if number + 16 == head => json!("finalized"),
            _ => json!(Self::q(number)),
        }
    }

    /// A random successful transaction of the chain after genesis.
    fn successful_tx(&mut self) -> Option<&'a ChainTx> {
        let index = self.index;
        let genesis = self.genesis();
        (0..16).find_map(|_| {
            let tx = index.txs.get(self.rng.index(index.txs.len().max(1)))?;
            (tx.status == 1 && tx.block > genesis).then_some(tx)
        })
    }

    /// `tx` as a call object; deposits keep only what a call can express and need a zero value,
    /// since a call mints nothing.
    /// With `gas`, the call always carries the transaction's gas limit: `eth_simulateV1` gives a
    /// call without one the block's remaining gas, which a long block runs out of.
    fn tx_call(&mut self, tx: &ChainTx, gas: bool) -> Option<Value> {
        let decoded = ChainIndex::decode(tx)?;
        let mut call = Map::new();
        call.insert("from".into(), json!(tx.from));
        if let Some(to) = decoded.to() {
            call.insert("to".into(), json!(to));
        }
        call.insert(
            if self.rng.chance(80) { "input" } else { "data" }.into(),
            json!(decoded.input()),
        );
        call.insert("value".into(), json!(format!("{:#x}", decoded.value())));
        if gas || self.rng.chance(70) {
            call.insert("gas".into(), json!(Self::q(decoded.gas_limit())));
        }
        if matches!(decoded, BaseTxEnvelope::Deposit(_)) {
            return decoded.value().is_zero().then_some(Value::Object(call));
        }
        if self.rng.chance(30) {
            call.insert("nonce".into(), json!(Self::q(decoded.nonce())));
        }
        if self.rng.chance(50) {
            if decoded.is_dynamic_fee() {
                call.insert(
                    "maxFeePerGas".into(),
                    json!(format!("{:#x}", decoded.max_fee_per_gas())),
                );
                call.insert(
                    "maxPriorityFeePerGas".into(),
                    json!(format!("{:#x}", decoded.max_priority_fee_per_gas().unwrap_or_default())),
                );
            } else {
                call.insert("gasPrice".into(), json!(format!("{:#x}", decoded.max_fee_per_gas())));
            }
        }
        if self.rng.chance(50) {
            call.insert("type".into(), json!(Self::q(u64::from(decoded.ty()))));
            if let Some(list) = decoded.access_list() {
                call.insert("accessList".into(), json!(list));
            }
            if let Some(auths) = decoded.authorization_list() {
                call.insert("authorizationList".into(), json!(auths));
            }
        }
        Some(Value::Object(call))
    }

    /// A successful transaction replayed on its parent block's state.
    fn replay_call(&mut self) -> Option<(Value, Value)> {
        let tx = self.successful_tx()?;
        let call = self.tx_call(tx, false)?;
        Some((call, self.block_ref(tx.block - 1)))
    }

    /// A random account holding a balance at the head.
    fn funded_account(&mut self) -> Option<Address> {
        let accounts = &self.state.accounts;
        (0..16).find_map(|_| {
            let (address, account) = accounts.iter().nth(self.rng.index(accounts.len().max(1)))?;
            (!account.balance.is_zero()).then_some(*address)
        })
    }

    /// A funded account calling a contract with the input some chain transaction sent it, at or
    /// near the head.
    fn funded_call(&mut self) -> (Value, Value) {
        let mut call = Map::new();
        if let Some(from) = self.funded_account() {
            call.insert("from".into(), json!(from));
        }
        let index = self.index;
        let target = (0..16).find_map(|_| {
            let tx = index.txs.get(self.rng.index(index.txs.len().max(1)))?;
            let decoded = ChainIndex::decode(tx)?;
            let to = decoded.to()?;
            let account = self.state.account(&to)?;
            (account.code_hash != alloy_primitives::KECCAK256_EMPTY)
                .then(|| (to, decoded.input().clone()))
        });
        match target {
            Some((to, input)) => {
                call.insert("to".into(), json!(to));
                call.insert("input".into(), json!(input));
            }
            None => {
                call.insert("to".into(), json!(self.address()));
                call.insert("input".into(), json!(self.data()));
            }
        }
        if self.rng.chance(20) {
            call.insert("value".into(), json!(Self::q(self.rng.below(1_000_000))));
        }
        let head = self.head();
        let block = match self.rng.below(6) {
            0 => json!("latest"),
            1 => json!("pending"),
            2 => json!("safe"),
            3 => json!("finalized"),
            _ => self.block_ref(head),
        };
        (Value::Object(call), block)
    }

    /// State overrides every client should accept: balance, nonce, code, and either `state` or
    /// `stateDiff` per account.
    fn valid_state_overrides(&mut self) -> Value {
        let mut overrides = Map::new();
        for _ in 0..self.rng.range(1, 3) {
            let address = if self.rng.chance(50) {
                self.funded_account().unwrap_or_else(|| self.address())
            } else {
                self.address()
            };
            let mut entry = Map::new();
            if self.rng.chance(50) {
                entry.insert(
                    "balance".into(),
                    json!(format!("{:#x}", U256::from(10).pow(U256::from(self.rng.range(15, 30))))),
                );
            }
            if self.rng.chance(20) {
                entry.insert("nonce".into(), json!(Self::q(self.rng.below(100))));
            }
            if self.rng.chance(30) && !self.state.codes.is_empty() {
                let i = self.rng.index(self.state.codes.len());
                entry.insert(
                    "code".into(),
                    json!(self.state.codes.values().nth(i).expect("in range")),
                );
            }
            if self.rng.chance(50) {
                let mut slots = Map::new();
                for _ in 0..self.rng.range(1, 3) {
                    let slot = self.slot(&address);
                    slots.insert(slot.to_string(), json!(self.rng.b256()));
                }
                let key = if self.rng.chance(50) { "state" } else { "stateDiff" };
                entry.insert(key.into(), Value::Object(slots));
            }
            overrides.insert(format!("{address:#x}"), Value::Object(entry));
        }
        Value::Object(overrides)
    }

    /// Block overrides every client should accept: the next block's number and time, and fee
    /// and context fields in range.
    fn valid_block_overrides(&mut self) -> Value {
        let mut overrides = Map::new();
        let head = self.index.head();
        if self.rng.chance(40) {
            overrides.insert("number".into(), json!(Self::q(head.number + self.rng.range(1, 3))));
        }
        if self.rng.chance(40) {
            overrides
                .insert("time".into(), json!(Self::q(head.timestamp + self.rng.range(1, 100))));
        }
        if self.rng.chance(30) {
            overrides.insert(
                "gasLimit".into(),
                json!(Self::q(*self.rng.pick(&[30_000_000u64, 60_000_000]))),
            );
        }
        if self.rng.chance(30) {
            overrides.insert("feeRecipient".into(), json!(self.address()));
        }
        if self.rng.chance(30) {
            overrides.insert("prevRandao".into(), json!(self.rng.b256()));
        }
        if self.rng.chance(30) {
            overrides.insert("baseFeePerGas".into(), json!(Self::q(self.rng.below(1_000_000_000))));
        }
        if self.rng.chance(20) {
            overrides.insert("blobBaseFee".into(), json!(Self::q(self.rng.range(1, 1000))));
        }
        Value::Object(overrides)
    }

    /// A block of the chain with a non-deposit transaction replayed by `eth_simulateV1` on its
    /// parent: the block's calls in order, optionally split over two simulated blocks.
    fn valid_simulate(&mut self) -> Option<RpcCase> {
        let index = self.index;
        if index.blocks.len() < 2 {
            return None;
        }
        let block = (0..16).find_map(|_| {
            let block = &index.blocks[self.rng.range(1, index.blocks.len() as u64 - 1) as usize];
            block.txs.iter().any(|&i| index.txs[i].tx_type != 0x7e).then_some(block)
        })?;
        let mut calls = Vec::new();
        for &i in &block.txs {
            let tx = &index.txs[i];
            if tx.tx_type != 0x7e
                && let Some(call) = self.tx_call(tx, true)
            {
                calls.push(call);
            }
        }
        let mut blocks = Vec::new();
        if calls.len() > 1 && self.rng.chance(30) {
            let rest = calls.split_off(calls.len() / 2);
            blocks.push(json!({ "calls": calls }));
            blocks.push(json!({ "calls": rest }));
        } else {
            let mut first = json!({ "calls": calls });
            if self.rng.chance(20) {
                first["stateOverrides"] = self.valid_state_overrides();
            }
            blocks.push(first);
        }
        let mut payload = json!({ "blockStateCalls": blocks });
        for flag in ["validation", "traceTransfers", "returnFullTransactions"] {
            if self.rng.chance(40) {
                payload[flag] = json!(self.rng.chance(50));
            }
        }
        let parent = self.block_ref(block.number - 1);
        Some(RpcCase::new("simulate", "eth_simulateV1", json!([payload, parent])))
    }

    /// A block number after genesis, or a tag other than `earliest`.
    fn chain_block_or_tag(&mut self) -> Value {
        if self.head() == self.genesis() {
            return json!("latest");
        }
        match self.rng.below(8) {
            0 => json!("latest"),
            1 => json!("safe"),
            2 => json!("finalized"),
            3 => json!("pending"),
            _ => json!(Self::q(self.rng.range(self.genesis() + 1, self.head()))),
        }
    }

    /// A transaction of the chain of a random type (deposit, legacy, EIP-2930, EIP-1559,
    /// EIP-7702), and its label for shapes.
    fn typed_tx(&mut self) -> Option<(&'a ChainTx, String)> {
        let index = self.index;
        let want = *self.rng.pick(&[0x7eu8, 0, 1, 2, 4]);
        let tx = (0..32)
            .find_map(|_| {
                let tx = index.txs.get(self.rng.index(index.txs.len().max(1)))?;
                (tx.tx_type == want).then_some(tx)
            })
            .or_else(|| index.txs.get(self.rng.index(index.txs.len().max(1))))?;
        let kind = match tx.tx_type {
            0x7e => "deposit",
            0 => "legacy",
            1 => "eip2930",
            2 => "eip1559",
            4 => "eip7702",
            _ => "other",
        };
        Some((tx, format!("{kind}{}", if tx.status == 0 { "-failed" } else { "" })))
    }

    /// A transaction hash (sometimes unknown) and its shape label.
    fn typed_tx_hash(&mut self) -> (B256, String) {
        match self.typed_tx() {
            Some((tx, label)) if !self.rng.chance(8) => (tx.hash, label),
            _ => (self.rng.b256(), "unknown".into()),
        }
    }

    fn tracer_config(&mut self, call: bool, valid: bool) -> Value {
        let mut config = match self.rng.below(10) {
            0 | 1 => {
                json!({ "tracer": "callTracer", "tracerConfig": { "onlyTopCall": self.rng.chance(30), "withLog": self.rng.chance(50) } })
            }
            2 | 3 => {
                json!({ "tracer": "prestateTracer", "tracerConfig": { "diffMode": self.rng.chance(50), "disableCode": self.rng.chance(20), "disableStorage": self.rng.chance(20) } })
            }
            4 => json!({ "tracer": "4byteTracer" }),
            5 => json!({ "tracer": "noopTracer" }),
            6 => json!({ "tracer": "callTracer" }),
            7 => json!({ "tracer": "flatCallTracer" }),
            _ => {
                let mut config = json!({
                    "disableStack": self.rng.chance(50),
                    "disableStorage": self.rng.chance(50),
                    "enableMemory": self.rng.chance(20),
                    "enableReturnData": self.rng.chance(50),
                });
                if self.rng.chance(10) {
                    // geth's legacy spelling; clients that only know `enableMemory` ignore it.
                    config["disableMemory"] = json!(self.rng.chance(50));
                }
                // The opcode logger writes a record per step: a 20k-step transaction with a few
                // hundred KiB of memory takes base/base past 10 GB, and without memory the reply
                // is still tens of MB per node. Every opcode trace has a step limit, which every
                // client honors.
                let limits: &[u64] = if config["enableMemory"] == json!(true) {
                    &[1, 16, 64]
                } else {
                    &[1, 64, 512]
                };
                config["limit"] = json!(*self.rng.pick(limits));
                config
            }
        };
        if call && self.rng.chance(25) {
            config["stateOverrides"] = self.overrides(valid);
        }
        if call && self.rng.chance(15) {
            config["blockOverrides"] = self.block_overrides_for(valid);
        }
        config
    }

    // ---- families ----------------------------------------------------------------------------

    fn state_read(&mut self) -> RpcCase {
        let address = self.address();
        let block = self.block_param();
        match self.rng.below(6) {
            0 => RpcCase::new("state", "eth_getBalance", json!([address, block])),
            1 => RpcCase::new("state", "eth_getCode", json!([address, block])),
            2 => RpcCase::new("state", "eth_getTransactionCount", json!([address, block])),
            3 | 4 => {
                let slot = self.slot(&address);
                let slot = if self.rng.chance(15) {
                    json!(Self::q(U256::from_be_bytes(slot.0).saturating_to::<u64>()))
                } else {
                    json!(slot)
                };
                RpcCase::new("state", "eth_getStorageAt", json!([address, slot, block]))
            }
            _ => {
                let keys: Vec<B256> = (0..self.rng.below(4)).map(|_| self.slot(&address)).collect();
                RpcCase::new("state", "eth_getProof", json!([address, keys, block]))
            }
        }
    }

    fn simulate(&mut self) -> RpcCase {
        if self.rng.chance(Self::VALID_SHARE)
            && let Some(case) = self.valid_simulate()
        {
            return case;
        }
        let mut blocks = Vec::new();
        for _ in 0..self.rng.range(1, 3) {
            let mut block = Map::new();
            if self.rng.chance(40) {
                block.insert("blockOverrides".into(), self.block_overrides());
            }
            if self.rng.chance(30) {
                block.insert("stateOverrides".into(), self.state_overrides());
            }
            let calls: Vec<Value> = (0..self.rng.below(4)).map(|_| self.call_object()).collect();
            block.insert("calls".into(), Value::Array(calls));
            blocks.push(Value::Object(block));
        }
        let mut payload = json!({ "blockStateCalls": blocks });
        for flag in ["validation", "traceTransfers", "returnFullTransactions"] {
            if self.rng.chance(40) {
                payload[flag] = json!(self.rng.chance(50));
            }
        }
        let block = self.block_param();
        RpcCase::new("simulate", "eth_simulateV1", json!([payload, block]))
    }

    fn log_filter(&mut self) -> Value {
        let mut filter = Map::new();
        let (genesis, head) = (self.genesis(), self.head());
        if self.rng.chance(20) {
            filter.insert("blockHash".into(), json!(self.block_hash()));
        } else if self.rng.chance(Self::VALID_SHARE) {
            // A range inside the chain.
            match self.rng.below(4) {
                0 => {
                    filter.insert(
                        "fromBlock".into(),
                        json!(*self.rng.pick(&["earliest", "finalized", "safe"])),
                    );
                    filter.insert("toBlock".into(), json!(*self.rng.pick(&["latest", "pending"])));
                }
                1 => {
                    filter.insert(
                        "fromBlock".into(),
                        json!(Self::q(head.saturating_sub(self.rng.below(50)).max(genesis))),
                    );
                }
                _ => {
                    let from = self.rng.range(genesis, head);
                    let to = (from + *self.rng.pick(&[0u64, 1, 5, 20, 100])).min(head);
                    filter.insert("fromBlock".into(), json!(Self::q(from)));
                    filter.insert("toBlock".into(), json!(Self::q(to)));
                }
            }
        } else {
            if self.rng.chance(80) {
                let from = self.block_number();
                filter.insert("fromBlock".into(), json!(Self::q(from)));
                if self.rng.chance(85) {
                    let span = *self.rng.pick(&[0u64, 1, 5, 20, 100, 10_000]);
                    let to =
                        if self.rng.chance(5) { from.saturating_sub(span) } else { from + span };
                    filter.insert("toBlock".into(), json!(Self::q(to.min(head + 2))));
                }
            } else {
                filter.insert("fromBlock".into(), self.block_number_or_tag());
                filter.insert("toBlock".into(), self.block_number_or_tag());
            }
        }
        let log = self
            .index
            .txs
            .get(self.rng.index(self.index.txs.len().max(1)))
            .and_then(|tx| tx.logs.first())
            .cloned();
        match self.rng.below(4) {
            0 => {}
            1 => {
                let address = log.as_ref().map_or_else(|| self.address(), |l| l.address);
                filter.insert("address".into(), json!(address));
            }
            _ => {
                let addresses: Vec<Address> =
                    (0..self.rng.range(1, 4)).map(|_| self.address()).collect();
                filter.insert("address".into(), json!(addresses));
            }
        }
        if self.rng.chance(60) {
            let mut topics = Vec::new();
            for position in 0..self.rng.range(0, 4) {
                let from_log = log.as_ref().and_then(|l| l.topics.get(position as usize)).copied();
                let topic = |g: &mut Self| match from_log {
                    Some(t) if g.rng.chance(60) => json!(t),
                    _ if !g.index.topics.is_empty() && g.rng.chance(60) => {
                        json!(g.index.topics[g.rng.index(g.index.topics.len())])
                    }
                    _ => json!(g.rng.b256()),
                };
                topics.push(match self.rng.below(5) {
                    0 => Value::Null,
                    1 => json!([topic(self), topic(self)]),
                    2 => json!([]),
                    _ => topic(self),
                });
            }
            filter.insert("topics".into(), Value::Array(topics));
        }
        Value::Object(filter)
    }

    fn block_lookup(&mut self) -> RpcCase {
        let full = self.rng.chance(50);
        match self.rng.below(12) {
            0 | 1 => RpcCase::new(
                "blocks",
                "eth_getBlockByNumber",
                json!([self.block_number_or_tag(), full]),
            ),
            2 | 3 => RpcCase::new("blocks", "eth_getBlockByHash", json!([self.block_hash(), full])),
            4 => RpcCase::new(
                "blocks",
                "eth_getBlockTransactionCountByNumber",
                json!([self.block_number_or_tag()]),
            ),
            5 => RpcCase::new(
                "blocks",
                "eth_getBlockTransactionCountByHash",
                json!([self.block_hash()]),
            ),
            6 => {
                let method = *self
                    .rng
                    .pick(&["eth_getUncleCountByBlockNumber", "eth_getUncleByBlockNumberAndIndex"]);
                let mut params = vec![self.block_number_or_tag()];
                if method.ends_with("Index") {
                    params.push(json!("0x0"));
                }
                RpcCase::new("blocks", method, Value::Array(params))
            }
            7 | 8 => RpcCase::new("blocks", "eth_getBlockReceipts", json!([self.block_param()])),
            9 => {
                let method = *self.rng.pick(&[
                    "debug_getRawHeader",
                    "debug_getRawBlock",
                    "debug_getRawReceipts",
                ]);
                RpcCase::new("blocks", method, json!([self.block_number_or_tag()]))
            }
            10 => {
                RpcCase::new("blocks", "eth_getUncleCountByBlockHash", json!([self.block_hash()]))
            }
            _ => RpcCase::new("blocks", "eth_blockNumber", json!([])),
        }
    }

    fn tx_index(&mut self) -> String {
        Self::q(*self.rng.pick(&[0u64, 1, 2, 3, 5, 10, 30, u64::from(u32::MAX)]))
    }

    fn tx_lookup(&mut self) -> RpcCase {
        let labelled = |mut case: RpcCase, label: &str| {
            case.shape = format!("{}:{label}", case.shape);
            case
        };
        match self.rng.below(10) {
            0..=4 => {
                let method = match self.rng.below(7) {
                    0 | 1 => "eth_getTransactionByHash",
                    2..=4 => "eth_getTransactionReceipt",
                    5 => "eth_getRawTransactionByHash",
                    _ => "debug_getRawTransaction",
                };
                let (hash, label) = self.typed_tx_hash();
                labelled(RpcCase::new("txs", method, json!([hash])), &label)
            }
            5..=7 => {
                let method = *self.rng.pick(&[
                    "eth_getTransactionByBlockHashAndIndex",
                    "eth_getTransactionByBlockNumberAndIndex",
                    "eth_getRawTransactionByBlockNumberAndIndex",
                    "eth_getRawTransactionByBlockHashAndIndex",
                ]);
                let by_hash = method.contains("Hash");
                match self.typed_tx().filter(|_| self.rng.chance(Self::VALID_SHARE)) {
                    Some((tx, label)) => {
                        let block = if by_hash {
                            json!(self.index.blocks[(tx.block - self.genesis()) as usize].hash)
                        } else {
                            json!(Self::q(tx.block))
                        };
                        let case = RpcCase::new("txs", method, json!([block, Self::q(tx.index)]));
                        labelled(case, &label)
                    }
                    None => {
                        let block = if by_hash {
                            json!(self.block_hash())
                        } else {
                            self.block_number_or_tag()
                        };
                        RpcCase::new("txs", method, json!([block, self.tx_index()]))
                    }
                }
            }
            _ => {
                let real = self
                    .typed_tx()
                    .filter(|(tx, _)| tx.tx_type != 0x7e && self.rng.chance(Self::VALID_SHARE))
                    .and_then(|(tx, label)| {
                        Some((tx.from, ChainIndex::decode(tx)?.nonce(), label))
                    });
                match real {
                    Some((from, nonce, label)) => labelled(
                        RpcCase::new(
                            "txs",
                            "eth_getTransactionBySenderAndNonce",
                            json!([from, Self::q(nonce)]),
                        ),
                        &label,
                    ),
                    None => RpcCase::new(
                        "txs",
                        "eth_getTransactionBySenderAndNonce",
                        json!([self.address(), Self::q(self.rng.below(20))]),
                    ),
                }
            }
        }
    }

    fn fees(&mut self) -> RpcCase {
        match self.rng.below(6) {
            0..3 => {
                let valid = self.rng.chance(Self::VALID_SHARE) && self.head() > self.genesis();
                // A valid range stays inside the chain: newest after genesis, and no more blocks
                // than genesis..=newest holds.
                let (genesis, head) = (self.genesis(), self.head());
                let (newest, newest_number) = match self.rng.below(5) {
                    0 => (json!("latest"), head),
                    1 if head >= genesis + 4 => (json!("safe"), head - 4),
                    2 if head >= genesis + 16 => (json!("finalized"), head - 16),
                    _ => {
                        let n = self.rng.range(genesis + 1, head.max(genesis + 1));
                        (json!(Self::q(n)), n)
                    }
                };
                let available = (newest_number - genesis + 1).min(64);
                let count = match self.rng.below(6) {
                    _ if valid => {
                        let n = self.rng.range(1, available);
                        if self.rng.chance(30) { json!(n) } else { json!(Self::q(n)) }
                    }
                    0 => json!(0),
                    1 => json!(self.rng.range(1, 10)),
                    2 => json!(Self::q(1025)),
                    _ => json!(Self::q(self.rng.range(1, 64))),
                };
                let percentiles = match self.rng.below(7) {
                    0 => Value::Null,
                    1 => json!([]),
                    2 if !valid => json!([90.0, 10.0]),
                    3 if !valid => json!([0.0, 100.0, 101.0]),
                    4 => json!([50]),
                    5 => json!([0.0, 100.0]),
                    _ => json!([10.0, 25.5, 50.0, 75.0, 99.9]),
                };
                let newest = if valid { newest } else { self.block_number_or_tag() };
                let mut params = vec![count, newest];
                if !percentiles.is_null() || self.rng.chance(50) {
                    params.push(percentiles);
                }
                RpcCase::new("fees", "eth_feeHistory", Value::Array(params))
            }
            3 => RpcCase::new("fees", "eth_gasPrice", json!([])),
            4 => RpcCase::new("fees", "eth_maxPriorityFeePerGas", json!([])),
            _ => RpcCase::new("fees", "eth_blobBaseFee", json!([])),
        }
    }

    fn trace(&mut self) -> RpcCase {
        match self.rng.below(8) {
            0..3 => {
                let (hash, label) = self.typed_tx_hash();
                let config = self.tracer_config(false, true);
                let mut case =
                    RpcCase::new("trace", "debug_traceTransaction", json!([hash, config]));
                case.shape = format!("{}:{label}", case.shape);
                case
            }
            3 | 4 => {
                let (call, block, valid) = self.call_and_block();
                let config = self.tracer_config(true, valid);
                RpcCase::new("trace", "debug_traceCall", json!([call, block, config]))
            }
            5 | 6 => {
                let mut config = self.tracer_config(false, true);
                if config.get("tracer").is_none() {
                    config = json!({ "tracer": "callTracer" });
                }
                RpcCase::new(
                    "trace",
                    "debug_traceBlockByNumber",
                    json!([
                        if self.rng.chance(Self::VALID_SHARE) {
                            self.chain_block_or_tag()
                        } else {
                            self.block_number_or_tag()
                        },
                        config
                    ]),
                )
            }
            _ => {
                let mut config = self.tracer_config(false, true);
                if config.get("tracer").is_none() {
                    config = json!({ "tracer": "prestateTracer" });
                }
                RpcCase::new("trace", "debug_traceBlockByHash", json!([self.block_hash(), config]))
            }
        }
    }

    fn base(&mut self) -> RpcCase {
        match self.rng.below(4) {
            0 => {
                let block = if self.rng.chance(Self::VALID_SHARE) {
                    self.chain_block_or_tag()
                } else {
                    self.block_number_or_tag()
                };
                RpcCase::new("base", "base_meterBlockByNumber", json!([block]))
            }
            1 => RpcCase::new("base", "base_meterBlockByHash", json!([self.block_hash()])),
            2 => {
                let (hash, label) = self.typed_tx_hash();
                let mut case = RpcCase::new("base", "base_transactionStatus", json!([hash]));
                case.shape = format!("{}:{label}", case.shape);
                case
            }
            _ => {
                let picked: Vec<(Bytes, String)> = (0..self.rng.range(0, 3))
                    .filter_map(|_| self.typed_tx().map(|(t, label)| (t.raw.clone(), label)))
                    .collect();
                let labels: Vec<&str> = picked.iter().map(|(_, l)| l.as_str()).collect();
                let txs: Vec<&Bytes> = picked.iter().map(|(raw, _)| raw).collect();
                let mut case = RpcCase::new("base", "base_meterBundle", json!([{ "txs": txs }]));
                case.shape = format!("{}:[{}]", case.shape, labels.join(","));
                case
            }
        }
    }

    /// A valid case of another family with its envelope or parameters broken.
    fn malformed(&mut self) -> RpcCase {
        let families = ["state", "call", "logs", "blocks", "txs", "fees", "trace", "estimate"];
        let family = *self.rng.pick(&families);
        let base = self.family(family);
        let mut envelope: Value =
            serde_json::from_str(&base.body).expect("generated cases are JSON");
        const KINDS: [&str; 16] = [
            "truncated",
            "empty",
            "params-object",
            "params-missing-last",
            "params-extra",
            "bad-hex",
            "method-case",
            "jsonrpc-version",
            "jsonrpc-missing",
            "id-type",
            "id-missing",
            "batch",
            "batch-empty",
            "params-scalar",
            "method-unknown",
            "params-absent",
        ];
        let kind = self.rng.below(16);
        let body = match kind {
            0 => {
                let cut = self.rng.range(1, base.body.len() as u64 - 1) as usize;
                base.body[..cut].to_owned()
            }
            1 => String::new(),
            2 => {
                envelope["params"] = json!({ "0": envelope["params"][0] });
                envelope.to_string()
            }
            3 => {
                if let Some(params) = envelope["params"].as_array_mut() {
                    params.pop();
                }
                envelope.to_string()
            }
            4 => {
                if let Some(params) = envelope["params"].as_array_mut() {
                    params.push(json!("0x1"));
                    params.push(json!({}));
                }
                envelope.to_string()
            }
            5 => {
                Self::mangle_quantities(self.rng, &mut envelope["params"]);
                envelope.to_string()
            }
            6 => {
                envelope["method"] = json!(base.method.to_uppercase());
                envelope.to_string()
            }
            7 => {
                envelope["jsonrpc"] = json!(*self.rng.pick(&["1.0", "2", ""]));
                envelope.to_string()
            }
            8 => {
                envelope.as_object_mut().expect("object").remove("jsonrpc");
                envelope.to_string()
            }
            9 => {
                envelope["id"] = self
                    .rng
                    .pick(&[
                        json!("abc"),
                        Value::Null,
                        json!({"a":1}),
                        json!(1.5),
                        json!(-1),
                        json!(u64::MAX),
                        json!([1]),
                    ])
                    .clone();
                envelope.to_string()
            }
            10 => {
                envelope.as_object_mut().expect("object").remove("id");
                envelope.to_string()
            }
            11 => {
                let mut items = Vec::new();
                for id in 0..self.rng.range(1, 4) {
                    let mut item =
                        serde_json::from_str::<Value>(&self.family(family).body).expect("JSON");
                    item["id"] = json!(id + 10);
                    items.push(item);
                }
                if self.rng.chance(50) {
                    items.push(json!({ "jsonrpc": "2.0", "id": 99, "method": "eth_nonexistentMethod", "params": [] }));
                }
                if self.rng.chance(30) {
                    items.push(json!(42));
                }
                Value::Array(items).to_string()
            }
            12 => "[]".to_owned(),
            13 => {
                envelope["params"] =
                    self.rng.pick(&[Value::Null, json!("latest"), json!(1), json!(true)]).clone();
                envelope.to_string()
            }
            14 => {
                envelope["method"] = json!(*self.rng.pick(&[
                    "eth_nonexistentMethod",
                    "",
                    "rpc_modules",
                    "eth_",
                    "debug_traceTransactionX"
                ]));
                envelope.to_string()
            }
            _ => {
                envelope.as_object_mut().expect("object").remove("params");
                envelope.to_string()
            }
        };
        let shape = format!("malformed:{}", KINDS[kind as usize]);
        RpcCase { family: "malformed".to_owned(), method: base.method, body, shape }
    }

    /// Breaks the hex encoding of one or more strings in `value`.
    fn mangle_quantities(rng: &mut Rng, value: &mut Value) {
        match value {
            Value::String(text) if text.starts_with("0x") && rng.chance(40) => {
                let digits = text[2..].to_owned();
                *text = match rng.below(9) {
                    0 => format!("0x0{digits}"),
                    1 => digits,
                    2 => format!("0X{}", digits.to_uppercase()),
                    3 => format!("0x{}", "f".repeat(65)),
                    4 => format!("0x{digits}g"),
                    5 => "0x".to_owned(),
                    6 => format!("0x{}", &digits[..digits.len().saturating_sub(1)]),
                    7 => format!(
                        "{}",
                        u64::from_str_radix(&digits[..digits.len().min(15)], 16).unwrap_or(7)
                    ),
                    _ => format!(" {text}"),
                };
            }
            Value::String(text) if rng.chance(10) => {
                *text = "latests".to_owned();
            }
            Value::Bool(b) if rng.chance(50) => *value = json!(b.to_string()),
            Value::Array(items) => {
                items.iter_mut().for_each(|item| Self::mangle_quantities(rng, item))
            }
            Value::Object(object) => {
                object.values_mut().for_each(|item| Self::mangle_quantities(rng, item))
            }
            _ => {}
        }
    }
}
