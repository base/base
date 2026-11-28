//! Method parameters read as base-reth's server reads them. jsonrpsee 0.26's `#[rpc]` macro
//! decodes a JSON array one parameter at a time from the raw request text (`ParamsSequence`),
//! and a JSON object as a struct whose fields are the Rust argument names (`params.parse()`);
//! the serde errors of those decodes, positions included, are the `-32602` error's `data`. Each
//! method declares reth's argument names and types with [`signature!`], which decodes the same
//! types the same way, then hands the method its parameters as a positional list.

use serde::Deserialize;
use serde_json::Value;

use crate::RpcError;

/// jsonrpsee's `-32602`: message `Invalid params`, the decode error as `data`.
pub(super) fn invalid_params(data: impl ToString) -> RpcError {
    RpcError { code: -32602, message: "Invalid params".into(), data: Some(Value::String(data.to_string())) }
}

/// jsonrpsee's `ParamsSequence` over the raw text of a parameter array.
#[derive(Debug)]
pub(super) struct Sequence<'a> {
    rest: &'a str,
    /// The raw text of each parameter decoded so far, `None` for an absent optional one.
    pub values: Vec<Option<&'a str>>,
}

impl<'a> Sequence<'a> {
    /// The sequence over `raw`; an absent list and `[]` (exactly) have no parameters.
    pub(super) fn new(raw: Option<&'a str>) -> Self {
        let rest = match raw {
            Some("[]") | None => "",
            Some(raw) => raw,
        };
        Self { rest, values: Vec::new() }
    }

    fn next_inner<T: Deserialize<'a>>(&mut self) -> Option<Result<(T, &'a str), RpcError>> {
        let json = match self.rest.as_bytes().first()? {
            b']' => {
                self.rest = "";
                return None;
            }
            b'[' | b',' => &self.rest[1..],
            _ => {
                let found = self.rest;
                return Some(Err(invalid_params(format!(
                    "Invalid params. Expected one of '[', ']' or ',' but found {found:?}"
                ))));
            }
        };
        let mut values = serde_json::Deserializer::from_str(json).into_iter::<T>();
        match values.next()? {
            Ok(value) => {
                let end = values.byte_offset();
                self.rest = json[end..].trim_start();
                Some(Ok((value, &json[..end])))
            }
            Err(error) => {
                self.rest = "";
                Some(Err(invalid_params(error)))
            }
        }
    }

    /// A required parameter (`seq.next()`).
    pub(super) fn next<T: Deserialize<'a>>(&mut self) -> Result<(), RpcError> {
        let (_, raw) = self.next_inner::<T>().unwrap_or_else(|| Err(invalid_params("No more params")))?;
        self.values.push(Some(raw));
        Ok(())
    }

    /// An `Option` parameter (`seq.optional_next()`): absent at the end of the list.
    pub(super) fn optional_next<T: Deserialize<'a>>(&mut self) -> Result<(), RpcError> {
        let raw = self.next_inner::<Option<T>>().transpose()?.map(|(_, raw)| raw);
        self.values.push(raw);
        Ok(())
    }

    /// The decoded parameters as JSON values, `null` for absent ones.
    pub(super) fn into_values(self) -> Vec<Value> {
        self.values
            .into_iter()
            .map(|raw| raw.map_or(Value::Null, |raw| serde_json::from_str(raw).expect("decoded parameters are JSON")))
            .collect()
    }
}

/// The parameters of an object, by argument name or its lowerCamelCase alias, in order.
pub(super) fn object_values(raw: &str, names: &[(&str, &str)]) -> Vec<Value> {
    let object: serde_json::Map<String, Value> = serde_json::from_str(raw).expect("decoded parameters are an object");
    names
        .iter()
        .map(|(name, camel)| object.get(*name).or_else(|| object.get(*camel)).cloned().unwrap_or(Value::Null))
        .collect()
}

/// Decodes raw parameters `$raw` (`Option<&str>`) as a reth method with these arguments does,
/// into a positional list. Each argument is `name "rustName" "camelName": req Type` or
/// `... : opt Type` for an argument declared `Option<Type>`.
macro_rules! signature {
    ($raw:expr; $($field:ident $name:literal $camel:literal : $kind:ident $ty:ty),+ $(,)?) => {{
        let raw: Option<&str> = $raw;
        match raw {
            Some(object) if object.starts_with('{') => {
                #[derive(serde::Deserialize)]
                #[allow(dead_code)]
                struct ParamsObject {
                    $(
                        #[serde(alias = $name, alias = $camel, rename = $name)]
                        $field: signature!(@type $kind $ty),
                    )+
                }
                serde_json::from_str::<ParamsObject>(object).map_err(super::params::invalid_params)?;
                super::params::object_values(object, &[$(($name, $camel)),+])
            }
            _ => {
                let mut sequence = super::params::Sequence::new(raw);
                $( signature!(@next sequence $kind $ty); )+
                sequence.into_values()
            }
        }
    }};
    (@type req $ty:ty) => { $ty };
    (@type opt $ty:ty) => { Option<$ty> };
    (@next $sequence:ident req $ty:ty) => { $sequence.next::<$ty>()? };
    (@next $sequence:ident opt $ty:ty) => { $sequence.optional_next::<$ty>()? };
}

