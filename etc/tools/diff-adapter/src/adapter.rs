//! Runs protocol requests on the node's block import (validate) and block building (build) paths.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufWriter, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::Arc,
};

use alloy_consensus::{
    Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, TxReceipt, Typed2718,
    transaction::{Recovered, SignerRecoverable},
};
use alloy_eips::eip2718::WithEncoded;
use alloy_primitives::{Address, B64, B256, Bytes, U256};
use base_common_chains::{BaseFeature, Upgrades};
use base_common_consensus::{
    BasePrimitives, BaseReceipt, BaseTxEnvelope, BaseTxReceipt, Predeploys, decode_2718_canonical,
};
use base_common_rpc_types_engine::ExecutionData;
use base_diff_fuzz::{
    COVERAGE_ENV, Era, HeaderResults, Log, Mode, PostAccount, Receipt, Rejected, Request, Response,
};
use base_execution_chainspec::BaseChainSpec;
use base_execution_consensus::BaseBeaconConsensus;
use base_execution_evm::{BaseEvmConfig, BaseEvmEnvBuilder, BaseNextBlockEnvAttributes};
use base_node_core::engine::{BaseEngineTypes, BaseEngineValidator};
use eyre::{Context, bail, ensure, eyre};
use reth_chainspec::EthChainSpec;
use reth_consensus::{Consensus, FullConsensus, HeaderValidator};
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, Evm,
    block::{BlockExecutionError, BlockExecutor},
    execute::{BlockAssembler, BlockAssemblerInput, BlockBuilder},
};
use reth_execution_types::{BlockExecutionOutput, BlockExecutionResult};
use reth_node_api::PayloadValidator;
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_revm::{
    State,
    database::StateProviderDatabase,
    db::{BundleState, states::bundle_state::BundleRetention},
};
use reth_storage_api::{
    HashedPostStateProvider, StateProvider, StateProviderBox, StateRootProvider,
};
use revm::inspector::inspectors::TracerEip3155;
use tracing::warn;

use crate::{Coverage, WitnessState};

/// The only chain the protocol defines today.
pub const BASE_MAINNET: &str = "base-mainnet";

/// The node's name for Base mainnet, as given to `base-reth-node --chain`.
pub const BASE_MAINNET_NODE_CHAIN: &str = "base";

/// The base/base differential execution adapter.
///
/// Holds the same chain spec, EVM config, consensus, and engine validator the node builds for
/// `--chain base`, and runs each request against a [`WitnessState`] of the parent block.
#[derive(Debug)]
pub struct DiffAdapter {
    /// Base mainnet chain spec.
    pub chain_spec: Arc<BaseChainSpec>,
    /// The node's EVM config.
    pub evm_config: BaseEvmConfig,
    /// The node's consensus rules.
    pub consensus: BaseBeaconConsensus,
    /// The node's engine API payload validator.
    pub engine_validator: BaseEngineValidator<BaseTxEnvelope, BaseChainSpec>,
    /// Whether build-mode responses carry coverage features (see [`Coverage`]).
    pub coverage: bool,
}

impl DiffAdapter {
    /// Creates an adapter for Base mainnet.
    pub fn base_mainnet() -> Self {
        let chain_spec = BaseChainSpec::parse_chain(BASE_MAINNET_NODE_CHAIN)
            .expect("base mainnet is a built-in chain");
        Self {
            evm_config: BaseEvmConfig::base(Arc::clone(&chain_spec)),
            consensus: BaseBeaconConsensus::new(Arc::clone(&chain_spec)),
            engine_validator: BaseEngineValidator::new::<reth_trie_common::KeccakKeyHasher>(
                Arc::clone(&chain_spec),
            ),
            chain_spec,
            coverage: std::env::var_os(COVERAGE_ENV).is_some_and(|value| value != "0"),
        }
    }

