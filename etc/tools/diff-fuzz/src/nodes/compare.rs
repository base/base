//! Semantic comparison of JSON-RPC responses from several nodes.
//!
//! Responses are compared as JSON values after these normalizations, and nothing else:
//!
//! 1. An error keeps only `code` and `data`; `message` is free text and dropped, and so is a
//!    `data` string that is not `0x`-hex (some servers put parser messages there; revert data,
//!    the `data` the specs define, is hex).
//! 2. A batch response is sorted by `id` (JSON-RPC 2.0 lets servers answer a batch in any order).
//! 3. A reply without a JSON body (connection failure, timeout, HTTP error page) becomes
//!    `{"transport": true}`; its text is not compared. A 2xx reply with an empty body or a bare
//!    `null` body is "no reply" (`{"empty": true}`), which JSON-RPC 2.0 requires for a
//!    notification; servers signal it with 204, an empty 200, or `null`.
//! 4. Results of `eth_newFilter`, `eth_newBlockFilter`, and `eth_newPendingTransactionFilter`
//!    are filter IDs each node picks; a string result becomes `"<filter id>"`.
//! 5. `base_meterBundle` and `base_meterBlockBy*` report wall-clock execution times; every field
//!    whose name ends in `TimeUs` is dropped.
//! 6. A JSON number with a zero fraction in a result becomes an integer (`0.0` is `0`, as in
//!    `eth_feeHistory`'s `gasUsedRatio`); JSON does not distinguish them.
//! 7. Error texts inside a result are free text too: a string `error` field (call tracer
//!    frames, struct logs, `eth_createAccessList`) becomes `"<error>"`, so only its presence is
//!    compared, and an object `error` (an `eth_simulateV1` call) loses its `message`.
//! 8. A struct log's `"refund": 0`, empty `"memory": []` and empty `"returnData": "0x"` are
//!    dropped: geth (and Nethermind) omit these empty fields, reth writes them.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::nodes::Reply;

/// One difference between a node's normalized response and the reference's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcDifference {
    /// JSON path, such as `result.transactions[3].v`.
    pub path: String,
    /// The reference's value (`null` when absent).
    pub expected: Value,
    /// The node's value (`null` when absent).
    pub actual: Value,
}

/// The comparison of every node's response to one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcVerdict {
    /// Every node agrees with the reference.
    pub agree: bool,
    /// Per node that disagrees with the reference: its differences, at most
    /// [`RpcVerdict::MAX_DIFFERENCES`].
    pub differences: Vec<(String, Vec<RpcDifference>)>,
    /// Nodes grouped by identical normalized responses, the reference's group first.
    pub groups: Vec<Vec<String>>,
    /// Per group, the kind of its response: `ok`, `err<code>`, `transport`, or `batch`.
    pub kinds: Vec<String>,
}

impl RpcVerdict {
    /// Most differences kept per node.
    pub const MAX_DIFFERENCES: usize = 16;

    /// Compares normalized responses; `responses[0]` is the reference.
    pub fn compare(responses: &[(String, Value)]) -> Self {
        let mut groups: Vec<(Value, Vec<String>)> = Vec::new();
        for (name, value) in responses {
            match groups.iter_mut().find(|(v, _)| v == value) {
                Some((_, names)) => names.push(name.clone()),
                None => groups.push((value.clone(), vec![name.clone()])),
            }
        }
        let reference = &responses[0].1;
        let mut differences = Vec::new();
        for (name, value) in &responses[1..] {
            if value != reference {
                let mut found = Vec::new();
                RpcCompare::diff("", reference, value, &mut found);
                differences.push((name.clone(), found));
            }
        }
        Self {
            agree: groups.len() == 1,
            differences,
            kinds: groups.iter().map(|(value, _)| Self::kind(value)).collect(),
            groups: groups.into_iter().map(|(_, names)| names).collect(),
        }
    }

    /// The kind of a normalized response.
    pub fn kind(value: &Value) -> String {
        if value.get("error").is_some() {
            format!("err{}", value["error"].get("code").map_or_else(String::new, Value::to_string))
        } else if value.get("result").is_some() {
            "ok".into()
        } else if value.is_array() {
            "batch".into()
        } else if value.get("empty").is_some() {
            "empty".into()
        } else {
            "transport".into()
        }
    }
}

/// Response normalization and structural diffing.
#[derive(Debug)]
pub struct RpcCompare;

