//! The JSON-RPC envelope as base-reth's server (jsonrpsee 0.26) reads it: which bodies are
//! requests, notifications, invalid requests or parse errors, how batches are answered, and how
//! positional parameters are checked before a method runs.

use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

use crate::{Node, RpcError};

/// The raw text of a request's `params`, which jsonrpsee decodes parameters from.
#[derive(Deserialize)]
struct RawParams<'a> {
    #[serde(borrow, default)]
    params: Option<&'a RawValue>,
}

/// How one object of a body parses.
enum Envelope<'a> {
    /// A request to answer.
    Request { id: Value, method: &'a str, params: Value, raw: Option<&'a str> },
    /// A notification: no reply, and jsonrpsee does not run the method.
    Notification,
    /// An object with a valid `id` that is not a request.
    Invalid(Value),
    /// Nothing usable.
    Unparsable,
}

impl Node {
    /// Answers one HTTP body: the response, or `None` when there is nothing to answer (a
    /// notification, or a batch of only notifications), which base-reth sends as `null`.
    pub fn handle_body(&self, body: &[u8], authenticated: bool) -> Option<Value> {
        let first = body.iter().find(|byte| !byte.is_ascii_whitespace());
        let parsed = match first {
            Some(b'{' | b'[') => serde_json::from_slice::<Value>(body).ok(),
            _ => None,
        };
        let Some(request) = parsed else {
            return Some(Self::response(Value::Null, Err(RpcError::new(-32700, "Parse error"))));
        };
        // The body parsed as JSON, so it is UTF-8.
        let text = std::str::from_utf8(body).expect("JSON is UTF-8");
        let raw_objects: Vec<&RawValue> = match &request {
            Value::Array(_) => serde_json::from_str(text).unwrap_or_default(),
            _ => serde_json::from_str(text).map(|one| vec![one]).unwrap_or_default(),
        };
        let raw_params = |index: usize| {
            let object = raw_objects.get(index)?;
            serde_json::from_str::<RawParams<'_>>(object.get()).ok()?.params.map(RawValue::get)
        };
        match request {
            Value::Array(batch) if batch.is_empty() => {
                Some(Self::response(Value::Null, Err(RpcError::new(-32600, "Invalid request"))))
            }
            Value::Array(batch) => {
                let responses: Vec<Value> = batch
                    .iter()
                    .enumerate()
                    .filter_map(|(index, one)| match Self::envelope(one, raw_params(index)) {
                        Envelope::Request { id, method, params, raw } => {
                            Some(Self::response(id, self.dispatch(method, &params, raw, authenticated)))
                        }
                        Envelope::Notification => None,
                        Envelope::Invalid(id) => Some(Self::invalid_request(id)),
                        Envelope::Unparsable => Some(Self::invalid_request(Value::Null)),
                    })
                    .collect();
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            one => match Self::envelope(&one, raw_params(0)) {
                Envelope::Request { id, method, params, raw } => {
                    Some(Self::response(id, self.dispatch(method, &params, raw, authenticated)))
                }
                Envelope::Notification => None,
                Envelope::Invalid(id) => Some(Self::invalid_request(id)),
                Envelope::Unparsable => {
                    Some(Self::response(Value::Null, Err(RpcError::new(-32700, "Parse error"))))
                }
            },
        }
    }

    /// jsonrpsee's three parses of an object: request, notification, then `{id}`.
    fn envelope<'a>(value: &'a Value, raw: Option<&'a str>) -> Envelope<'a> {
        let Value::Object(object) = value else { return Envelope::Unparsable };
        let version_ok = object.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").filter(|id| Self::valid_id(id));
        match (version_ok, method, id) {
            (true, Some(method), Some(id)) => Envelope::Request {
                id: id.clone(),
                method,
                params: object.get("params").cloned().unwrap_or(Value::Null),
                raw,
            },
            (true, Some(_), None) => Envelope::Notification,
            (_, _, Some(id)) => Envelope::Invalid(id.clone()),
            _ => Envelope::Unparsable,
        }
    }

    /// jsonrpsee ids: `null`, an unsigned 64-bit integer, or a string.
    fn valid_id(id: &Value) -> bool {
        match id {
            Value::Null | Value::String(_) => true,
            Value::Number(number) => number.is_u64(),
            _ => false,
        }
    }

    fn invalid_request(id: Value) -> Value {
        Self::response(id, Err(RpcError::new(-32600, "Invalid request")))
    }

    fn response(id: Value, result: Result<Value, RpcError>) -> Value {
        match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
        }
    }