impl crate::Node {
    /// The parameters of `method` decoded from `raw` as base-reth's signature for it decodes
    /// them (`None` for a method tiger does not serve, whose parameters are never read).
    pub(super) fn reth_params(method: &str, raw: Option<&str>) -> Option<Result<Vec<Value>, RpcError>> {
        Self::decode_params(method, raw).transpose()
    }

    fn decode_params(method: &str, raw: Option<&str>) -> Result<Option<Vec<Value>>, RpcError> {
        use alloy_eips::{BlockId, BlockNumberOrTag};
        use alloy_primitives::{Address, B256, Bytes, U64, U256};
        use alloy_rpc_types_eth::{BlockOverrides, Filter, FilterId, Index, state::StateOverride};
        use alloy_serde::JsonStorageKey;
        use base_common_rpc_types::BaseTransactionRequest;

        Ok(Some(match method {
            // Methods without arguments never read their parameters.
            "eth_chainId" | "eth_blockNumber" | "net_version" | "web3_clientVersion" | "eth_syncing"
            | "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_blobBaseFee" | "eth_newBlockFilter"
            | "eth_config" | "rpc_modules" => Vec::new(),
            "eth_feeHistory" => signature!(raw;
                block_count "block_count" "blockCount": req U64,
                newest_block "newest_block" "newestBlock": req BlockNumberOrTag,
                reward_percentiles "reward_percentiles" "rewardPercentiles": opt Vec<f64>),
            "eth_getBlockByHash" => signature!(raw; hash "hash" "hash": req B256, full "full" "full": req bool),
            "eth_getBlockByNumber" => {
                signature!(raw; number "number" "number": req BlockNumberOrTag, full "full" "full": req bool)
            }
            "eth_getBlockReceipts" | "debug_getRawHeader" | "debug_getRawBlock" | "debug_getRawReceipts" => {
                signature!(raw; block_id "block_id" "blockId": req BlockId)
            }
            "eth_getBlockTransactionCountByHash"
            | "eth_getUncleCountByBlockHash"
            | "eth_getTransactionByHash"
            | "eth_getRawTransactionByHash"
            | "eth_getTransactionReceipt"
            | "debug_getRawTransaction" => signature!(raw; hash "hash" "hash": req B256),
            "eth_getBlockTransactionCountByNumber" | "eth_getUncleCountByBlockNumber" => {
                signature!(raw; number "number" "number": req BlockNumberOrTag)
            }
            "eth_getUncleByBlockHashAndIndex"
            | "eth_getTransactionByBlockHashAndIndex"
            | "eth_getRawTransactionByBlockHashAndIndex" => {
                signature!(raw; hash "hash" "hash": req B256, index "index" "index": req Index)
            }
            "eth_getUncleByBlockNumberAndIndex"
            | "eth_getTransactionByBlockNumberAndIndex"
            | "eth_getRawTransactionByBlockNumberAndIndex" => {
                signature!(raw; number "number" "number": req BlockNumberOrTag, index "index" "index": req Index)
            }
            "eth_getTransactionBySenderAndNonce" => {
                signature!(raw; address "address" "address": req Address, nonce "nonce" "nonce": req U64)
            }
            "eth_getBalance" | "eth_getCode" => signature!(raw;
                address "address" "address": req Address,
                block_number "block_number" "blockNumber": opt BlockId),
            "eth_getStorageAt" => signature!(raw;
                address "address" "address": req Address,
                index "index" "index": req JsonStorageKey,
                block_number "block_number" "blockNumber": opt BlockId),
            "eth_getTransactionCount" => signature!(raw;
                address "address" "address": req Address,
                block_number "block_number" "blockNumber": opt BlockId,
                nonce_key "nonce_key" "nonceKey": opt U256),
            "eth_getProof" => signature!(raw;
                address "address" "address": req Address,
                keys "keys" "keys": req Vec<JsonStorageKey>,
                block_number "block_number" "blockNumber": opt BlockId),
            // Base's overrides name the third argument `state_overrides`.
            "eth_call" | "eth_estimateGas" => signature!(raw;
                request "request" "request": req BaseTransactionRequest,
                block_number "block_number" "blockNumber": opt BlockId,
                state_overrides "state_overrides" "stateOverrides": opt StateOverride,
                block_overrides "block_overrides" "blockOverrides": opt Box<BlockOverrides>),
            "eth_createAccessList" => signature!(raw;
                request "request" "request": req BaseTransactionRequest,
                block_number "block_number" "blockNumber": opt BlockId,
                state_override "state_override" "stateOverride": opt StateOverride),
            "eth_getLogs" | "eth_newFilter" => signature!(raw; filter "filter" "filter": req Filter),
            "eth_getFilterChanges" | "eth_getFilterLogs" | "eth_uninstallFilter" => {
                signature!(raw; id "id" "id": req FilterId)
            }
            "eth_sendRawTransaction" => signature!(raw; bytes "bytes" "bytes": req Bytes),
            "base_transactionStatus" => signature!(raw; tx_hash "tx_hash" "txHash": req B256),
            _ => return Ok(None),
        }))
    }
}