impl RpcCompare {
    /// Normalizes one node's reply to a request for `method` (see the module docs).
    pub fn normalize(method: &str, reply: &Reply) -> Value {
        match reply {
            Reply::Transport(text)
                if text.starts_with("HTTP 2") && text.trim_end().ends_with(':') =>
            {
                json!({ "empty": true })
            }
            Reply::Body(Value::Null) => json!({ "empty": true }),
            Reply::Transport(_) => json!({ "transport": true }),
            Reply::Body(Value::Array(items)) => {
                let mut items: Vec<Value> =
                    items.iter().map(|item| Self::envelope(method, item)).collect();
                items.sort_by_key(|item| item.get("id").map(Value::to_string).unwrap_or_default());
                Value::Array(items)
            }
            Reply::Body(body) => Self::envelope(method, body),
        }
    }

    fn envelope(method: &str, body: &Value) -> Value {
        let Value::Object(object) = body else { return body.clone() };
        let mut out = Map::new();
        for (key, value) in object {
            match key.as_str() {
                "error" => {
                    let mut error = Map::new();
                    if let Some(code) = value.get("code") {
                        error.insert("code".into(), code.clone());
                    }
                    if let Some(data) = value.get("data")
                        && data.as_str().is_none_or(Self::is_hex)
                    {
                        error.insert("data".into(), data.clone());
                    }
                    if !value.is_object() {
                        error.insert("value".into(), value.clone());
                    }
                    out.insert(key.clone(), Value::Object(error));
                }
                "result" => {
                    let mut result = value.clone();
                    if method.starts_with("eth_new")
                        && method.ends_with("Filter")
                        && result.is_string()
                    {
                        result = json!("<filter id>");
                    }
                    if method.starts_with("base_meter") {
                        Self::drop_timings(&mut result);
                    }
                    Self::integral_floats(&mut result);
                    Self::error_texts(&mut result);
                    out.insert(key.clone(), result);
                }
                _ => {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        Value::Object(out)
    }

    /// Whether `text` is `0x` followed by hex digits.
    pub fn is_hex(text: &str) -> bool {
        text.strip_prefix("0x").is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    fn drop_timings(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.retain(|key, _| !key.ends_with("TimeUs"));
                object.values_mut().for_each(Self::drop_timings);
            }
            Value::Array(items) => items.iter_mut().for_each(Self::drop_timings),
            _ => {}
        }
    }

    fn error_texts(value: &mut Value) {
        match value {
            Value::Object(object) => {
                match object.get_mut("error") {
                    Some(text @ Value::String(_)) => *text = json!("<error>"),
                    Some(Value::Object(error)) => {
                        error.remove("message");
                    }
                    _ => {}
                }
                if object.contains_key("op") {
                    for (key, empty) in
                        [("refund", json!(0)), ("memory", json!([])), ("returnData", json!("0x"))]
                    {
                        if object.get(key) == Some(&empty) {
                            object.remove(key);
                        }
                    }
                }
                object.values_mut().for_each(Self::error_texts);
            }
            Value::Array(items) => items.iter_mut().for_each(Self::error_texts),
            _ => {}
        }
    }

    fn integral_floats(value: &mut Value) {
        match value {
            Value::Number(number) if number.is_f64() => {
                if let Some(f) = number.as_f64()
                    && f.fract() == 0.0
                    && f.abs() < 9e15
                {
                    *value = json!(f as i64);
                }
            }
            Value::Object(object) => object.values_mut().for_each(Self::integral_floats),
            Value::Array(items) => items.iter_mut().for_each(Self::integral_floats),
            _ => {}
        }
    }

    /// Appends the differences between `a` and `b` under `path`.
    pub fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<RpcDifference>) {
        if out.len() >= RpcVerdict::MAX_DIFFERENCES || a == b {
            return;
        }
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
                keys.sort();
                keys.dedup();
                for key in keys {
                    let sub = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                    match (x.get(key), y.get(key)) {
                        (Some(p), Some(q)) => Self::diff(&sub, p, q, out),
                        (p, q) => out.push(RpcDifference {
                            path: sub,
                            expected: p.map_or_else(|| json!("<absent>"), Self::clip),
                            actual: q.map_or_else(|| json!("<absent>"), Self::clip),
                        }),
                    }
                }
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    Self::diff(&format!("{path}[{i}]"), p, q, out);
                }
            }
            _ => out.push(RpcDifference {
                path: path.to_owned(),
                expected: Self::clip(a),
                actual: Self::clip(b),
            }),
        }
    }

    /// Long values are cut to keep findings readable.
    fn clip(value: &Value) -> Value {
        let text = value.to_string();
        if text.len() <= 600 {
            value.clone()
        } else {
            json!(format!("{}… ({} bytes)", &text[..text.floor_char_boundary(600)], text.len()))
        }
    }

    /// A path with array indices replaced by `[]`, for grouping differences.
    pub fn shape(path: &str) -> String {
        let mut out = String::with_capacity(path.len());
        let mut in_index = false;
        for c in path.chars() {
            match c {
                '[' => {
                    in_index = true;
                    out.push_str("[]");
                }
                ']' => in_index = false,
                _ if in_index => {}
                _ => out.push(c),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RpcCompare, RpcVerdict};
    use crate::nodes::Reply;

    #[test]
    fn error_messages_are_ignored_but_codes_and_data_are_compared() {
        let a = Reply::Body(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted","data":"0x01"}}),
        );
        let b = Reply::Body(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"reverted: 0x01","data":"0x01"}}),
        );
        let c = Reply::Body(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted","data":"0x02"}}),
        );
        let n = |r| RpcCompare::normalize("eth_call", r);
        let verdict =
            RpcVerdict::compare(&[("a".into(), n(&a)), ("b".into(), n(&b)), ("c".into(), n(&c))]);
        assert_eq!(
            verdict.groups,
            vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]]
        );
        assert_eq!(verdict.differences.len(), 1);
        assert_eq!(verdict.differences[0].1[0].path, "error.data");

        // In a result, an error text is compared only by its presence.
        let n = |r| RpcCompare::normalize("debug_traceTransaction", &Reply::Body(r));
        let oog = n(json!({"id":1,"result":{"calls":[{"error":"out of gas"}]}}));
        let other = n(json!({"id":1,"result":{"calls":[{"error":"max initcode size exceeded"}]}}));
        let none = n(json!({"id":1,"result":{"calls":[{}]}}));
        assert!(RpcVerdict::compare(&[("a".into(), oog.clone()), ("b".into(), other)]).agree);
        assert!(!RpcVerdict::compare(&[("a".into(), oog), ("b".into(), none)]).agree);

        // A zero struct-log refund equals an absent one; a nonzero refund does not.
        let log = |refund: Option<u64>| {
            let mut step = json!({"op":"SSTORE","gas":10});
            if let Some(refund) = refund {
                step["refund"] = json!(refund);
            }
            n(json!({"id":1,"result":{"structLogs":[step]}}))
        };
        assert!(RpcVerdict::compare(&[("a".into(), log(Some(0))), ("b".into(), log(None))]).agree);
        assert!(
            !RpcVerdict::compare(&[("a".into(), log(Some(4800))), ("b".into(), log(None))]).agree
        );
        // Empty memory and return data equal absent ones; nonempty ones do not.
        let step = |extra: serde_json::Value| {
            let mut step = json!({"op":"PUSH0","gas":10});
            step.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            n(json!({"id":1,"result":{"structLogs":[step]}}))
        };
        let empty = step(json!({"memory": [], "returnData": "0x"}));
        assert!(RpcVerdict::compare(&[("a".into(), empty), ("b".into(), step(json!({})))]).agree);
        let full = step(json!({"returnData": "0x01"}));
        assert!(!RpcVerdict::compare(&[("a".into(), full), ("b".into(), step(json!({})))]).agree);
    }

    #[test]
    fn batches_compare_in_id_order() {
        let a = Reply::Body(json!([{"id":1,"result":"0x1"},{"id":2,"result":"0x2"}]));
        let b = Reply::Body(json!([{"id":2,"result":"0x2"},{"id":1,"result":"0x1"}]));
        let n = |r| RpcCompare::normalize("eth_chainId", r);
        assert!(RpcVerdict::compare(&[("a".into(), n(&a)), ("b".into(), n(&b))]).agree);
    }

    #[test]
    fn integral_floats_equal_integers_but_fractions_differ() {
        let n = |r| RpcCompare::normalize("eth_feeHistory", &Reply::Body(r));
        let zero = n(json!({"id":1,"result":{"blobGasUsedRatio":[0.0, 0.5]}}));
        let int = n(json!({"id":1,"result":{"blobGasUsedRatio":[0, 0.5]}}));
        let other = n(json!({"id":1,"result":{"blobGasUsedRatio":[0, 0.25]}}));
        assert!(RpcVerdict::compare(&[("a".into(), zero.clone()), ("b".into(), int)]).agree);
        assert!(!RpcVerdict::compare(&[("a".into(), zero), ("b".into(), other)]).agree);
    }

    #[test]
    fn shapes_drop_indices() {
        assert_eq!(RpcCompare::shape("result.logs[12].topics[0]"), "result.logs[].topics[]");
    }
}