    /// Decodes the parameters as base-reth's signature for the method does, then runs the
    /// method (catching a panic as an internal error).
    fn dispatch(&self, method: &str, params: &Value, raw: Option<&str>, authenticated: bool) -> Result<Value, RpcError> {
        if !authenticated && !self.rpc_config.serves(method) {
            return Err(RpcError::method_not_found(method));
        }
        let params = match Self::reth_params(method, raw) {
            Some(decoded) => Value::Array(decoded?),
            None => params.clone(),
        };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.call(method, &params, authenticated)))
            .unwrap_or_else(|panic| {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                eprintln!("tiger: panic in {method}: {message}");
                Err(RpcError::internal(format!("internal error: {message}")))
            })
    }

}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::test_utils::{node, reply};

    #[test]
    fn envelope_answers_as_jsonrpsee() {
        let node = node();
        let chain_id = |id: &str| format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"eth_chainId","params":[]}}"#);
        // Ids jsonrpsee cannot read make a notification: nothing is answered.
        for id in ["1.5", "-1", "{}", "true"] {
            assert_eq!(reply(&node, &chain_id(id)), None, "id {id}");
        }
        assert_eq!(reply(&node, r#"{"jsonrpc":"2.0","method":"eth_chainId"}"#), None);
        assert_eq!(reply(&node, &chain_id("7")).unwrap()["result"], json!("0x2105"));
        assert_eq!(reply(&node, "[]").unwrap()["error"]["code"], json!(-32600));
        assert_eq!(reply(&node, "nonsense").unwrap()["error"]["code"], json!(-32700));
        let wrong_version = reply(&node, r#"{"jsonrpc":"1.0","id":3,"method":"eth_chainId"}"#).unwrap();
        assert_eq!((wrong_version["id"].clone(), wrong_version["error"]["code"].clone()), (json!(3), json!(-32600)));
        // A batch answers each request, skips notifications, and rejects non-objects.
        let batch = reply(&node, &format!("[{}, {}, 5]", chain_id("1"), chain_id("2.5"))).unwrap();
        assert_eq!(batch[0]["result"], json!("0x2105"));
        assert_eq!((batch[1]["id"].clone(), batch[1]["error"]["code"].clone()), (json!(null), json!(-32600)));
        assert_eq!(batch.as_array().unwrap().len(), 2);
        // A required parameter that is missing, or parameters that are not a list, are -32602.
        let missing = reply(&node, r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBlockReceipts","params":[]}"#).unwrap();
        assert_eq!(missing["error"]["code"], json!(-32602));
        let scalar = reply(&node, r#"{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":"latest"}"#).unwrap();
        assert_eq!(scalar["result"], json!("0x0"), "a method without parameters ignores them");
        let scalar = reply(&node, r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":"latest"}"#).unwrap();
        assert_eq!(scalar["error"]["code"], json!(-32602));
        // Decode errors are jsonrpsee's: `Invalid params` with the decoder's text as `data`,
        // serde positions included, and objects matched by reth's argument names.
        let data = |body: &str| reply(&node, body).unwrap()["error"]["data"].clone();
        assert_eq!(data(r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBlockReceipts","params":[]}"#), json!("No more params"));
        assert_eq!(
            data(r#"{"jsonrpc":"2.0","id":1,"method":"eth_getBalance","params":"latest"}"#),
            json!("Invalid params. Expected one of '[', ']' or ',' but found \"\\\"latest\\\"\"")
        );
        assert_eq!(
            data(r#"{"jsonrpc":"2.0","id":1,"method":"eth_getTransactionByHash","params":{"0":"0x00"}}"#),
            json!("missing field `hash` at line 1 column 12")
        );
        let unknown = reply(&node, r#"{"jsonrpc":"2.0","id":1,"method":"ETH_CHAINID","params":[]}"#).unwrap();
        assert_eq!(unknown["error"], json!({ "code": -32601, "message": "Method not found" }));
    }
}