    /// Answers one request line. Never panics: parse errors, adapter errors, and caught panics
    /// become `ok: false` responses.
    pub fn handle_line(&self, line: &str) -> Response {
        let request = match serde_json::from_str::<Request>(line) {
            Ok(request) => request,
            Err(error) => {
                let id = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| value.get("id")?.as_str().map(str::to_owned))
                    .unwrap_or_default();
                return Self::failure(id, format!("invalid request: {error}"));
            }
        };
        match catch_unwind(AssertUnwindSafe(|| self.execute(&request))) {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => Self::failure(request.id, format!("{error:#}")),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic payload".to_owned());
                Self::failure(request.id, format!("panic: {message}"))
            }
        }
    }

    /// An adapter-level failure response.
    pub const fn failure(id: String, error: String) -> Response {
        Response {
            id,
            ok: false,
            error: Some(error),
            valid: false,
            invalid_reason: None,
            rejected: Vec::new(),
            header: None,
            computed_base_fee_per_gas: None,
            receipts: Vec::new(),
            post: BTreeMap::new(),
            coverage: None,
        }
    }

    /// Executes a parsed request.
    pub fn execute(&self, request: &Request) -> eyre::Result<Response> {
        ensure!(request.chain == BASE_MAINNET, "unsupported chain {:?}", request.chain);

        let ancestors = request
            .ancestors
            .iter()
            .enumerate()
            .map(|(index, raw)| {
                let header: Header = alloy_rlp::decode_exact(raw)
                    .wrap_err_with(|| format!("ancestors[{index}] is not an RLP header"))?;
                Ok(SealedHeader::seal_slow(header))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let parent = ancestors.first().cloned().ok_or_else(|| eyre!("request has no ancestors"))?;
        for pair in ancestors.windows(2) {
            ensure!(
                pair[0].parent_hash == pair[1].hash() && pair[0].number == pair[1].number + 1,
                "ancestor {} is not the parent of ancestor {}",
                pair[1].number,
                pair[0].number
            );
        }
        ensure!(
            request.header.number == parent.number + 1,
            "block number {} does not follow parent {}",
            request.header.number,
            parent.number
        );

        let state = WitnessState::new(
            parent.state_root,
            &request.witness.state,
            &request.witness.codes,
            ancestors.iter().map(|header| (header.number, header.hash())),
            parent
                .withdrawals_root
                .filter(|_| {
                    self.chain_spec.is_feature_active_at_timestamp(
                        BaseFeature::L2ToL1MessageRoot,
                        parent.timestamp,
                    )
                })
                .map(|root| (Predeploys::L2_TO_L1_MESSAGE_PASSER, root)),
        );
        let computed_base_fee =
            self.chain_spec.next_block_base_fee(parent.header(), request.header.timestamp);

        let mut outcome = match request.mode {
            Mode::Build => self.build(request, &parent, &state)?,
            Mode::Validate => self.validate(request, &parent, &state)?,
        };
        if let Some(missing) = state.missing() {
            bail!(missing);
        }
        if let Some(coverage) = outcome.coverage.as_mut() {
            Self::transaction_coverage(request, &outcome.rejected, &outcome.receipts, coverage);
        }

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
            coverage: outcome.coverage,
        })
    }

    /// Builds a block from the request template the way the payload builder does: the env comes
    /// from the parent and the attributes (with the request's base fee), transactions run through
    /// reth's block builder, and transactions failing validity checks are skipped.
    pub fn build(
        &self,
        request: &Request,
        parent: &SealedHeader<Header>,
        state: &WitnessState,
    ) -> eyre::Result<Outcome> {
        let input = &request.header;
        let attributes = BaseNextBlockEnvAttributes {
            timestamp: input.timestamp,
            suggested_fee_recipient: input.coinbase,
            prev_randao: input.prev_randao,
            gas_limit: input.gas_limit,
            parent_beacon_block_root: input.parent_beacon_block_root,
            extra_data: input.extra_data.clone(),
        };
        let evm_env = BaseEvmEnvBuilder::next_evm_env(
            parent.header(),
            &attributes,
            input.base_fee_per_gas,
            self.chain_spec.as_ref(),
        );
        let ctx = self.evm_config.context_for_next_block(parent, attributes)?;

        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(state.clone().into_evm_state_provider()))
            .with_bundle_update()
            .build();
        // The payload builder loads the L1 block contract before building.
        if let Err(error) = db.load_cache_account(Predeploys::L1_BLOCK_INFO) {
            return Outcome::invalid(state, format!("loading L1 block info: {error}"));
        }

        let mut rejected = Vec::new();
        let mut coverage = None;
        let built = match &request.trace {
            None if self.coverage => {
                #[cfg(feature = "edge-coverage")]
                crate::EdgeCounters::reset();
                let fork = format!("{:?}", Era::at(input.timestamp));
                let evm = self.evm_config.evm_with_env_and_inspector(
                    &mut db,
                    evm_env,
                    Coverage::new(fork),
                );
                let mut builder = self.evm_config.create_block_builder(evm, parent, ctx);
                let filled = Self::fill_template(
                    &mut builder,
                    request,
                    state,
                    &mut rejected,
                    |builder, index| {
                        let tx_type = Self::tx_type(&request.transactions[index]);
                        builder.evm_mut().inspector_mut().begin_tx(tx_type);
                    },
                )?;
                let features = std::mem::take(builder.evm_mut().inspector_mut()).features();
                #[cfg(feature = "edge-coverage")]
                let features = crate::EdgeCounters::with_edges(features);
                coverage = Some(features);
                filled.map(|()| builder.finish(state, None))
            }
            None => {
                let evm = self.evm_config.evm_with_env(&mut db, evm_env);
                let mut builder = self.evm_config.create_block_builder(evm, parent, ctx);
                Self::fill_template(&mut builder, request, state, &mut rejected, |_, _| {})?
                    .map(|()| builder.finish(state, None))
            }
            Some(dir) => {
                let tracer = TracerEip3155::new(Box::new(io::sink()));
                let evm = self.evm_config.evm_with_env_and_inspector(&mut db, evm_env, tracer);
                let mut builder = self.evm_config.create_block_builder(evm, parent, ctx);
                Self::fill_template(
                    &mut builder,
                    request,
                    state,
                    &mut rejected,
                    |builder, index| {
                        let writer = Self::trace_writer(dir, &request.id, index);
                        builder.evm_mut().inspector_mut().set_writer(writer);
                    },
                )?
                .map(|()| builder.finish(state, None))
            }
        };
        let built = match built {
            Ok(Ok(built)) => built,
            Ok(Err(error)) => {
                return Outcome::invalid(state, format!("finishing block: {error}"));
            }
            Err(reason) => return Outcome::invalid(state, reason),
        };
        let bundle = db.take_bundle();

        // The header is the request inputs plus computed fields. The assembler writes Base's
        // constant zero excess blob gas; a given input takes its place so the hash covers it.
        let mut header = built.block.header().clone();
        if header.excess_blob_gas.is_some() && input.excess_blob_gas.is_some() {
            header.excess_blob_gas = input.excess_blob_gas;
        }

        Ok(Outcome {
            invalid_reason: None,
            rejected,
            header: Some(Self::header_results(&SealedHeader::seal_slow(header))),
            receipts: built.execution_result.receipts.iter().map(Self::receipt).collect(),
            post: Self::post_state(&bundle),
            coverage,
        })
    }

    /// Imports the canonical block the way the engine tree validates a new payload: payload to
    /// block conversion, header and pre-execution consensus checks, execution with reth's block
    /// executor, post-execution consensus checks, the engine validator's hashed-state checks, and
    /// the state root check.
    pub fn validate(
        &self,
        request: &Request,
        parent: &SealedHeader<Header>,
        state: &WitnessState,
    ) -> eyre::Result<Outcome> {
        let input = &request.header;
        let canonical = input
            .results
            .as_ref()
            .ok_or_else(|| eyre!("validate request without canonical header results"))?;

        let mut transactions = Vec::with_capacity(request.transactions.len());
        let mut senders = Vec::with_capacity(request.transactions.len());
        for (index, raw) in request.transactions.iter().enumerate() {
            match Self::decode_transaction(raw) {
                Ok(tx) => {
                    let (tx, sender) = tx.into_parts();
                    transactions.push(tx);
                    senders.push(sender);
                }
                Err(error) => {
                    return Outcome::invalid(state, format!("transaction {index}: {error}"));
                }
            }
        }

        let header = Header {
            parent_hash: parent.hash(),
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
        let withdrawals = self
            .chain_spec
            .is_feature_active_at_timestamp(BaseFeature::Withdrawals, input.timestamp)
            .then(Default::default);
        let block = Block::new(
            header,
            BlockBody { transactions: transactions.clone(), ommers: Vec::new(), withdrawals },
        );
        let payload = ExecutionData::from_block_unchecked(canonical.hash, &block);

        // The first validation failure decides validity; execution continues so that every
        // computed field is still reported.
        let mut invalid_reason = None;

        let sealed = match PayloadValidator::<BaseEngineTypes>::convert_payload_to_block(
            &self.engine_validator,
            payload.clone(),
        ) {
            Ok(sealed) => sealed,
            Err(error) => {
                Self::note(&mut invalid_reason, "payload", &error);
                SealedBlock::seal_slow(block)
            }
        };
        if let Err(error) = self.consensus.validate_header(sealed.sealed_header()) {
            Self::note(&mut invalid_reason, "header", &error);
        }
        if let Err(error) =
            self.consensus.validate_header_against_parent(sealed.sealed_header(), parent)
        {
            Self::note(&mut invalid_reason, "header against parent", &error);
        }
        if let Err(error) = self.consensus.validate_block_pre_execution_with_tx_root(&sealed, None)
        {
            Self::note(&mut invalid_reason, "pre-execution", &error);
        }
        let recovered = sealed.with_senders(senders.clone());

        let evm_env = self.evm_config.evm_env_for_payload(&payload)?;
        let ctx = self.evm_config.context_for_payload(&payload)?;
        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(state.clone().into_evm_state_provider()))
            .with_bundle_update()
            .build();
        let txs = request.transactions.iter().zip(&transactions).zip(&senders).map(
            |((raw, tx), sender)| {
                WithEncoded::new(raw.clone(), Recovered::new_unchecked(tx.clone(), *sender))
            },
        );
        let executed = match &request.trace {
            None => {
                let evm = self.evm_config.evm_with_env(&mut db, evm_env.clone());
                let executor = self.evm_config.create_executor(evm, ctx.clone());
                Self::run_block(executor, txs, |_, _| {})
            }
            Some(dir) => {
                let tracer = TracerEip3155::new(Box::new(io::sink()));
                let evm =
                    self.evm_config.evm_with_env_and_inspector(&mut db, evm_env.clone(), tracer);
                let executor = self.evm_config.create_executor(evm, ctx.clone());
                Self::run_block(executor, txs, |executor, index| {
                    let writer = Self::trace_writer(dir, &request.id, index);
                    executor.evm_mut().inspector_mut().set_writer(writer);
                })
            }
        };
        let result = match executed {
            Ok(result) => result,
            Err(error) => {
                Self::note(&mut invalid_reason, "execution", &error);
                return Outcome::invalid(state, invalid_reason.unwrap_or_default());
            }
        };
        db.merge_transitions(BundleRetention::Reverts);
        let output = BlockExecutionOutput { result, state: db.take_bundle() };

        if let Err(error) = FullConsensus::<BasePrimitives>::validate_block_post_execution(
            &self.consensus,
            &recovered,
            &output.result,
            None,
            None,
        ) {
            Self::note(&mut invalid_reason, "post-execution", &error);
        }
        let hashed_state = state.hashed_post_state(&output.state)?;
        if let Err(error) =
            PayloadValidator::<BaseEngineTypes>::validate_block_post_execution_with_hashed_state(
                &self.engine_validator,
                || &hashed_state,
                &recovered,
                parent,
                || Ok(Box::new(state.clone()) as StateProviderBox),
            )
        {
            Self::note(&mut invalid_reason, "post-execution hashed state", &error);
        }
        let state_root = state.state_root(hashed_state)?;
        if state_root != recovered.state_root {
            Self::note(
                &mut invalid_reason,
                "state root",
                &format!("computed {state_root}, header {}", recovered.state_root),
            );
        }

        let assembled = BlockAssembler::assemble_block(
            self.evm_config.block_assembler(),
            BlockAssemblerInput::<<BaseEvmConfig as ConfigureEvm>::BlockExecutorFactory>::new(
                evm_env,
                ctx,
                parent,
                transactions,
                &output.result,
                &output.state,
                state as &dyn StateProvider,
                state_root,
                None,
            ),
        )?;
        let assembled = SealedHeader::seal_slow(assembled.header);

        Ok(Outcome {
            invalid_reason,
            rejected: Vec::new(),
            header: Some(Self::header_results(&assembled)),
            receipts: output.result.receipts.iter().map(Self::receipt).collect(),
            post: Self::post_state(&output.state),
            coverage: None,
        })
    }

    /// Runs the build-mode template through `builder`: pre-execution changes, then every
    /// transaction, skipping the ones that fail validity checks. `before_tx` runs before each
    /// executed transaction. Returns `Err(reason)` when the block cannot be executed at all.
    pub fn fill_template<B: BlockBuilder<Primitives = BasePrimitives>>(
        builder: &mut B,
        request: &Request,
        state: &WitnessState,
        rejected: &mut Vec<Rejected>,
        mut before_tx: impl FnMut(&mut B, usize),
    ) -> eyre::Result<Result<(), String>> {
        if let Err(error) = builder.apply_pre_execution_changes() {
            if let Some(missing) = state.missing() {
                bail!(missing);
            }
            return Ok(Err(format!("pre-execution changes: {error}")));
        }
        for (index, raw) in request.transactions.iter().enumerate() {
            let tx = match Self::decode_transaction(raw) {
                Ok(tx) => tx,
                Err(error) => {
                    rejected.push(Rejected { index, error });
                    continue;
                }
            };
            before_tx(builder, index);
            if let Err(error) = builder.execute_transaction(WithEncoded::new(raw.clone(), tx)) {
                if let Some(missing) = state.missing() {
                    bail!(missing);
                }
                match error {
                    BlockExecutionError::Validation(error) => {
                        rejected.push(Rejected { index, error: error.to_string() });
                    }
                    error => return Ok(Err(format!("transaction {index}: {error}"))),
                }
            }
        }
        Ok(Ok(()))
    }

    /// Executes a canonical block with `executor` the way the engine tree does. `before_tx` runs
    /// before each transaction.
    pub fn run_block<E: BlockExecutor<Transaction = BaseTxEnvelope>>(
        mut executor: E,
        transactions: impl Iterator<Item = WithEncoded<Recovered<BaseTxEnvelope>>>,
        mut before_tx: impl FnMut(&mut E, usize),
    ) -> Result<BlockExecutionResult<E::Receipt>, BlockExecutionError> {
        executor.apply_pre_execution_changes()?;
        for (index, tx) in transactions.enumerate() {
            before_tx(&mut executor, index);
            executor.execute_transaction(tx)?;
        }
        executor.finish().map(|(_, result)| result)
    }

    /// Opens the EIP-3155 trace file of transaction `index`, or discards the trace if the file
    /// cannot be created.
    pub fn trace_writer(dir: &str, id: &str, index: usize) -> Box<dyn Write> {
        let path = Path::new(dir).join(format!("{id}-{index}.jsonl"));
        match File::create(&path) {
            Ok(file) => Box::new(BufWriter::new(file)),
            Err(error) => {
                warn!(path = %path.display(), error = %error, "cannot create trace file");
                Box::new(io::sink())
            }
        }
    }

    /// EIP-2718 type of an encoded transaction (0 for legacy).
    pub fn tx_type(raw: &Bytes) -> u8 {
        raw.first().copied().filter(|byte| *byte < 0xc0).unwrap_or(0)
    }

    /// Adds transaction-level features to a block's coverage: each included transaction's type
    /// and status, and each rejected transaction's type and error kind (numbers elided).
    pub fn transaction_coverage(
        request: &Request,
        rejected: &[Rejected],
        receipts: &[Receipt],
        coverage: &mut BTreeMap<String, u64>,
    ) {
        for receipt in receipts {
            let feature = format!("tx:t{}:s{}", receipt.tx_type, receipt.status);
            *coverage.entry(feature).or_default() += 1;
        }
        for rejection in rejected {
            let tx_type = request.transactions.get(rejection.index).map_or(0, Self::tx_type);
            let kind: Vec<&str> = rejection
                .error
                .split_whitespace()
                .map(|word| if word.chars().any(|c| c.is_ascii_digit()) { "#" } else { word })
                .take(12)
                .collect();
            let feature = format!("reject:t{tx_type}:{}", kind.join(" "));
            *coverage.entry(feature).or_default() += 1;
        }
    }

    /// Keeps the first validation failure of a block.
    pub fn note(reason: &mut Option<String>, stage: &str, error: &dyn core::fmt::Display) {
        reason.get_or_insert_with(|| format!("{stage}: {error}"));
    }

    /// Decodes a block-body transaction the way the engine API does and recovers its signer.
    pub fn decode_transaction(raw: &Bytes) -> Result<Recovered<BaseTxEnvelope>, String> {
        let tx = decode_2718_canonical::<BaseTxEnvelope>(raw)
            .map_err(|error| format!("decoding: {error}"))?;
        tx.try_into_recovered().map_err(|error| format!("recovering signer: {error}"))
    }

    /// The protocol's computed header fields of `header`.
    pub fn header_results(header: &SealedHeader<Header>) -> HeaderResults {
        HeaderResults {
            state_root: header.state_root,
            receipts_root: header.receipts_root,
            transactions_root: header.transactions_root,
            logs_bloom: header.logs_bloom,
            gas_used: header.gas_used,
            blob_gas_used: header.blob_gas_used,
            withdrawals_root: header.withdrawals_root,
            requests_hash: header.requests_hash,
            hash: header.hash(),
        }
    }

    /// The protocol form of a receipt.
    pub fn receipt(receipt: &BaseReceipt) -> Receipt {
        Receipt {
            tx_type: receipt.ty(),
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
        }
    }

    /// The protocol `post` diff of a block's bundle state.
    ///
    /// The bundle holds, per touched account, its info at the start of the block and after it,
    /// and for every written slot its value at the start of the block and after it.
    pub fn post_state(bundle: &BundleState) -> BTreeMap<Address, Option<PostAccount>> {
        let mut post = BTreeMap::new();
        for (address, account) in bundle.state() {
            match (&account.original_info, &account.info) {
                (None, None) => {}
                (Some(_), None) => {
                    post.insert(*address, None);
                }
                (before, Some(after)) => {
                    let storage = account
                        .storage
                        .iter()
                        .filter(|(_, slot)| slot.present_value != slot.previous_or_original_value)
                        .map(|(key, slot)| (B256::from(*key), B256::from(slot.present_value)))
                        .collect::<BTreeMap<_, _>>();
                    let info_changed = before.as_ref().is_none_or(|before| {
                        before.nonce != after.nonce
                            || before.balance != after.balance
                            || before.code_hash != after.code_hash
                    });
                    if info_changed || !storage.is_empty() {
                        post.insert(
                            *address,
                            Some(PostAccount {
                                nonce: after.nonce,
                                balance: after.balance,
                                code_hash: after.code_hash,
                                storage,
                            }),
                        );
                    }
                }
            }
        }
        post
    }
}

