//! `eth_getLogs` and the polling filters (`eth_newFilter`, `eth_newBlockFilter`,
//! `eth_getFilterChanges`, `eth_getFilterLogs`, `eth_uninstallFilter`) as reth's `EthFilter`
//! answers them, with reth's default limits.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{B256, U128, keccak256};
use alloy_rpc_types_eth::{Filter, FilterBlockOption};
use base_common_rpc_types::BaseLogResponse;
use serde_json::{Value, json};

use super::param;
use crate::{Node, RpcError, chain::StoredBlock};

/// reth's `--rpc.max-blocks-per-filter` default.
const MAX_BLOCKS_PER_FILTER: u64 = 100_000;
/// reth's `--rpc.max-logs-per-response` default.
const MAX_LOGS_PER_RESPONSE: usize = 20_000;
/// reth's log scan step: each window covers this many blocks after its first.
const HEADER_WINDOW: u64 = 1_000;
/// reth drops a filter this long after its last poll (checked as often), so 5 to 10 minutes.
const STALE_FILTER_TTL: Duration = Duration::from_secs(5 * 60);
/// At most this many filters are installed; the oldest-polled is dropped beyond it.
const FILTERS_MAX: usize = 4096;

/// What a filter returns.
#[derive(Debug, Clone)]
enum FilterKind {
    /// Hashes of new canonical blocks.
    Block,
    /// Logs matching the filter.
    Log(Box<Filter>),
}

/// An installed filter.
#[derive(Debug, Clone)]
struct Installed {
    /// The first block the next `eth_getFilterChanges` reports.
    block: u64,
    last_poll: Instant,
    kind: FilterKind,
}

/// The node's installed filters.
#[derive(Debug, Default)]
pub struct RpcFilters {
    filters: Mutex<HashMap<String, Installed>>,
}

/// A block range resolved from a filter, inclusive.
#[derive(Debug, Clone, Copy)]
struct Range {
    from: u64,
    to: u64,
}

