//! The JSON-RPC envelope as base-reth's server (jsonrpsee 0.26) reads it: which bodies are
//! requests, notifications, invalid requests or parse errors, how batches are answered, and how
//! positional parameters are checked before a method runs.

use serde_json::{Value, json};

use crate::{Node, RpcError};

/// How one object of a body parses.
enum Envelope<'a> {
    /// A request to answer.
    Request { id: Value, method: &'a str, params: Value },
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
        match request {
            Value::Array(batch) if batch.is_empty() => {
                Some(Self::response(Value::Null, Err(RpcError::new(-32600, "Invalid request"))))
            }
            Value::Array(batch) => {
                let responses: Vec<Value> = batch
                    .iter()
                    .filter_map(|one| match Self::envelope(one) {
                        Envelope::Request { id, method, params } => {
                            Some(Self::response(id, self.dispatch(method, &params, authenticated)))
                        }
                        Envelope::Notification => None,
                        Envelope::Invalid(id) => Some(Self::invalid_request(id)),
                        Envelope::Unparsable => Some(Self::invalid_request(Value::Null)),
                    })
                    .collect();
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            one => match Self::envelope(&one) {
                Envelope::Request { id, method, params } => {
                    Some(Self::response(id, self.dispatch(method, &params, authenticated)))
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
    fn envelope(value: &Value) -> Envelope<'_> {
        let Value::Object(object) = value else { return Envelope::Unparsable };
        let version_ok = object.get("jsonrpc").and_then(Value::as_str) == Some("2.0");
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").filter(|id| Self::valid_id(id));
        match (version_ok, method, id) {
            (true, Some(method), Some(id)) => Envelope::Request {
                id: id.clone(),
                method,
                params: object.get("params").cloned().unwrap_or(Value::Null),
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

    /// Checks the parameter list's shape as jsonrpsee's generated code does, then runs the
    /// method (catching a panic as an internal error).
    fn dispatch(&self, method: &str, params: &Value, authenticated: bool) -> Result<Value, RpcError> {
        let params = match (Self::arity(method), params) {
            // A method without parameters never looks at them.
            (Some((0, _)), _) => Value::Array(Vec::new()),
            (_, Value::Null) => Value::Array(Vec::new()),
            (_, Value::Array(_)) => params.clone(),
            // An object is matched by argument name; with every argument optional, unknown names
            // leave them all unset.
            (Some((_, 0)), Value::Object(_)) => Value::Array(Vec::new()),
            (Some(_), _) => return Err(RpcError::invalid_params("Invalid params".into())),
            (None, _) => params.clone(),
        };
        // Required arguments are not `Option`s: a missing or `null` one does not deserialize.
        if let (Some((_, required)), Value::Array(items)) = (Self::arity(method), &params)
            && (items.len() < required || items[..required].iter().any(Value::is_null))
        {
            return Err(RpcError::invalid_params("Invalid params".into()));
        }
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

    /// `(parameters, required parameters)` of each method base-reth serves, from its RPC trait
    /// signatures; `None` for methods tiger does not know.
    fn arity(method: &str) -> Option<(usize, usize)> {
        Some(match method {
            "eth_chainId" | "eth_blockNumber" | "net_version" | "eth_syncing" | "web3_clientVersion"
            | "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_blobBaseFee" | "eth_newBlockFilter"
            | "rpc_modules" | "eth_config" => (0, 0),
            "eth_getBalance" => (2, 1),
            "eth_getCode" => (2, 1),
            "eth_getTransactionCount" => (3, 1),
            "eth_getStorageAt" | "eth_getProof" => (3, 2),
            "eth_getBlockByNumber" | "eth_getBlockByHash" => (2, 2),
            "eth_getUncleByBlockNumberAndIndex"
            | "eth_getUncleByBlockHashAndIndex"
            | "eth_getTransactionByBlockHashAndIndex"
            | "eth_getTransactionByBlockNumberAndIndex"
            | "eth_getRawTransactionByBlockHashAndIndex"
            | "eth_getRawTransactionByBlockNumberAndIndex"
            | "eth_getTransactionBySenderAndNonce" => (2, 2),
            "eth_getTransactionByHash"
            | "eth_getTransactionReceipt"
            | "eth_getBlockReceipts"
            | "eth_getBlockTransactionCountByNumber"
            | "eth_getBlockTransactionCountByHash"
            | "eth_getUncleCountByBlockNumber"
            | "eth_getUncleCountByBlockHash"
            | "eth_getRawTransactionByHash"
            | "debug_getRawTransaction"
            | "debug_getRawHeader"
            | "debug_getRawBlock"
            | "debug_getRawReceipts"
            | "eth_getLogs"
            | "eth_newFilter"
            | "eth_getFilterChanges"
            | "eth_getFilterLogs"
            | "eth_uninstallFilter"
            | "trace_block"
            | "trace_transaction"
            | "base_transactionStatus"
            | "eth_sendRawTransaction" => (1, 1),
            "eth_call" | "eth_estimateGas" => (4, 1),
            "eth_createAccessList" | "debug_traceCall" => (3, 1),
            "eth_simulateV1" => (2, 1),
            "eth_feeHistory" => (3, 2),
            "debug_traceTransaction" | "debug_traceBlockByNumber" | "debug_traceBlockByHash" => (2, 1),
            _ => return None,
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
    }
}