/// What a build or validate run computed.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Why the block is invalid (validate) or cannot be built (build).
    pub invalid_reason: Option<String>,
    /// Skipped transactions (build mode).
    pub rejected: Vec<Rejected>,
    /// Computed header fields, absent when execution did not complete.
    pub header: Option<HeaderResults>,
    /// Receipts of included transactions.
    pub receipts: Vec<Receipt>,
    /// Post-state diff.
    pub post: BTreeMap<Address, Option<PostAccount>>,
    /// Coverage features, when coverage is on.
    pub coverage: Option<BTreeMap<String, u64>>,
}

impl Outcome {
    /// A block that could not be executed to completion. Fails the request instead when the
    /// failure came from a read the witness could not serve.
    pub fn invalid(state: &WitnessState, reason: String) -> eyre::Result<Self> {
        if let Some(missing) = state.missing() {
            bail!(missing);
        }
        Ok(Self { invalid_reason: Some(reason), ..Default::default() })
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::{eip2718::Encodable2718, eip7685::EMPTY_REQUESTS_HASH};
    use alloy_primitives::{TxKind, bytes, keccak256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use alloy_trie::{
        EMPTY_ROOT_HASH, HashBuilder, KECCAK_EMPTY, Nibbles, TrieAccount, proof::ProofRetainer,
    };
    use base_common_evm::BaseTime;
    use base_diff_fuzz::{RequestHeader, Witness};

    use super::*;

    /// One account of a synthetic world state.
    #[derive(Clone, Default)]
    struct Account {
        nonce: u64,
        balance: U256,
        code: Bytes,
        storage: BTreeMap<B256, U256>,
    }

    fn trie(leaves: &BTreeMap<B256, Vec<u8>>) -> (B256, Vec<Bytes>) {
        let targets = leaves.keys().map(Nibbles::unpack).collect();
        let mut builder = HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
        for (key, value) in leaves {
            builder.add_leaf(Nibbles::unpack(key), value);
        }
        let root = builder.root();
        (root, builder.take_proof_nodes().into_inner().into_values().collect())
    }

    /// The state root of `world` and a complete witness for it.
    fn witness(world: &BTreeMap<Address, Account>) -> (B256, Witness) {
        let mut witness = Witness::default();
        let mut leaves = BTreeMap::new();
        for (address, account) in world {
            let slots = account
                .storage
                .iter()
                .map(|(slot, value)| (keccak256(slot), alloy_rlp::encode(value)))
                .collect();
            let (storage_root, nodes) = trie(&slots);
            witness.state.extend(nodes);
            if !account.code.is_empty() {
                witness.codes.push(account.code.clone());
            }
            let code_hash =
                if account.code.is_empty() { KECCAK_EMPTY } else { keccak256(&account.code) };
            let leaf = TrieAccount::new(account.nonce, account.balance, storage_root, code_hash);
            leaves.insert(keccak256(address), alloy_rlp::encode(leaf));
        }
        let (root, nodes) = trie(&leaves);
        witness.state.extend(nodes);
        (root, witness)
    }

    fn apply(
        world: &mut BTreeMap<Address, Account>,
        post: &BTreeMap<Address, Option<PostAccount>>,
    ) {
        for (address, change) in post {
            let Some(change) = change else {
                world.remove(address);
                continue;
            };
            let account = world.entry(*address).or_default();
            account.nonce = change.nonce;
            account.balance = change.balance;
            let installed = BaseTime::implementation_bytecode();
            if change.code_hash == keccak256(&installed) {
                account.code = installed;
            } else if change.code_hash == KECCAK_EMPTY {
                account.code = Bytes::new();
            }
            assert_eq!(
                change.code_hash,
                if account.code.is_empty() { KECCAK_EMPTY } else { keccak256(&account.code) }
            );
            for (slot, value) in &change.storage {
                if value.is_zero() {
                    account.storage.remove(slot);
                } else {
                    account.storage.insert(*slot, U256::from_be_bytes(value.0));
                }
            }
        }
    }

    /// The header a protocol client assembles from the request inputs and computed fields.
    fn assemble(parent: &Header, input: &RequestHeader, results: &HeaderResults) -> Header {
        Header {
            parent_hash: parent.hash_slow(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: input.coinbase,
            state_root: results.state_root,
            transactions_root: results.transactions_root,
            receipts_root: results.receipts_root,
            logs_bloom: results.logs_bloom,
            difficulty: U256::ZERO,
            number: input.number,
            gas_limit: input.gas_limit,
            gas_used: results.gas_used,
            timestamp: input.timestamp,
            extra_data: input.extra_data.clone(),
            mix_hash: input.prev_randao,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(input.base_fee_per_gas),
            withdrawals_root: results.withdrawals_root,
            blob_gas_used: results.blob_gas_used,
            excess_blob_gas: input.excess_blob_gas,
            parent_beacon_block_root: input.parent_beacon_block_root,
            requests_hash: results.requests_hash,
            block_access_list_hash: None,
            slot_number: None,
        }
    }

    /// Builds a short synthetic chain under the mainnet config where every block's parent is the
    /// previous block's computed header and its witness is the previous block's post state.
    #[test]
    fn synthetic_chain_builds_on_its_own_post_state_and_header() {
        let adapter = DiffAdapter::base_mainnet();
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x42)).unwrap();
        let contract = Address::repeat_byte(0xc0);
        // CALLVALUE PUSH1 0 SSTORE PUSH1 0 PUSH1 1 SSTORE STOP: slot 0 = value, clear slot 1.
        let code = bytes!("34600055600060015500");
        let mut world = BTreeMap::from([
            (
                signer.address(),
                Account { balance: U256::from(10u128.pow(19)), ..Default::default() },
            ),
            (
                contract,
                Account {
                    code,
                    storage: BTreeMap::from([
                        (B256::ZERO, U256::from(7)),
                        (B256::with_last_byte(1), U256::from(5)),
                    ]),
                    ..Default::default()
                },
            ),
            // A BaseTime proxy awaiting its implementation, installed by the first Denim block.
            (
                Predeploys::BASE_TIME,
                Account {
                    code: bytes!("00"),
                    storage: BTreeMap::from([(
                        B256::from(BaseTime::ADMIN_SLOT),
                        U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
                    )]),
                    ..Default::default()
                },
            ),
        ]);
        let mut parent = Header {
            number: 58_365_650,
            timestamp: 1_794_863_505,
            gas_limit: 40_000_000,
            base_fee_per_gas: Some(5_000_000),
            extra_data: bytes!("01000003e80000000500000000004c4b40"),
            state_root: witness(&world).0,
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        };
        let mut ancestors = Vec::new();

        for nonce in 0..3u64 {
            ancestors.insert(0, Bytes::from(alloy_rlp::encode(&parent)));
            let tx = TxEip1559 {
                chain_id: 8453,
                nonce,
                gas_limit: 100_000,
                max_fee_per_gas: 10_000_000,
                max_priority_fee_per_gas: 1,
                to: TxKind::Call(contract),
                value: U256::from(nonce + 1),
                ..Default::default()
            };
            let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
            let raw = Bytes::from(BaseTxEnvelope::from(tx.into_signed(signature)).encoded_2718());
            let input = RequestHeader {
                number: parent.number + 1,
                timestamp: parent.timestamp + 1,
                coinbase: Predeploys::SEQUENCER_FEE_VAULT,
                prev_randao: B256::with_last_byte(nonce as u8),
                gas_limit: parent.gas_limit,
                base_fee_per_gas: 5_000_000,
                extra_data: parent.extra_data.clone(),
                parent_beacon_block_root: Some(B256::with_last_byte(9)),
                excess_blob_gas: Some(0),
                results: None,
            };
            let request = Request {
                id: format!("synthetic-{nonce}"),
                chain: BASE_MAINNET.to_owned(),
                mode: Mode::Build,
                ancestors: ancestors.clone(),
                header: input.clone(),
                // The duplicate reuses the nonce and must be skipped.
                transactions: vec![raw.clone(), raw],
                witness: witness(&world).1,
                trace: None,
            };

            let response = adapter.execute(&request).unwrap();
            assert!(response.ok && response.valid, "{response:?}");
            assert_eq!(response.rejected.iter().map(|r| r.index).collect::<Vec<_>>(), [1]);
            assert_eq!(response.receipts.len(), 1);
            assert_eq!(response.receipts[0].status, 1);

            let results = response.header.unwrap();
            apply(&mut world, &response.post);
            assert_eq!(
                witness(&world).0,
                results.state_root,
                "post diff must yield the state root"
            );
            assert_eq!(
                world[&contract].storage,
                BTreeMap::from([(B256::ZERO, U256::from(nonce + 1))])
            );

            let header = assemble(&parent, &input, &results);
            assert_eq!(header.hash_slow(), results.hash);
            parent = header;
        }
    }

    /// Coverage mode reports semantic features and leaves every compared field unchanged.
    #[test]
    fn coverage_reports_features_without_changing_results() {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x42)).unwrap();
        let contract = Address::repeat_byte(0xc0);
        let world = BTreeMap::from([
            (
                signer.address(),
                Account { balance: U256::from(10u128.pow(19)), ..Default::default() },
            ),
            // CALLVALUE PUSH1 0 SSTORE STOP
            (contract, Account { code: bytes!("3460005500"), ..Default::default() }),
            // The BaseTime proxy the first Denim block installs an implementation into.
            (
                Predeploys::BASE_TIME,
                Account {
                    code: bytes!("00"),
                    storage: BTreeMap::from([(
                        B256::from(BaseTime::ADMIN_SLOT),
                        U256::from_be_slice(Predeploys::PROXY_ADMIN.as_slice()),
                    )]),
                    ..Default::default()
                },
            ),
        ]);
        let parent = Header {
            number: 58_365_650,
            timestamp: 1_794_863_505,
            gas_limit: 40_000_000,
            base_fee_per_gas: Some(5_000_000),
            extra_data: bytes!("01000003e80000000500000000004c4b40"),
            state_root: witness(&world).0,
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            ..Default::default()
        };
        let tx = TxEip1559 {
            chain_id: 8453,
            nonce: 0,
            gas_limit: 100_000,
            max_fee_per_gas: 10_000_000,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(contract),
            value: U256::from(3),
            ..Default::default()
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let raw = Bytes::from(BaseTxEnvelope::from(tx.into_signed(signature)).encoded_2718());
        let request = Request {
            id: "coverage".to_owned(),
            chain: BASE_MAINNET.to_owned(),
            mode: Mode::Build,
            ancestors: vec![Bytes::from(alloy_rlp::encode(&parent))],
            header: RequestHeader {
                number: parent.number + 1,
                timestamp: parent.timestamp + 1,
                coinbase: Predeploys::SEQUENCER_FEE_VAULT,
                prev_randao: B256::ZERO,
                gas_limit: parent.gas_limit,
                base_fee_per_gas: 5_000_000,
                extra_data: parent.extra_data,
                parent_beacon_block_root: Some(B256::with_last_byte(9)),
                excess_blob_gas: Some(0),
                results: None,
            },
            // The duplicate reuses the nonce and is rejected.
            transactions: vec![raw.clone(), raw],
            witness: witness(&world).1,
            trace: None,
        };

        let plain = DiffAdapter::base_mainnet().execute(&request).unwrap();
        let mut adapter = DiffAdapter::base_mainnet();
        adapter.coverage = true;
        let covered = adapter.execute(&request).unwrap();
        assert!(plain.coverage.is_none());
        assert!(plain.valid, "{plain:?}");
        let coverage = covered.coverage.clone().expect("coverage is on");
        assert_eq!(Response { coverage: None, ..covered }, plain);

        let has = |predicate: &dyn Fn(&str) -> bool| coverage.keys().any(|k| predicate(k));
        assert!(has(&|k| k.starts_with("op:") && k.ends_with(":SSTORE:d0")), "{coverage:?}");
        assert!(has(&|k| k.starts_with("gas:SSTORE:")), "{coverage:?}");
        assert!(has(&|k| k == "end:STOP:Stop"), "{coverage:?}");
        assert_eq!(coverage.get("tx:t2:s1"), Some(&1));
        assert!(has(&|k| k.starts_with("reject:t2:")), "{coverage:?}");
    }
}