impl Node {
    /// Answers the log and filter methods; `None` for other methods.
    pub(super) fn rpc_filters(&self, method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            "eth_getLogs" => param::<Filter>(p, 0).and_then(|filter| self.logs(&filter)),
            "eth_newFilter" => param::<Filter>(p, 0).map(|filter| self.install(FilterKind::Log(Box::new(filter)))),
            "eth_newBlockFilter" => Ok(self.install(FilterKind::Block)),
            "eth_getFilterChanges" => param::<Value>(p, 0).and_then(|id| self.filter_changes(&id)),
            "eth_getFilterLogs" => param::<Value>(p, 0).and_then(|id| self.filter_logs(&id)),
            "eth_uninstallFilter" => param::<Value>(p, 0).map(|id| {
                let id = Self::filter_id(&id);
                json!(id.is_some_and(|id| self.filters.filters.lock().expect("filters lock").remove(&id).is_some()))
            }),
            _ => return None,
        })
    }

    /// `eth_getLogs`.
    fn logs(&self, filter: &Filter) -> Result<Value, RpcError> {
        if let FilterBlockOption::AtBlockHash(hash) = filter.block_option {
            return self.logs_at_hash(filter, hash);
        }
        let best = self.head_number();
        let (mut from, mut to) = (filter.block_option.get_from_block().copied(), filter.block_option.get_to_block().copied());
        // Base's pending block is the latest block.
        if from == Some(BlockNumberOrTag::Pending) {
            match to.unwrap_or(BlockNumberOrTag::Pending) {
                BlockNumberOrTag::Pending => {}
                BlockNumberOrTag::Number(number) if number >= best => {}
                _ => return Ok(json!([])),
            }
        }
        if from == Some(BlockNumberOrTag::Pending) {
            from = None;
        }
        if to == Some(BlockNumberOrTag::Pending) {
            to = None;
        }
        let from = from.map(|tag| self.convert_block_number(tag)).transpose()?;
        let to = to.map(|tag| self.convert_block_number(tag)).transpose()?;
        if to.is_some_and(|to| to > best) {
            return Err(RpcError::invalid_params("block range extends beyond current head block".into()));
        }
        let range = Self::filter_block_range(from, to, best, best)?;
        self.logs_in_range(filter, range).map(|logs| json!(logs))
    }

    /// The logs of the block with hash `hash`; unknown is `-32001`.
    fn logs_at_hash(&self, filter: &Filter, hash: B256) -> Result<Value, RpcError> {
        let chain = self.chain.read().expect("chain lock");
        if let Some(number) = chain.expired(&hash) {
            return Err(Self::pruned(number, chain.earliest()));
        }
        let block = chain.block(&hash).ok_or_else(|| RpcError::new(-32001, format!("block not found: hash {hash}")))?;
        drop(chain);
        Ok(json!(Self::block_logs(filter, &block)))
    }

    /// reth's `convert_block_number` for a filter bound: unset `safe`/`finalized` is `-32001`.
    fn convert_block_number(&self, tag: BlockNumberOrTag) -> Result<u64, RpcError> {
        let chain = self.chain.read().expect("chain lock");
        Ok(match tag {
            BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => chain.head().number(),
            BlockNumberOrTag::Earliest => 0,
            BlockNumberOrTag::Number(number) => number,
            BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized => {
                let forkchoice = chain.forkchoice();
                let hash = if tag == BlockNumberOrTag::Safe { forkchoice.safe } else { forkchoice.finalized };
                chain.block(&hash).map(|block| block.number()).ok_or_else(|| {
                    RpcError::new(-32001, format!("block not found: {tag}"))
                })?
            }
        })
    }

    /// reth's `get_filter_block_range`: an absent `from` is `start`, an absent `to` is `best`.
    fn filter_block_range(from: Option<u64>, to: Option<u64>, start: u64, best: u64) -> Result<Range, RpcError> {
        let from = from.unwrap_or(start);
        let to = to.unwrap_or(best);
        if from > to {
            return Err(RpcError::invalid_params("invalid block range params".into()));
        }
        if to > best {
            return Err(RpcError::invalid_params("block range extends beyond current head block".into()));
        }
        Ok(Range { from, to })
    }

    /// reth's `get_logs_in_block_range`: the span and response limits, then a bloom-guided scan.
    fn logs_in_range(&self, filter: &Filter, range: Range) -> Result<Vec<BaseLogResponse>, RpcError> {
        if range.to - range.from > MAX_BLOCKS_PER_FILTER {
            return Err(RpcError::invalid_params(format!(
                "query exceeds max block range {MAX_BLOCKS_PER_FILTER}"
            )));
        }
        let chain = self.chain.read().expect("chain lock");
        if Self::is_pruned(&chain, range.from) {
            return Err(Self::pruned(range.from, chain.earliest()));
        }
        drop(chain);
        // reth reads headers in windows of `HEADER_WINDOW + 1` blocks (its range iterator's end
        // is inclusive); a window yields the blocks from its start up to the first one it does
        // not have, so a window starting below the first stored block yields none.
        let mut logs = Vec::new();
        let mut window_start = range.from;
        while window_start <= range.to {
            let window_end = range.to.min(window_start.saturating_add(HEADER_WINDOW));
            for number in window_start..=window_end {
                let block = self.chain.read().expect("chain lock").canonical_block(number);
                let Some(block) = block else { break };
                if !filter.matches_bloom(block.header.logs_bloom) {
                    continue;
                }
                logs.extend(Self::block_logs(filter, &block));
                if range.from != range.to && logs.len() > MAX_LOGS_PER_RESPONSE {
                    return Err(RpcError::invalid_params(format!(
                        "query exceeds max results {MAX_LOGS_PER_RESPONSE}, retry with the range {}-{}",
                        range.from, number
                    )));
                }
            }
            let Some(next) = window_end.checked_add(1) else { break };
            window_start = next;
        }
        Ok(logs)
    }

    /// The logs of `block` that match `filter`, with block-wide log indexes.
    fn block_logs(filter: &Filter, block: &StoredBlock) -> Vec<BaseLogResponse> {
        let mut first_log_index = 0;
        let mut logs = Vec::new();
        for index in 0..block.receipts.len() {
            let transaction_logs = Self::transaction_logs(block, index, first_log_index);
            first_log_index += transaction_logs.len();
            logs.extend(transaction_logs.into_iter().filter(|log| {
                filter.matches_address(log.inner.address()) && filter.matches_topics(log.inner.topics())
            }));
        }
        logs
    }

    fn head_number(&self) -> u64 {
        self.chain.read().expect("chain lock").head().number()
    }

    /// Installs a filter from the current head, returning reth's random hex id.
    fn install(&self, kind: FilterKind) -> Value {
        let block = self.head_number();
        let now = Instant::now();
        let mut filters = self.filters.filters.lock().expect("filters lock");
        filters.retain(|_, filter| now.duration_since(filter.last_poll) < 2 * STALE_FILTER_TTL);
        if filters.len() >= FILTERS_MAX {
            let oldest = filters.iter().min_by_key(|(_, filter)| filter.last_poll).map(|(id, _)| id.clone());
            filters.remove(&oldest.expect("the map is full"));
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let seed = keccak256([nanos.as_nanos().to_le_bytes(), (filters.len() as u128).to_le_bytes()].concat());
        let id = format!("{:#x}", U128::from_be_slice(&seed[..16]));
        filters.insert(id.clone(), Installed { block, last_poll: now, kind });
        json!(id)
    }

    /// A filter id: reth's ids are strings, so a JSON number never names one.
    fn filter_id(id: &Value) -> Option<String> {
        id.as_str().map(str::to_owned)
    }

    /// The live filter `id`, or reth's `-32000 filter not found`.
    fn filter(&self, id: &Value) -> Result<(String, Installed), RpcError> {
        let not_found = || RpcError::new(-32000, "filter not found");
        let id = Self::filter_id(id).ok_or_else(not_found)?;
        let filters = self.filters.filters.lock().expect("filters lock");
        let filter = filters.get(&id).filter(|filter| filter.last_poll.elapsed() < 2 * STALE_FILTER_TTL);
        Ok((id.clone(), filter.cloned().ok_or_else(not_found)?))
    }

    /// `eth_getFilterChanges`: what happened since the last poll (the install-time head included
    /// on the first poll).
    fn filter_changes(&self, id: &Value) -> Result<Value, RpcError> {
        let (id, installed) = self.filter(id)?;
        let best = self.head_number();
        if installed.block > best {
            return Ok(json!([]));
        }
        let start = installed.block;
        if let Some(filter) = self.filters.filters.lock().expect("filters lock").get_mut(&id) {
            filter.block = best + 1;
            filter.last_poll = Instant::now();
        }
        match installed.kind {
            FilterKind::Block => {
                let chain = self.chain.read().expect("chain lock");
                let hashes: Vec<B256> = (start..=best).filter_map(|number| chain.canonical_hash(number)).collect();
                Ok(json!(hashes))
            }
            FilterKind::Log(filter) => match filter.block_option {
                FilterBlockOption::AtBlockHash(hash) => self.logs_at_hash(&filter, hash),
                FilterBlockOption::Range { from_block, to_block } => {
                    // Without filtering `pending`, which reth's provider resolves to no block.
                    let bound = |tag: Option<BlockNumberOrTag>| match tag {
                        None | Some(BlockNumberOrTag::Pending) => Ok(None),
                        Some(tag) => self.convert_block_number(tag).map(Some),
                    };
                    let range = Self::filter_block_range(bound(from_block)?, bound(to_block)?, start, best)?;
                    self.logs_in_range(&filter, range).map(|logs| json!(logs))
                }
            },
        }
    }

    /// `eth_getFilterLogs`: `eth_getLogs` with a log filter's stored filter.
    fn filter_logs(&self, id: &Value) -> Result<Value, RpcError> {
        let (id, installed) = self.filter(id)?;
        let FilterKind::Log(filter) = installed.kind else {
            return Err(RpcError::new(-32000, "filter not found"));
        };
        if let Some(installed) = self.filters.filters.lock().expect("filters lock").get_mut(&id) {
            installed.last_poll = Instant::now();
        }
        self.logs(&filter)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::test_utils::{node, reply};

    fn call(node: &crate::Node, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        reply(node, &body.to_string()).expect("a request is answered")
    }

    #[test]
    fn filters_poll_and_fail_as_reth() {
        let node = node();
        let genesis_hash = call(&node, "eth_getBlockByNumber", json!(["latest", false]))["result"]["hash"].clone();
        let block_filter = call(&node, "eth_newBlockFilter", json!([]))["result"].clone();
        // The first poll includes the head the filter was installed at; the next has nothing new.
        assert_eq!(call(&node, "eth_getFilterChanges", json!([block_filter]))["result"], json!([genesis_hash]));
        assert_eq!(call(&node, "eth_getFilterChanges", json!([block_filter]))["result"], json!([]));
        assert_eq!(call(&node, "eth_getFilterLogs", json!([block_filter]))["error"]["code"], json!(-32000));
        assert_eq!(call(&node, "eth_uninstallFilter", json!([block_filter]))["result"], json!(true));
        assert_eq!(call(&node, "eth_uninstallFilter", json!([block_filter]))["result"], json!(false));
        assert_eq!(call(&node, "eth_getFilterChanges", json!([block_filter]))["error"]["code"], json!(-32000));
        // Ranges past the head or backwards are -32602; an unknown block hash is -32001.
        let logs = |filter: Value| call(&node, "eth_getLogs", json!([filter]));
        assert_eq!(logs(json!({ "toBlock": "0x1" }))["error"]["code"], json!(-32602));
        assert_eq!(logs(json!({ "fromBlock": "0x1" }))["error"]["code"], json!(-32602));
        assert_eq!(logs(json!({ "blockHash": format!("0x{}", "11".repeat(32)) }))["error"]["code"], json!(-32001));
        assert_eq!(logs(json!({ "fromBlock": "earliest", "toBlock": "latest" }))["result"], json!([]));
    }
}
