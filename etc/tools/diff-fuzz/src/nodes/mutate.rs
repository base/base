//! Mutations of JSON-RPC requests kept in the coverage corpus.

use serde_json::{Value, json};

use crate::{Rng, nodes::RpcCase};

/// Mutates corpus requests. Values come from three places: the parent itself, a donor kept in
/// the corpus, and a fresh request of the parent's family, which carries valid block numbers,
/// hashes, addresses, and call objects for the current chain.
#[derive(Debug)]
pub struct RpcMutator {
    /// Random stream.
    pub rng: Rng,
}

impl RpcMutator {
    /// Operator names, for statistics.
    pub const OPERATORS: [&'static str; 12] = [
        "fresh-param",
        "fresh-subtree",
        "fresh-key",
        "splice",
        "drop",
        "duplicate",
        "number",
        "hex",
        "tag",
        "retype",
        "method",
        "bytes",
    ];

    /// Methods that take the same parameters, so a request can switch between them.
    pub const SIBLINGS: [&'static [&'static str]; 6] = [
        &["eth_call", "eth_estimateGas", "eth_createAccessList", "debug_traceCall"],
        &["eth_getBalance", "eth_getTransactionCount", "eth_getCode"],
        &[
            "eth_getBlockTransactionCountByNumber",
            "eth_getUncleCountByBlockNumber",
            "eth_getBlockReceipts",
            "debug_getRawHeader",
            "debug_getRawBlock",
            "debug_getRawReceipts",
        ],
        &[
            "eth_getBlockTransactionCountByHash",
            "eth_getUncleCountByBlockHash",
            "eth_getBlockReceipts",
        ],
        &[
            "eth_getTransactionByHash",
            "eth_getTransactionReceipt",
            "debug_getRawTransaction",
            "debug_traceTransaction",
            "trace_transaction",
        ],
        &["debug_traceBlockByNumber", "trace_block", "eth_getBlockByNumber"],
    ];

    /// One to three stacked mutations of `parent`, and the operators applied.
    pub fn mutate(
        &mut self,
        parent: &RpcCase,
        donor: Option<&RpcCase>,
        fresh: &RpcCase,
    ) -> (RpcCase, Vec<&'static str>) {
        let mut case = parent.clone();
        let mut applied = Vec::new();
        for _ in 0..1 + self.rng.below(3) {
            let operator = *self.rng.pick(&Self::OPERATORS);
            if let Some(next) = self.apply(operator, &case, donor, fresh) {
                case = next;
                applied.push(operator);
            }
        }
        if applied.is_empty() {
            let next = self.apply("bytes", &case, donor, fresh).expect("byte edits always apply");
            case = next;
            applied.push("bytes");
        }
        Self::bound_struct_logs(&mut case);
        (case, applied)
    }

    /// Applies one operator, or returns `None` when it does not fit the request.
    fn apply(
        &mut self,
        operator: &str,
        case: &RpcCase,
        donor: Option<&RpcCase>,
        fresh: &RpcCase,
    ) -> Option<RpcCase> {
        if operator == "bytes" {
            return Some(self.edit_bytes(case));
        }
        let mut envelope: Value = serde_json::from_str(&case.body).ok()?;
        // A batch mutates one of its requests.
        let target = match &mut envelope {
            Value::Array(items) if !items.is_empty() => {
                let i = self.rng.index(items.len());
                &mut items[i]
            }
            other => other,
        };
        let target = target.as_object_mut()?;
        let mut method = case.method.clone();
        let params = target.entry("params").or_insert_with(|| json!([]));
        let fresh_params = fresh.params().unwrap_or(Value::Null);
        let changed = match operator {
            "fresh-param" => {
                let (Value::Array(items), Value::Array(new)) = (&mut *params, &fresh_params) else {
                    return None;
                };
                if new.is_empty() {
                    return None;
                }
                let j = self.rng.index(new.len());
                if j < items.len() && self.rng.chance(80) {
                    items[j] = new[j].clone();
                } else {
                    items.push(new[j].clone());
                }
                true
            }
            "fresh-subtree" => {
                let path = self.random_path(params);
                Self::get(&fresh_params, &path)
                    .or_else(|| self.random_subtree(&fresh_params))
                    .is_some_and(|value| Self::set(params, &path, value))
            }
            "fresh-key" => {
                let source = self.random_subtree(&fresh_params)?;
                let from = source.as_object().filter(|o| !o.is_empty())?;
                let (key, value) = from.iter().nth(self.rng.index(from.len()))?;
                let path = self.random_path(params);
                match Self::get_mut(params, &path) {
                    Some(Value::Object(object)) => {
                        object.insert(key.clone(), value.clone());
                        true
                    }
                    _ => false,
                }
            }
            "splice" => {
                let donor_params = donor?.params()?;
                let path = self.random_path(params);
                Self::get(&donor_params, &path)
                    .or_else(|| self.random_subtree(&donor_params))
                    .is_some_and(|value| Self::set(params, &path, value))
            }
            "drop" | "duplicate" => {
                let path = self.random_path(params);
                match Self::get_mut(params, &path) {
                    Some(Value::Array(items)) if !items.is_empty() => {
                        let i = self.rng.index(items.len());
                        if operator == "drop" {
                            items.remove(i);
                        } else {
                            items.insert(i, items[i].clone());
                        }
                        true
                    }
                    Some(Value::Object(object)) if operator == "drop" && !object.is_empty() => {
                        let key = object.keys().nth(self.rng.index(object.len()))?.clone();
                        object.remove(&key);
                        true
                    }
                    _ => false,
                }
            }
            "number" | "hex" | "tag" | "retype" => {
                let leaves = Self::leaves(params);
                let candidates: Vec<&Vec<PathStep>> = leaves
                    .iter()
                    .filter(|(_, v)| match operator {
                        "number" => Self::quantity(v).is_some(),
                        "hex" => v.as_str().is_some_and(|s| s.starts_with("0x") && s.len() > 18),
                        _ => true,
                    })
                    .map(|(p, _)| p)
                    .collect();
                if candidates.is_empty() {
                    return None;
                }
                let path = candidates[self.rng.index(candidates.len())].clone();
                let old = Self::get(params, &path)?;
                let new = match operator {
                    "number" => self.tweak_quantity(Self::quantity(&old)?),
                    "hex" => self.tweak_hex(old.as_str()?),
                    "tag" => self.tag(),
                    _ => self.retype(),
                };
                Self::set(params, &path, new)
            }
            "method" => {
                let group = Self::SIBLINGS.iter().find(|g| g.contains(&method.as_str()))?;
                let next = *self.rng.pick(group);
                if next == method {
                    return None;
                }
                method = next.to_owned();
                target.insert("method".into(), json!(next));
                true
            }
            _ => false,
        };
        if !changed {
            return None;
        }
        let shape = target.get("params").map(RpcCase::shape_of).unwrap_or_default();
        Some(RpcCase { family: case.family.clone(), method, body: envelope.to_string(), shape })
    }

    /// Most struct-log steps a mutant may ask for.
    pub const MAX_STRUCT_LOGS: u64 = 2000;

    /// Gives a struct-log trace of a mutant a step limit. Without one, a trace of a whole block or
    /// a long transaction (such as `eth_getBlockByNumber` switched to
    /// `debug_traceBlockByNumber`) is tens of megabytes per node, which the harness parses for
    /// every node and again while minimizing. A tracer config that is missing, not an object,
    /// or names no tracer (no `tracer`, or an empty or non-string one) gets `limit` set to at most
    /// [`Self::MAX_STRUCT_LOGS`].
    fn bound_struct_logs(case: &mut RpcCase) {
        let Ok(mut envelope) = serde_json::from_str::<Value>(&case.body) else {
            return;
        };
        let requests = match &mut envelope {
            Value::Array(items) => items.iter_mut().collect(),
            single => vec![single],
        };
        let mut changed = false;
        for request in requests {
            let index = match request.get("method").and_then(Value::as_str) {
                Some(
                    "debug_traceTransaction"
                    | "debug_traceBlockByNumber"
                    | "debug_traceBlockByHash",
                ) => 1,
                Some("debug_traceCall") => 2,
                _ => continue,
            };
            let Some(Value::Array(params)) = request.get_mut("params") else {
                continue;
            };
            if params.len() <= index {
                params.resize(index + 1, Value::Null);
            }
            if !params[index].is_object() {
                params[index] = json!({});
            }
            let config = params[index].as_object_mut().expect("an object");
            // An empty or non-string tracer still runs the struct logger on some clients.
            if config.get("tracer").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
                continue;
            }
            let limit = config.get("limit").and_then(Value::as_u64);
            if !limit.is_some_and(|l| (1..=Self::MAX_STRUCT_LOGS).contains(&l)) {
                config.insert("limit".into(), json!(512));
                changed = true;
            }
        }
        if changed {
            if let Some(params) = envelope.get("params") {
                case.shape = RpcCase::shape_of(params);
            }
            case.body = envelope.to_string();
        }
    }

    /// Inserts, deletes, or replaces one character of the body: requests that are not valid
    /// JSON reach the parser's error paths.
    fn edit_bytes(&mut self, case: &RpcCase) -> RpcCase {
        let mut body: Vec<char> = case.body.chars().collect();
        let at = self.rng.index(body.len() + 1);
        let chars = ['"', '{', '}', '[', ']', ',', ':', '0', 'x', '-', ' ', '\\', 'e', 'n'];
        match self.rng.below(3) {
            0 => body.insert(at, *self.rng.pick(&chars)),
            1 if at < body.len() => {
                body.remove(at);
            }
            _ if at < body.len() => body[at] = *self.rng.pick(&chars),
            _ => body.push(*self.rng.pick(&chars)),
        }
        RpcCase {
            family: case.family.clone(),
            method: case.method.clone(),
            body: body.into_iter().collect(),
            shape: "bytes".into(),
        }
    }

    /// A hex quantity's value, when it fits in 128 bits.
    fn quantity(value: &Value) -> Option<u128> {
        let text = value.as_str()?.strip_prefix("0x")?;
        (!text.is_empty() && text.len() <= 32).then(|| u128::from_str_radix(text, 16).ok())?
    }

    fn tweak_quantity(&mut self, value: u128) -> Value {
        let new = match self.rng.below(8) {
            0 => value.wrapping_add(1),
            1 => value.wrapping_sub(1),
            2 => value.wrapping_add(1 + u128::from(self.rng.below(64))),
            3 => value.wrapping_sub(1 + u128::from(self.rng.below(64))),
            4 => value / 2,
            5 => value.saturating_mul(2),
            6 => 0,
            _ => {
                let edges = [u128::from(u64::MAX), u128::from(u32::MAX), 1 << 63, u128::MAX];
                *self.rng.pick(&edges)
            }
        };
        match self.rng.below(10) {
            // Leading zeros are invalid quantities.
            0 => json!(format!("0x0{new:x}")),
            // Decimal numbers instead of hex strings.
            1 => json!(new as u64),
            _ => json!(format!("{new:#x}")),
        }
    }

    fn tweak_hex(&mut self, text: &str) -> Value {
        let digits = &text[2..];
        let new = match self.rng.below(6) {
            // Flip one nibble.
            0 if !digits.is_empty() => {
                let mut chars: Vec<char> = digits.chars().collect();
                let i = self.rng.index(chars.len());
                chars[i] = *self.rng.pick(&['0', '1', '7', '8', 'f', 'F']);
                chars.into_iter().collect()
            }
            // Drop bytes from the end, or a single nibble.
            1 => {
                let keep = self.rng.index(digits.len() + 1);
                digits[..keep].to_owned()
            }
            2 => {
                format!("{digits}{}", alloy_primitives::hex::encode(self.rng.bytes_between(1, 33)))
            }
            3 => String::new(),
            4 => format!("{digits}{digits}"),
            _ => digits.get(1..).unwrap_or_default().to_owned(),
        };
        json!(format!("0x{new}"))
    }

    fn tag(&mut self) -> Value {
        json!(*self.rng.pick(&["latest", "earliest", "pending", "safe", "finalized", "0x0"]))
    }

    fn retype(&mut self) -> Value {
        match self.rng.below(8) {
            0 => Value::Null,
            1 => json!(true),
            2 => json!(0),
            3 => json!(""),
            4 => json!([]),
            5 => json!({}),
            6 => json!("0x"),
            _ => json!(-1),
        }
    }

    /// A random path into `value`, to an inner node or a leaf (possibly the root).
    fn random_path(&mut self, value: &Value) -> Vec<PathStep> {
        let mut path = Vec::new();
        let mut current = value;
        loop {
            let next = match current {
                Value::Array(items) if !items.is_empty() && self.rng.chance(70) => {
                    let i = self.rng.index(items.len());
                    path.push(PathStep::Index(i));
                    &items[i]
                }
                Value::Object(object) if !object.is_empty() && self.rng.chance(70) => {
                    let (key, child) = object.iter().nth(self.rng.index(object.len())).unwrap();
                    path.push(PathStep::Key(key.clone()));
                    child
                }
                _ => return path,
            };
            current = next;
        }
    }

    fn random_subtree(&mut self, value: &Value) -> Option<Value> {
        let path = self.random_path(value);
        Self::get(value, &path).filter(|v| !v.is_null())
    }

    /// Every leaf (scalar) under `value` with its path.
    fn leaves(value: &Value) -> Vec<(Vec<PathStep>, Value)> {
        let mut out = Vec::new();
        let mut stack = vec![(Vec::new(), value)];
        while let Some((path, value)) = stack.pop() {
            match value {
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        let mut p = path.clone();
                        p.push(PathStep::Index(i));
                        stack.push((p, item));
                    }
                }
                Value::Object(object) => {
                    for (key, item) in object {
                        let mut p = path.clone();
                        p.push(PathStep::Key(key.clone()));
                        stack.push((p, item));
                    }
                }
                leaf => out.push((path, leaf.clone())),
            }
        }
        out
    }

    fn get(value: &Value, path: &[PathStep]) -> Option<Value> {
        let mut current = value;
        for step in path {
            current = match (step, current) {
                (PathStep::Index(i), Value::Array(items)) => items.get(*i)?,
                (PathStep::Key(k), Value::Object(object)) => object.get(k)?,
                _ => return None,
            };
        }
        Some(current.clone())
    }

    fn get_mut<'v>(value: &'v mut Value, path: &[PathStep]) -> Option<&'v mut Value> {
        let mut current = value;
        for step in path {
            current = match (step, current) {
                (PathStep::Index(i), Value::Array(items)) => items.get_mut(*i)?,
                (PathStep::Key(k), Value::Object(object)) => object.get_mut(k)?,
                _ => return None,
            };
        }
        Some(current)
    }

    /// Replaces the value at `path`; `false` when the path does not exist or nothing changed.
    fn set(value: &mut Value, path: &[PathStep], new: Value) -> bool {
        match Self::get_mut(value, path) {
            Some(slot) if *slot != new => {
                *slot = new;
                true
            }
            _ => false,
        }
    }
}

