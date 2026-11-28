//! `tiger diff-adapter`: the differential fuzz protocol (`etc/tools/diff-fuzz/PROTOCOL.md`).
//!
//! Build and validate run the node's own block execution, rules and header assembly over a
//! stateless witness of the parent state.

mod protocol;

use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    slice,
    sync::Arc,
};

use alloy_consensus::{EMPTY_OMMER_ROOT_HASH, Header};
use alloy_primitives::{Address, B64, B256, Bytes, U256};
use base_common_consensus::{BaseReceiptEnvelope, Predeploys};
use base_common_genesis::{BaseFeature, SystemConfig};
use base_tiger_evm::{
    BlockEnvInput, BlockRules, ChainSpec,
};
use evm2::{
    BlockStateAccumulator,
};
use protocol::{
    ConfigLogOutcome, ConfigLogResult, HeaderResults, Log, Mode, PostAccount, Receipt, Rejected,
    Request, Response, SystemConfigOutput, SystemConfigValues,
};

use crate::{
    exec::BlockExecution,
    witness::{WitnessNodes, WitnessState},
};

/// The only chain the protocol defines today.
const BASE_MAINNET: &str = "base-mainnet";

/// Sets the Everest activation timestamp of the adapter's Base mainnet chain.
const EVEREST_TIMESTAMP_ENV: &str = "BASE_DIFF_EVEREST_TIMESTAMP";

/// Base mainnet's L1 system config contract, which emits `ConfigUpdate` logs.
const L1_SYSTEM_CONFIG: Address =
    alloy_primitives::address!("73a79fab69143498ed3712e519a88a918e1f4072");

/// What a build or validate run computed.
#[derive(Debug, Default)]
struct Outcome {
    invalid_reason: Option<String>,
    rejected: Vec<Rejected>,
    header: Option<HeaderResults>,
    receipts: Vec<Receipt>,
    post: BTreeMap<Address, Option<PostAccount>>,
}

impl Outcome {
    fn invalid(reason: String) -> Self {
        Self { invalid_reason: Some(reason), ..Self::default() }
    }
}

/// The adapter: Base mainnet rules, answering one request per stdin line.
#[derive(Debug)]
pub struct Adapter {
    chain: ChainSpec,
    /// Whether build-mode responses carry `coverage` (`BASE_DIFF_COVERAGE=1`).
    coverage: bool,
}

