//! Fetches Base mainnet blocks as validate-mode requests and caches them on disk.

use std::{
    cmp::Reverse,
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process, thread,
};

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy_eips::Encodable2718;
use alloy_primitives::{B64, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable;
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_trie::{Nibbles, TrieAccount, proof::verify_proof, root::ordered_trie_root_with_encoder};
use base_common_chains::ChainConfig;
use base_common_consensus::Predeploys;
use base_common_rpc_types::BaseBlockResponse;
use eyre::{Context, bail, ensure, eyre};
use serde_json::json;

use super::RpcClient;
use crate::{HeaderResults, Mode, Request, RequestHeader, Witness};

/// Turns Base mainnet block numbers into complete validate-mode [`Request`]s.
///
/// A case is assembled from two calls: `debug_getRawBlock` on the block endpoints, which gives
/// the exact header RLP and EIP-2718 transaction bytes, and `debug_executionWitness` on the
/// witness endpoints, which gives the parent-state trie nodes, the bytecodes, and the ancestor
/// headers the block's `BLOCKHASH` reads need. From Isthmus the header's `withdrawalsRoot` is
/// the `L2ToL1MessagePasser` storage root, which a client must read even when the block leaves
/// the message passer untouched; reth's witness only records state the EVM touched, so the
/// message passer's account proof at the parent (`eth_getProof` on the proof endpoints,
/// verified against the parent state root) is merged into the witness. Before a case is returned or cached it must pass
/// [`MainnetFetcher::validate`], and the header must hash to the block hash that
/// `eth_getBlockByNumber` reports.
///
/// Finished cases are cached as `<cache_dir>/<number>.json`, one [`Request`] per file. A cached
/// case is revalidated and returned without any network access, so reruns work offline; set
/// [`MainnetFetcher::offline`] to make a cache miss an error instead of a fetch.
#[derive(Debug, Clone)]
pub struct MainnetFetcher {
    /// Directory holding one `<number>.json` request per fetched block.
    pub cache_dir: PathBuf,
    /// Endpoints serving `debug_executionWitness`.
    pub witness: RpcClient,
    /// Endpoints serving `debug_getRawBlock` and `eth_getBlockByNumber`.
    pub blocks: RpcClient,
    /// Endpoints serving `eth_getProof` at recent parents.
    pub proofs: RpcClient,
    /// When set, only cached cases are returned and nothing is fetched.
    pub offline: bool,
}

impl MainnetFetcher {
    /// Chain name written into every request.
    pub const CHAIN: &str = "base-mainnet";
    /// Most ancestors a request carries (the `BLOCKHASH` window).
    pub const MAX_ANCESTORS: usize = 256;
    /// Default witness endpoints. 1rpc.io is a load-balanced gateway whose backends often fail
    /// or time out; the public `BlockPI` endpoint serves witnesses for recent blocks reliably.
    pub const DEFAULT_WITNESS_URLS: [&str; 2] =
        ["https://1rpc.io/base", "https://base.public.blockpi.network/v1/rpc/public"];
    /// Default block endpoints; they must serve `debug_getRawBlock`.
    pub const DEFAULT_BLOCK_URLS: [&str; 1] = ["https://base.public.blockpi.network/v1/rpc/public"];
    /// Default proof endpoints; they must serve `eth_getProof` at the parent block.
    pub const DEFAULT_PROOF_URLS: [&str; 2] =
        ["https://base-mainnet.public.blastapi.io", "https://base.drpc.org"];
    /// Environment variable overriding the proof endpoints (comma-separated URLs).
    pub const PROOF_RPC_ENV: &str = "BASE_DIFF_PROOF_RPC";
    /// Environment variable overriding the witness endpoints (comma-separated URLs).
    pub const WITNESS_RPC_ENV: &str = "BASE_DIFF_WITNESS_RPC";
    /// Environment variable overriding the block endpoints (comma-separated URLs).
    pub const BLOCK_RPC_ENV: &str = "BASE_DIFF_BLOCK_RPC";

    /// Creates a fetcher with explicit endpoints.
    pub fn new(
        cache_dir: impl Into<PathBuf>,
        witness_urls: Vec<String>,
        block_urls: Vec<String>,
        proof_urls: Vec<String>,
    ) -> eyre::Result<Self> {
        Ok(Self {
            cache_dir: cache_dir.into(),
            witness: RpcClient::new(witness_urls)?,
            blocks: RpcClient::new(block_urls)?,
            proofs: RpcClient::new(proof_urls)?,
            offline: false,
        })
    }

    /// Creates a fetcher whose endpoints come from [`Self::WITNESS_RPC_ENV`],
    /// [`Self::BLOCK_RPC_ENV`], and [`Self::PROOF_RPC_ENV`] when set, and from the public defaults otherwise.
    pub fn from_env(cache_dir: impl Into<PathBuf>) -> eyre::Result<Self> {
        let urls = |var: &str, defaults: &[&str]| match env::var(var) {
            Ok(value) if !value.trim().is_empty() => value
                .split(',')
                .map(|url| url.trim().to_string())
                .filter(|url| !url.is_empty())
                .collect(),
            _ => defaults.iter().map(|url| url.to_string()).collect(),
        };
        Self::new(
            cache_dir,
            urls(Self::WITNESS_RPC_ENV, &Self::DEFAULT_WITNESS_URLS),
            urls(Self::BLOCK_RPC_ENV, &Self::DEFAULT_BLOCK_URLS),
            urls(Self::PROOF_RPC_ENV, &Self::DEFAULT_PROOF_URLS),
        )
    }

    /// Path of the cached request for block `number`.
    pub fn cache_path(&self, number: u64) -> PathBuf {
        self.cache_dir.join(format!("{number}.json"))
    }

    /// Returns the validated request for block `number`, from the cache when present.
    pub fn fetch_case(&self, number: u64) -> eyre::Result<Request> {
        let path = self.cache_path(number);
        if path.exists() {
            let request: Request = serde_json::from_slice(&fs::read(&path)?)
                .wrap_err_with(|| format!("parsing cached case {}", path.display()))?;
            ensure!(request.header.number == number, "{} holds another block", path.display());
            Self::validate(&request)
                .wrap_err_with(|| format!("cached case {} is invalid", path.display()))?;
            return Ok(request);
        }
        if self.offline {
            bail!(
                "block {number} is not cached in {} and fetching is disabled",
                self.cache_dir.display()
            );
        }
        let request = self.download(number).wrap_err_with(|| format!("block {number}"))?;
        Self::write_cache(&path, &request)?;
        Ok(request)
    }

    /// Fetches blocks `first..first + count` with up to `concurrency` blocks in flight and
    /// returns one result per block, in block order.
    pub fn fetch_range(
        &self,
        first: u64,
        count: u64,
        concurrency: usize,
    ) -> Vec<(u64, eyre::Result<Request>)> {
        let workers = (concurrency.max(1) as u64).min(count.max(1));
        let mut results: Vec<(u64, eyre::Result<Request>)> = thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    scope.spawn(move || {
                        (first + worker..first + count)
                            .step_by(workers as usize)
                            .map(|number| (number, self.fetch_case(number)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("fetch worker panicked"))
                .collect()
        });
        results.sort_by_key(|(number, _)| *number);
        results
    }

    /// Checks a validate-mode request for internal consistency with the canonical chain.
    ///
    /// - the transactions hash to `transactionsRoot`;
    /// - the ancestors decode as contiguous headers, newest first, starting at the parent, at
    ///   most [`Self::MAX_ANCESTORS`] of them;
    /// - the full header rebuilt from the request fields (with `parentHash` = hash of
    ///   `ancestors[0]` and the post-merge constants) hashes to `hash`;
    /// - the witness contains the parent state root node.
    pub fn validate(request: &Request) -> eyre::Result<()> {
        ensure!(request.mode == Mode::Validate, "not a validate-mode request");
        let header = &request.header;
        let results = header.results.as_ref().ok_or_else(|| eyre!("missing header results"))?;

        let transactions_root = ordered_trie_root_with_encoder(&request.transactions, |tx, buf| {
            buf.extend_from_slice(tx)
        });
        ensure!(
            transactions_root == results.transactions_root,
            "transactions hash to {transactions_root}, header has {}",
            results.transactions_root
        );

        ensure!(!request.ancestors.is_empty(), "no ancestors");
        ensure!(request.ancestors.len() <= Self::MAX_ANCESTORS, "more than 256 ancestors");
        let mut child_number = header.number;
        let mut parent: Option<Header> = None;
        let mut expected_hash = None;
        for raw in &request.ancestors {
            let ancestor = Header::decode(&mut raw.as_ref()).wrap_err("decoding ancestor")?;
            ensure!(
                ancestor.number + 1 == child_number,
                "ancestor {} is not contiguous",
                ancestor.number
            );
            if let Some(expected) = expected_hash {
                ensure!(
                    keccak256(raw) == expected,
                    "ancestor {} breaks the hash chain",
                    ancestor.number
                );
            }
            child_number = ancestor.number;
            expected_hash = Some(ancestor.parent_hash);
            parent.get_or_insert(ancestor);
        }
        let parent = parent.expect("ancestors are not empty");

        let rebuilt = Header {
            parent_hash: keccak256(&request.ancestors[0]),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: header.coinbase,
            state_root: results.state_root,
            transactions_root: results.transactions_root,
            receipts_root: results.receipts_root,
            logs_bloom: results.logs_bloom,
            difficulty: U256::ZERO,
            number: header.number,
            gas_limit: header.gas_limit,
            gas_used: results.gas_used,
            timestamp: header.timestamp,
            extra_data: header.extra_data.clone(),
            mix_hash: header.prev_randao,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(header.base_fee_per_gas),
            withdrawals_root: results.withdrawals_root,
            blob_gas_used: results.blob_gas_used,
            excess_blob_gas: header.excess_blob_gas,
            parent_beacon_block_root: header.parent_beacon_block_root,
            requests_hash: results.requests_hash,
            // Ethereum Amsterdam fields; no Base fork has them.
            block_access_list_hash: None,
            slot_number: None,
        };
        let rebuilt_hash = rebuilt.hash_slow();
        ensure!(
            rebuilt_hash == results.hash,
            "header rebuilt from the request hashes to {rebuilt_hash}, block hash is {}",
            results.hash
        );

        ensure!(
            request.witness.state.iter().any(|node| keccak256(node) == parent.state_root),
            "witness lacks the parent state root node {}",
            parent.state_root
        );
        Ok(())
    }

    /// Downloads and validates block `number`.
    pub fn download(&self, number: u64) -> eyre::Result<Request> {
        let tag = json!([format!("{number:#x}")]);

        // Few public endpoints serve `debug_getRawBlock`, so each is asked once before falling
        // back to the JSON block.
        let body =
            json!({ "jsonrpc": "2.0", "id": 1, "method": "debug_getRawBlock", "params": tag });
        let raw_block = self.blocks.urls.iter().find_map(|url| {
            serde_json::from_value::<Bytes>(self.blocks.call_once(url, &body).ok()?).ok()
        });
        let (raw_header, transactions) = match raw_block {
            Some(raw_block) => Self::split_block(&raw_block)?,
            None => self.block_from_json(number)?,
        };
        let header = Header::decode(&mut raw_header.as_ref()).wrap_err("decoding header")?;
        ensure!(header.number == number, "raw block is block {}", header.number);

        let block =
            self.blocks.call("eth_getBlockByNumber", json!([format!("{number:#x}"), false]))?;
        let canonical_hash: B256 = serde_json::from_value(block["hash"].clone())?;
        ensure!(
            keccak256(&raw_header) == canonical_hash,
            "raw header hashes to {}, block hash is {canonical_hash}",
            keccak256(&raw_header)
        );
        ensure!(header.hash_slow() == canonical_hash, "header does not re-encode to its RLP");

        // Some gateway backends answer with an empty witness; ask again.
        let mut witness = ExecutionWitness::default();
        for _ in 0..6 {
            witness =
                serde_json::from_value(self.witness.call("debug_executionWitness", tag.clone())?)?;
            if !witness.state.is_empty() {
                break;
            }
        }
        if !witness.keys.is_empty() {
            // Key preimages (addresses and storage slots) let a case recover the accounts the
            // block read from its witness; they are not part of the request.
            fs::create_dir_all(&self.cache_dir)?;
            fs::write(
                self.cache_dir.join(format!("{number}.keys.json")),
                serde_json::to_vec(&witness.keys)?,
            )?;
        }
        let ancestors = Self::order_ancestors(witness.headers, header.number)?;
        if header.timestamp >= ChainConfig::mainnet().isthmus_timestamp {
            let parent = Header::decode(&mut ancestors[0].as_ref())?;
            let known: HashSet<B256> = witness.state.iter().map(keccak256).collect();
            let proof = self.message_passer_proof(parent.number, parent.state_root)?;
            witness
                .state
                .extend(proof.into_iter().filter(|node| !known.contains(&keccak256(node))));
        }

        let base_fee_per_gas =
            header.base_fee_per_gas.ok_or_else(|| eyre!("header has no base fee"))?;
        let request = Request {
            id: format!("mainnet-{number}"),
            chain: Self::CHAIN.to_string(),
            mode: Mode::Validate,
            ancestors,
            header: RequestHeader {
                number,
                timestamp: header.timestamp,
                coinbase: header.beneficiary,
                prev_randao: header.mix_hash,
                gas_limit: header.gas_limit,
                base_fee_per_gas,
                extra_data: header.extra_data.clone(),
                parent_beacon_block_root: header.parent_beacon_block_root,
                excess_blob_gas: header.excess_blob_gas,
                results: Some(HeaderResults {
                    state_root: header.state_root,
                    receipts_root: header.receipts_root,
                    transactions_root: header.transactions_root,
                    logs_bloom: header.logs_bloom,
                    gas_used: header.gas_used,
                    blob_gas_used: header.blob_gas_used,
                    withdrawals_root: header.withdrawals_root,
                    requests_hash: header.requests_hash,
                    hash: canonical_hash,
                }),
            },
            transactions,
            witness: Witness { state: witness.state, codes: witness.codes },
            trace: None,
            system_config: None,
        };
        ensure!(
            keccak256(&request.ancestors[0]) == header.parent_hash,
            "witness headers do not end at the block's parent"
        );
        Self::validate(&request)?;
        Ok(request)
    }

    /// Rebuilds block `number`'s header RLP and EIP-2718 transactions from
    /// `eth_getBlockByNumber` with full transactions, for endpoints without `debug_getRawBlock`.
    /// The caller checks the header against the block hash and the transactions against the
    /// transactions root.
    pub fn block_from_json(&self, number: u64) -> eyre::Result<(Bytes, Vec<Bytes>)> {
        let block: BaseBlockResponse = serde_json::from_value(
            self.blocks.call("eth_getBlockByNumber", json!([format!("{number:#x}"), true]))?,
        )?;
        let raw_header = Bytes::from(alloy_rlp::encode(&block.header.inner.inner));
        let transactions = block
            .transactions
            .into_transactions()
            .map(|tx| Bytes::from(tx.inner.inner.into_inner().encoded_2718()))
            .collect();
        Ok((raw_header, transactions))
    }

    /// Fetches the `L2ToL1MessagePasser` account proof at block `number` and verifies it against
    /// `state_root`, returning the proof's trie nodes.
    pub fn message_passer_proof(&self, number: u64, state_root: B256) -> eyre::Result<Vec<Bytes>> {
        let address = Predeploys::L2_TO_L1_MESSAGE_PASSER;
        let proof: EIP1186AccountProofResponse = serde_json::from_value(
            self.proofs.call("eth_getProof", json!([address, [], format!("{number:#x}")]))?,
        )?;
        let account = TrieAccount {
            nonce: proof.nonce,
            balance: proof.balance,
            storage_root: proof.storage_hash,
            code_hash: proof.code_hash,
        };
        verify_proof(
            state_root,
            Nibbles::unpack(keccak256(address)),
            Some(alloy_rlp::encode(account)),
            &proof.account_proof,
        )
        .map_err(|error| eyre!("message passer proof at block {number}: {error}"))?;
        Ok(proof.account_proof)
    }

    /// Splits a raw RLP block into its header RLP and its EIP-2718 transaction encodings.
    ///
    /// In a block body a legacy transaction is an RLP list and a typed transaction is an RLP
    /// string whose payload is `type || payload`, so the exact transaction bytes are the list
    /// item itself or the string's payload.
    pub fn split_block(raw: &[u8]) -> eyre::Result<(Bytes, Vec<Bytes>)> {
        let mut buf = raw;
        let block = alloy_rlp::Header::decode(&mut buf)?;
        ensure!(block.list && block.payload_length <= buf.len(), "block is not an RLP list");
        let mut fields = &buf[..block.payload_length];

        let header_start = fields;
        let header = alloy_rlp::Header::decode(&mut fields)?;
        ensure!(header.list && header.payload_length <= fields.len(), "header is not an RLP list");
        let header_len = header_start.len() - fields.len() + header.payload_length;
        let raw_header = Bytes::copy_from_slice(&header_start[..header_len]);
        fields = &header_start[header_len..];

        let list = alloy_rlp::Header::decode(&mut fields)?;
        ensure!(
            list.list && list.payload_length <= fields.len(),
            "transactions are not an RLP list"
        );
        let mut items = &fields[..list.payload_length];
        let mut transactions = Vec::new();
        while !items.is_empty() {
            let item_start = items;
            let item = alloy_rlp::Header::decode(&mut items)?;
            ensure!(item.payload_length <= items.len(), "truncated transaction");
            let tx = if item.list {
                &item_start[..item_start.len() - items.len() + item.payload_length]
            } else {
                &items[..item.payload_length]
            };
            transactions.push(Bytes::copy_from_slice(tx));
            items = &items[item.payload_length..];
        }
        Ok((raw_header, transactions))
    }

    /// Orders witness headers newest first and keeps the newest [`Self::MAX_ANCESTORS`] that
    /// form a contiguous run ending at block `number - 1`.
    pub fn order_ancestors(headers: Vec<Bytes>, number: u64) -> eyre::Result<Vec<Bytes>> {
        let mut decoded = headers
            .into_iter()
            .map(|raw| Ok((Header::decode(&mut raw.as_ref())?.number, raw)))
            .collect::<eyre::Result<Vec<_>>>()
            .wrap_err("decoding witness headers")?;
        decoded.sort_by_key(|(n, _)| Reverse(*n));
        decoded.dedup_by_key(|(n, _)| *n);
        ensure!(
            decoded.first().is_some_and(|(n, _)| *n + 1 == number),
            "witness headers do not include the parent of block {number}"
        );
        Ok(decoded.into_iter().take(Self::MAX_ANCESTORS).map(|(_, raw)| raw).collect())
    }

    /// Writes `request` to `path` atomically (temporary file, then rename).
    pub fn write_cache(path: &Path, request: &Request) -> eyre::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.tmp-{}", process::id()));
        fs::write(&tmp, serde_json::to_vec(request)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