/// One step of a path into a JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep {
    /// An array index.
    Index(usize),
    /// An object key.
    Key(String),
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::RpcMutator;
    use crate::{Rng, nodes::RpcCase};

    #[test]
    fn mutants_keep_the_envelope_and_track_method_and_shape() {
        let parent = RpcCase::new(
            "call",
            "eth_call",
            json!([{ "to": "0x00000000000000000000000000000000000000aa", "data": "0x12345678" }, "latest"]),
        );
        let fresh = RpcCase::new("call", "eth_call", json!([{ "gas": "0x5208" }, "0x10"]));
        let mut mutator = RpcMutator { rng: Rng::new(&[7]) };
        let mut changed = 0;
        for _ in 0..500 {
            let (child, operators) = mutator.mutate(&parent, Some(&fresh), &fresh);
            assert!(!operators.is_empty());
            if child.body != parent.body {
                changed += 1;
            }
            if operators.contains(&"bytes") {
                continue;
            }
            let envelope: Value = serde_json::from_str(&child.body).expect("still JSON");
            assert_eq!(envelope["method"], json!(child.method));
            assert_eq!(envelope["id"], json!(1));
            assert_eq!(child.shape, RpcCase::shape_of(&envelope["params"]));
        }
        assert!(changed > 450, "only {changed} of 500 mutants differ from the parent");
    }

    #[test]
    fn struct_log_traces_of_mutants_stay_limited() {
        let block = RpcCase::new("blocks", "eth_getBlockByNumber", json!(["0x10", true]));
        let tx = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!([format!("0x{}", "ab".repeat(32)), { "enableMemory": true, "limit": 64 }]),
        );
        let mut mutator = RpcMutator { rng: Rng::new(&[11]) };
        let mut traces = 0;
        for _ in 0..2000 {
            for parent in [&block, &tx] {
                let (child, _) = mutator.mutate(parent, Some(&tx), &block);
                let Ok(envelope) = serde_json::from_str::<Value>(&child.body) else { continue };
                let index = match envelope["method"].as_str() {
                    Some("debug_traceBlockByNumber" | "debug_traceTransaction") => 1,
                    _ => continue,
                };
                // Without a parameter list (a renamed `params` key) the node traces nothing.
                if !envelope["params"].is_array() {
                    continue;
                }
                traces += 1;
                let config = &envelope["params"][index];
                let limit = config["limit"].as_u64().unwrap_or(0);
                let named = config["tracer"].as_str().is_some_and(|t| !t.is_empty());
                assert!(
                    named || (1..=2000).contains(&limit),
                    "unbounded struct-log trace: {}",
                    child.body
                );
            }
        }
        assert!(traces > 500, "only {traces} trace mutants");
    }
}