impl Adapter {
    /// Runs the request loop until stdin closes.
    pub fn run() -> io::Result<()> {
        let coverage = std::env::var_os(protocol::COVERAGE_ENV).is_some_and(|value| value != "0");
        let mut chain = ChainSpec::base_mainnet();
        // Everest is not scheduled on mainnet yet; fuzz runs place it with this variable.
        if let Some(value) = std::env::var_os(EVEREST_TIMESTAMP_ENV) {
            let text = value.to_string_lossy();
            let timestamp = text.parse::<u64>().map_err(|error| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("{EVEREST_TIMESTAMP_ENV}={text}: {error}"))
            })?;
            chain.upgrades.base.everest = Some(timestamp);
        }
        let adapter = Self { chain, coverage };
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let response = adapter.handle_line(&line);
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        Ok(())
    }

    /// Answers one request line. Parse errors, adapter errors and caught panics become
    /// `ok: false` responses.
    pub fn handle_line(&self, line: &str) -> serde_json::Value {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => return Self::failure(String::new(), format!("invalid request: {error}")),
        };
        let id = value.get("id").and_then(|id| id.as_str()).unwrap_or_default().to_owned();
        if value.get("mode").and_then(|mode| mode.as_str()) == Some("trie") {
            return crate::handle_trie_request(&value);
        }
        let request = match serde_json::from_value::<Request>(value) {
            Ok(request) => request,
            Err(error) => return Self::failure(id, format!("invalid request: {error}")),
        };
        match catch_unwind(AssertUnwindSafe(|| self.execute(&request))) {
            Ok(Ok(response)) => serde_json::to_value(response).expect("responses serialize"),
            Ok(Err(error)) => Self::failure(id, error),
            Err(panic) => Self::failure(id, format!("panic: {}", crate::trie_mode::panic_message(&*panic))),
        }
    }

    fn failure(id: String, error: String) -> serde_json::Value {
        serde_json::json!({ "id": id, "ok": false, "error": error, "valid": false })
    }

    fn execute(&self, request: &Request) -> Result<Response, String> {
        if request.chain != BASE_MAINNET {
            return Err(format!("unsupported chain {:?}", request.chain));
        }
        if request.mode == Mode::SystemConfig {
            return self.system_config(request);
        }

        let mut ancestors = Vec::with_capacity(request.ancestors.len());
        for (index, raw) in request.ancestors.iter().enumerate() {
            let header: Header = alloy_rlp::decode_exact(raw)
                .map_err(|error| format!("ancestors[{index}] is not an RLP header: {error}"))?;
            let hash = header.hash_slow();
            ancestors.push((header, hash));
        }
        let (parent, parent_hash) = ancestors.first().cloned().ok_or("request has no ancestors")?;
        for pair in ancestors.windows(2) {
            if pair[0].0.parent_hash != pair[1].1 || pair[0].0.number != pair[1].0.number + 1 {
                return Err(format!(
                    "ancestor {} is not the parent of ancestor {}",
                    pair[1].0.number, pair[0].0.number
                ));
            }
        }
        if request.header.number != parent.number + 1 {
            return Err(format!(
                "block number {} does not follow parent {}",
                request.header.number, parent.number
            ));
        }

        // Before a genesis block (zero parent hash) every block hash is zero.
        let (oldest, _) = ancestors.last().expect("the parent is an ancestor");
        let before_genesis = oldest
            .parent_hash
            .is_zero()
            .then(|| oldest.number.saturating_sub(256)..oldest.number)
            .into_iter()
            .flatten()
            .map(|number| (number, B256::ZERO));
        // A synthetic pre-state (`witness.root`) is a complete trie: read the message passer from
        // it rather than seeding its root from the parent header.
        let synthetic_root = request.witness.root.filter(|_| request.mode == Mode::Build);
        let message_passer_root = parent
            .withdrawals_root
            .filter(|_| synthetic_root.is_none())
            .filter(|_| self.chain.is_active(BaseFeature::L2ToL1MessageRoot, parent.timestamp))
            .map(|root| (Predeploys::L2_TO_L1_MESSAGE_PASSER, root));
        let state = Rc::new(WitnessState::new(
            synthetic_root.unwrap_or(parent.state_root),
            Arc::new(WitnessNodes::new(&request.witness.state, &request.witness.codes)),
            ancestors.iter().map(|(header, hash)| (header.number, *hash)).chain(before_genesis),
            message_passer_root,
        ));

        let rules = BlockRules { chain: &self.chain };
        let computed_base_fee =
            rules.next_base_fee(&parent, request.header.timestamp).map(U256::from);
        let input = &request.header;
        let env = BlockEnvInput {
            number: input.number,
            timestamp: input.timestamp,
            coinbase: input.coinbase,
            prev_randao: input.prev_randao,
            gas_limit: input.gas_limit,
            base_fee: input.base_fee_per_gas,
            parent_hash,
            parent_beacon_block_root: input.parent_beacon_block_root,
        };
        #[cfg(feature = "edge-coverage")]
        crate::EdgeCounters::reset();
        let outcome = match request.mode {
            Mode::Build => self.build(request, &env, &state),
            Mode::Validate => self.validate(request, &env, &parent, &state),
            Mode::SystemConfig => unreachable!("answered above"),
        };
        if let Some(missing) = state.missing() {
            return Err(missing.to_string());
        }
        let outcome = outcome?;
        let coverage = (self.coverage && request.mode == Mode::Build).then(Self::coverage);
        Ok(Response {
            id: request.id.clone(),
            ok: true,
            error: None,
            valid: outcome.invalid_reason.is_none(),
            invalid_reason: outcome.invalid_reason,
            rejected: outcome.rejected,
            header: outcome.header,
            computed_base_fee_per_gas: computed_base_fee,
            receipts: outcome.receipts,
            post: outcome.post,
            coverage,
            system_config: None,
        })
    }

    /// Edge features hit since the request started (empty without the `edge-coverage` build).
    fn coverage() -> BTreeMap<String, u64> {
        #[cfg(feature = "edge-coverage")]
        return crate::EdgeCounters::with_edges(BTreeMap::new());
        #[cfg(not(feature = "edge-coverage"))]
        BTreeMap::new()
    }

    /// Builds a block from the template as the payload builder does: transactions that fail
    /// validity checks are skipped.
    fn build(
        &self,
        request: &Request,
        env: &BlockEnvInput,
        state: &Rc<WitnessState>,
    ) -> Result<Outcome, String> {
        let input = &request.header;
        let built = BlockExecution { chain: &self.chain }.build(
            env,
            input.extra_data.clone(),
            input.excess_blob_gas,
            &request.transactions,
            true,
            state,
        )?;
        let Some(executed) = built.executed else {
            return Ok(Outcome::invalid(built.invalid.unwrap_or_default()));
        };
        Ok(Outcome {
            invalid_reason: None,
            rejected: built.rejected.into_iter().map(|(index, error)| Rejected { index, error }).collect(),
            header: Some(Self::header_results(&executed.header)),
            receipts: executed.receipts.iter().zip(&executed.outputs).map(Self::receipt).collect(),
            post: Self::post_state(&executed.changes, state)?,
        })
    }

    /// Imports the canonical block on the node's validation path.
    fn validate(
        &self,
        request: &Request,
        env: &BlockEnvInput,
        parent: &Header,
        state: &Rc<WitnessState>,
    ) -> Result<Outcome, String> {
        let input = &request.header;
        let canonical = input.results.as_ref().ok_or("validate request without header results")?;
        let header = Header {
            parent_hash: env.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: input.coinbase,
            state_root: canonical.state_root,
            transactions_root: canonical.transactions_root,
            receipts_root: canonical.receipts_root,
            logs_bloom: canonical.logs_bloom,
            difficulty: U256::ZERO,
            number: input.number,
            gas_limit: input.gas_limit,
            gas_used: canonical.gas_used,
            timestamp: input.timestamp,
            extra_data: input.extra_data.clone(),
            mix_hash: input.prev_randao,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(input.base_fee_per_gas),
            withdrawals_root: canonical.withdrawals_root,
            blob_gas_used: canonical.blob_gas_used,
            excess_blob_gas: input.excess_blob_gas,
            parent_beacon_block_root: input.parent_beacon_block_root,
            requests_hash: canonical.requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        };
        let hash = header.hash_slow();
        let hash_invalid = (hash != canonical.hash)
            .then(|| format!("payload: block hash {hash} is not {}", canonical.hash));
        let validated =
            BlockExecution { chain: &self.chain }.validate(&header, &request.transactions, parent, state)?;
        let invalid_reason = hash_invalid.or(validated.invalid);
        let Some(executed) = validated.executed else {
            return Ok(Outcome { invalid_reason, ..Outcome::default() });
        };
        Ok(Outcome {
            invalid_reason,
            rejected: Vec::new(),
            header: Some(Self::header_results(&executed.header)),
            receipts: executed.receipts.iter().zip(&executed.outputs).map(Self::receipt).collect(),
            post: Self::post_state(&executed.changes, state)?,
        })
    }

    fn header_results(header: &Header) -> HeaderResults {
        HeaderResults {
            state_root: header.state_root,
            receipts_root: header.receipts_root,
            transactions_root: header.transactions_root,
            logs_bloom: header.logs_bloom,
            gas_used: header.gas_used,
            blob_gas_used: header.blob_gas_used,
            withdrawals_root: header.withdrawals_root,
            requests_hash: header.requests_hash,
            hash: header.hash_slow(),
        }
    }

    fn receipt((receipt, output): (&BaseReceiptEnvelope, &Bytes)) -> Receipt {
        Receipt {
            tx_type: receipt.tx_type() as u8,
            status: u8::from(receipt.status()),
            cumulative_gas_used: receipt.cumulative_gas_used(),
            logs: receipt
                .logs()
                .iter()
                .map(|log| Log {
                    address: log.address,
                    topics: log.topics().to_vec(),
                    data: log.data.data.clone(),
                })
                .collect(),
            deposit_nonce: receipt.deposit_nonce(),
            deposit_receipt_version: receipt.deposit_receipt_version(),
            output: Some(output.clone()),
        }
    }

    /// The protocol `post` diff of a block's state changes.
    fn post_state(
        changes: &BlockStateAccumulator,
        state: &WitnessState,
    ) -> Result<BTreeMap<Address, Option<PostAccount>>, String> {
        let mut storage: BTreeMap<Address, BTreeMap<B256, B256>> = BTreeMap::new();
        for (key, slot) in changes.storage() {
            storage
                .entry(key.address())
                .or_default()
                .insert(B256::from(key.key()), B256::from(slot.current));
        }
        let wiped: std::collections::BTreeSet<Address> = changes.storage_wipes().collect();
        let mut post = BTreeMap::new();
        let mut addresses: std::collections::BTreeSet<Address> =
            changes.accounts().map(|(address, _)| address).collect();
        addresses.extend(storage.keys().copied());
        addresses.extend(wiped.iter().copied());
        let accounts: BTreeMap<Address, _> = changes.accounts().collect();
        for address in addresses {
            let (before_exists, after) = match accounts.get(&address) {
                Some(tracked) => (
                    tracked.original.is_some(),
                    tracked.current.as_ref().map(|info| (info.nonce, info.balance, info.code_hash)),
                ),
                None => {
                    let parent = state.account(&address).map_err(|error| error.to_string())?;
                    let parent = parent.map(|account| (account.nonce, account.balance, account.code_hash));
                    (parent.is_some(), parent)
                }
            };
            match after {
                None if before_exists => {
                    post.insert(address, None);
                }
                None => {}
                Some((nonce, balance, code_hash)) => {
                    post.insert(
                        address,
                        Some(PostAccount {
                            nonce,
                            balance,
                            code_hash,
                            storage: storage.remove(&address).unwrap_or_default(),
                            wiped: before_exists && wiped.contains(&address),
                        }),
                    );
                }
            }
        }
        Ok(post)
    }

    /// Applies the request's receipts with base/base's `SystemConfig::update_with_receipts`, one
    /// log at a time to learn each log's outcome, then all at once; both must agree.
    fn system_config(&self, request: &Request) -> Result<Response, String> {
        let input = request.system_config.as_ref().ok_or("systemConfig request has no systemConfig")?;
        for (index, receipt) in input.receipts.iter().enumerate() {
            if receipt.status > 1 {
                return Err(format!("receipts[{index}] has status {}", receipt.status));
            }
        }
        let ecotone = self.chain.is_active(BaseFeature::BlobAwareL1Fees, input.l1_timestamp);
        let receipts: Vec<alloy_consensus::Receipt> = input.receipts.iter().map(Into::into).collect();
        let mut config = SystemConfig::from(&input.config);
        let mut logs = Vec::new();
        for receipt in &receipts {
            for log in &receipt.logs {
                let single = alloy_consensus::Receipt {
                    status: receipt.status,
                    cumulative_gas_used: 0,
                    logs: vec![log.clone()],
                };
                let (applied, errors) =
                    config.update_with_receipts(slice::from_ref(&single), L1_SYSTEM_CONFIG, ecotone);
                logs.push(match (applied.as_slice(), errors.as_slice()) {
                    ([kind], []) => ConfigLogResult {
                        outcome: ConfigLogOutcome::Applied,
                        kind: Some(*kind as u64),
                        error: None,
                    },
                    ([], [error]) => ConfigLogResult {
                        outcome: ConfigLogOutcome::Rejected,
                        kind: None,
                        error: Some(error.to_string()),
                    },
                    ([], []) => ConfigLogResult { outcome: ConfigLogOutcome::Ignored, kind: None, error: None },
                    _ => return Err(format!("one log gave updates {applied:?} and errors {errors:?}")),
                });
            }
        }
        let mut whole = SystemConfig::from(&input.config);
        whole.update_with_receipts(&receipts, L1_SYSTEM_CONFIG, ecotone);
        if whole != config {
            return Err(format!("one log at a time gave {config:?}, all at once {whole:?}"));
        }
        Ok(Response {
            id: request.id.clone(),
            ok: true,
            error: None,
            valid: true,
            invalid_reason: None,
            rejected: Vec::new(),
            header: None,
            computed_base_fee_per_gas: None,
            receipts: Vec::new(),
            post: BTreeMap::new(),
            coverage: None,
            system_config: Some(SystemConfigOutput {
                config: Some(SystemConfigValues::from(&config)),
                logs,
            }),
        })
    }
}
