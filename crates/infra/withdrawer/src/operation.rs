use std::{collections::VecDeque, error::Error as StdError};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{Error as SolError, SolCall, SolEvent};
use alloy_transport::TransportError;
use alloy_trie::proof::ProofVerificationError;
use base_common_consensus::Predeploys;
use base_proof_contracts::{GameStatus, IAnchorStateRegistry, IDisputeGameFactory};
use thiserror::Error;

use crate::{
    FaultProofPortal, LegacyWithdrawalPortal, MessagePassed, ProofSystem, VerifiedWithdrawalProof,
    Withdrawal, WithdrawalDisputeGame, WithdrawalError, WithdrawalOutputOracle, WithdrawalPortal,
    WithdrawalRpc, WithdrawalRpcTransport, WithdrawerConfig,
};

/// Next step determined from the portal and proof contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalStatus {
    /// No further action is needed.
    Finalized,
    /// The withdrawal needs a new inclusion proof.
    Prove,
    /// The withdrawal can be executed.
    Finalize,
    /// The anchor state registry or legacy portal must be unpaused before proving or finalizing.
    Paused,
    /// Earliest known L1 ready time; an unresolved game can delay it further.
    NotReady {
        /// Earliest L1 timestamp allowed by the known delays.
        ready_at: u64,
        /// Game resolution is still required before its finality delay can start.
        waiting_for_game: bool,
    },
}

/// Failure of a complete withdrawal operation.
#[derive(Debug, Error)]
pub enum WithdrawerError {
    /// A provider request failed after removal of credential-bearing details.
    #[error("withdrawal RPC request failed: {0}")]
    Request(#[source] TransportError),
    /// A contract returned malformed ABI data.
    #[error("withdrawal contract response decoding failed for {call}: {source}")]
    Decode {
        /// ABI signature of the call whose response could not be decoded.
        call: &'static str,
        /// ABI error with response data removed.
        #[source]
        source: SolError,
    },
    /// Withdrawal extraction or local proof authentication failed.
    #[error(transparent)]
    Withdrawal(WithdrawalError),
    /// Local proof authentication failed, with proof-supplied values removed.
    #[error("{proof} proof verification failed: {source}")]
    Proof {
        /// Account or storage proof that failed verification.
        proof: &'static str,
        /// Proof error with data-bearing payloads replaced by their kind.
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
    /// A header required for proof verification or finality is missing.
    #[error("required block header is unavailable")]
    MissingBlock,
    /// The L2 RPC returned a different header than the requested proof block.
    #[error("L2 proof header has block {actual}, expected {expected}")]
    WrongProofBlock {
        /// Requested commitment block.
        expected: u64,
        /// Returned header's block number.
        actual: u64,
    },
    /// An onchain block number or timestamp cannot be represented safely.
    #[error("onchain block number or timestamp exceeds u64")]
    ValueOverflow,
    /// A factory game has no encoded L2 block number.
    #[error("dispute game {index} has malformed extra data")]
    InvalidGame {
        /// Factory index of the malformed game.
        index: U256,
    },
    /// No commitment includes the withdrawal yet.
    #[error("no published output covers withdrawal block {block_number}")]
    NoCoverage {
        /// L2 inclusion block requiring a commitment.
        block_number: u64,
    },
}

/// Published commitment used for local proof verification and portal calldata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WithdrawalProvingOutput {
    /// L2 block covered by the commitment.
    pub block_number: u64,
    /// Factory game or oracle output index.
    pub index: U256,
    /// Version-zero output root.
    pub root: B256,
}

/// Read-only withdrawal decisions from onchain state.
#[derive(Debug)]
pub struct Withdrawer {
    /// Validated operation settings.
    pub config: WithdrawerConfig,
}

impl Withdrawer {
    /// Constructs an operation without network or signer access.
    pub const fn new(config: WithdrawerConfig) -> Self {
        Self { config }
    }

    /// Reads the next action without preparing, signing, or submitting a transaction.
    pub async fn status(
        &self,
        rpc: &WithdrawalRpc,
        withdrawal: &Withdrawal,
        submitter: Address,
    ) -> Result<WithdrawalStatus, WithdrawerError> {
        let portal = self.config.network.portal;
        let hash = withdrawal.hash();
        if Self::read(
            rpc,
            portal,
            WithdrawalPortal::finalizedWithdrawalsCall { withdrawalHash: hash },
        )
        .await?
        {
            return Ok(WithdrawalStatus::Finalized);
        }
        let (ready_at, waiting_for_game, game_finalized) = match self.config.network.proofs {
            ProofSystem::FaultProofs { anchor_state_registry, .. } => {
                if Self::read(rpc, anchor_state_registry, IAnchorStateRegistry::pausedCall {})
                    .await?
                {
                    return Ok(WithdrawalStatus::Paused);
                }
                let proof = Self::read(
                    rpc,
                    portal,
                    FaultProofPortal::provenWithdrawalsCall { withdrawalHash: hash, submitter },
                )
                .await?;
                if proof.timestamp == 0
                    || !Self::game_eligible(rpc, anchor_state_registry, proof.disputeGameProxy)
                        .await?
                {
                    return Ok(WithdrawalStatus::Prove);
                }
                let maturity =
                    Self::read(rpc, portal, FaultProofPortal::proofMaturityDelaySecondsCall {})
                        .await?;
                let mut ready_at = Self::ready_time(U256::from(proof.timestamp), maturity)?;
                let resolved = Self::read(
                    rpc,
                    proof.disputeGameProxy,
                    WithdrawalDisputeGame::resolvedAtCall {},
                )
                .await?;
                let waiting = resolved == 0;
                if !waiting {
                    let delay = Self::read(
                        rpc,
                        anchor_state_registry,
                        IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall {},
                    )
                    .await?;
                    ready_at = ready_at.max(Self::ready_time(U256::from(resolved), delay)?);
                }
                let finalized = Self::read(
                    rpc,
                    anchor_state_registry,
                    IAnchorStateRegistry::isGameFinalizedCall { game: proof.disputeGameProxy },
                )
                .await?;
                (ready_at, waiting, finalized)
            }
            ProofSystem::OutputOracle { l2_output_oracle } => {
                if Self::read(rpc, portal, LegacyWithdrawalPortal::pausedCall {}).await? {
                    return Ok(WithdrawalStatus::Paused);
                }
                let proof = Self::read(
                    rpc,
                    portal,
                    LegacyWithdrawalPortal::provenWithdrawalsCall { withdrawalHash: hash },
                )
                .await?;
                if proof.timestamp == 0 {
                    return Ok(WithdrawalStatus::Prove);
                }
                let output = match Self::read(
                    rpc,
                    l2_output_oracle,
                    WithdrawalOutputOracle::getL2OutputCall {
                        index: U256::from(proof.l2OutputIndex),
                    },
                )
                .await
                {
                    Ok(output) if output.outputRoot == proof.outputRoot => output,
                    Ok(_) => return Ok(WithdrawalStatus::Prove),
                    Err(error) if error.is_execution_revert() => {
                        return Ok(WithdrawalStatus::Prove);
                    }
                    Err(error) => return Err(error),
                };
                let period = Self::read(
                    rpc,
                    l2_output_oracle,
                    WithdrawalOutputOracle::FINALIZATION_PERIOD_SECONDSCall {},
                )
                .await?;
                (
                    Self::ready_time(U256::from(proof.timestamp.max(output.timestamp)), period)?,
                    false,
                    true,
                )
            }
        };
        let head = rpc
            .l1
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .ok_or(WithdrawerError::MissingBlock)?;
        if head.header.timestamp < ready_at || waiting_for_game || !game_finalized {
            return Ok(WithdrawalStatus::NotReady { ready_at, waiting_for_game });
        }
        if matches!(self.config.network.proofs, ProofSystem::FaultProofs { .. }) {
            // The portal can enforce proof-validity gates beyond the timing reported here.
            Self::read(
                rpc,
                portal,
                FaultProofPortal::checkWithdrawalCall { withdrawalHash: hash, submitter },
            )
            .await?;
        }
        Ok(WithdrawalStatus::Finalize)
    }

    /// Checks whether a game can support a current or replacement proof.
    pub async fn game_eligible(
        rpc: &WithdrawalRpc,
        registry: Address,
        game: Address,
    ) -> Result<bool, WithdrawerError> {
        if Self::read(rpc, registry, IAnchorStateRegistry::isGameBlacklistedCall { game }).await?
            || Self::read(rpc, registry, IAnchorStateRegistry::isGameRetiredCall { game }).await?
            || !Self::read(rpc, registry, IAnchorStateRegistry::isGameProperCall { game }).await?
            || !Self::read(rpc, registry, IAnchorStateRegistry::isGameRespectedCall { game })
                .await?
        {
            return Ok(false);
        }
        let status = Self::read(rpc, game, WithdrawalDisputeGame::statusCall {}).await?;
        Ok(status == GameStatus::InProgress as u8 || status == GameStatus::DefenderWins as u8)
    }

    /// Checks eligibility and reads the root for a factory index, proxy, and claimed L2 block.
    pub async fn game_proving_output(
        rpc: &WithdrawalRpc,
        registry: Address,
        candidate: (U256, Address, u64),
    ) -> Result<Option<WithdrawalProvingOutput>, WithdrawerError> {
        if !Self::game_eligible(rpc, registry, candidate.1).await? {
            return Ok(None);
        }
        let root = Self::read(rpc, candidate.1, WithdrawalDisputeGame::rootClaimCall {}).await?;
        Ok(Some(WithdrawalProvingOutput { block_number: candidate.2, index: candidate.0, root }))
    }

    /// Authenticates the newest finalized covering game, or the oldest eligible pending game.
    ///
    /// The backward walk uses three contract reads per covering game: `gameAtIndex`, `extraData`,
    /// and `isGameFinalized`. Eligibility and `rootClaim` reads wait until verification is about to run,
    /// including finalized games. Ineligible games are skipped without a warning.
    ///
    /// The first proof RPC failure stops the walk and switches untried saved candidates to newest
    /// first, for RPCs that only serve recent state. A second RPC failure returns immediately;
    /// exhausting the fallback returns the first RPC error. Unavailable blocks and invalid proofs
    /// are skipped. Legacy outputs use the first covering commitment and fail on proof errors.
    pub async fn proving_output(
        &self,
        rpc: &WithdrawalRpc,
        withdrawal: &Withdrawal,
    ) -> Result<(WithdrawalProvingOutput, VerifiedWithdrawalProof), WithdrawerError> {
        let block = U256::from(withdrawal.block_number);
        match self.config.network.proofs {
            ProofSystem::FaultProofs { dispute_game_factory, anchor_state_registry } => {
                let mut count =
                    Self::read(rpc, dispute_game_factory, IDisputeGameFactory::gameCountCall {})
                        .await?;
                let withdrawal_timestamp = rpc
                    .l2
                    .get_block_by_number(withdrawal.block_number.into())
                    .await?
                    .ok_or(WithdrawerError::MissingBlock)?
                    .header
                    .timestamp;
                let mut candidates = VecDeque::new();
                let mut first_rpc_error = None;
                loop {
                    let mut finalized_candidate = None;
                    while first_rpc_error.is_none() && count > U256::ZERO {
                        count -= U256::from(1);
                        let game = Self::read(
                            rpc,
                            dispute_game_factory,
                            IDisputeGameFactory::gameAtIndexCall { index: count },
                        )
                        .await?;
                        // A game created before the withdrawal cannot honestly cover it, nor can older games.
                        if game.timestamp < withdrawal_timestamp {
                            count = U256::ZERO;
                            break;
                        }
                        let extra_data =
                            Self::read(rpc, game.proxy, WithdrawalDisputeGame::extraDataCall {})
                                .await?;
                        let Some(encoded_block) = extra_data.get(..32) else {
                            let error = WithdrawerError::InvalidGame { index: count };
                            tracing::warn!(
                                game_index = %count, proxy = %game.proxy, reason = %error,
                                "skipping unusable dispute game"
                            );
                            continue;
                        };
                        let game_block = U256::from_be_slice(encoded_block);
                        if game_block < block {
                            continue;
                        }
                        let Ok(block_number) = u64::try_from(game_block) else {
                            tracing::warn!(
                                game_index = %count, proxy = %game.proxy,
                                reason = %WithdrawerError::ValueOverflow,
                                "skipping unusable dispute game"
                            );
                            continue;
                        };
                        let finalized = Self::read(
                            rpc,
                            anchor_state_registry,
                            IAnchorStateRegistry::isGameFinalizedCall { game: game.proxy },
                        )
                        .await?;
                        let candidate = (count, game.proxy, block_number);
                        // Finalized games need only proof maturity if eligible; prefer the newest that verifies.
                        if finalized {
                            finalized_candidate = Some(candidate);
                            break;
                        }
                        candidates.push_back(candidate);
                    }
                    let candidate = match finalized_candidate {
                        Some(candidate) => Some(candidate),
                        // Some RPCs only serve recent state; stop searching older games after a failure.
                        None if first_rpc_error.is_some() => candidates.pop_front(),
                        // Without a usable finalized game, the oldest candidate should resolve first.
                        None => candidates.pop_back(),
                    };
                    let Some(candidate) = candidate else { break };
                    let Some(output) =
                        Self::game_proving_output(rpc, anchor_state_registry, candidate).await?
                    else {
                        continue;
                    };
                    match Self::verify_output(rpc, withdrawal, output).await {
                        Ok(proof) => return Ok((output, proof)),
                        Err(
                            error @ (WithdrawerError::MissingBlock
                            | WithdrawerError::Withdrawal(_)
                            | WithdrawerError::Proof { .. }
                            | WithdrawerError::Request(_)),
                        ) => {
                            tracing::warn!(
                                game_index = %output.index, proxy = %candidate.1, reason = %error,
                                "skipping unusable dispute game"
                            );
                            if matches!(error, WithdrawerError::Request(_)) {
                                if first_rpc_error.is_some() {
                                    return Err(error);
                                }
                                first_rpc_error = Some(error);
                            }
                        }
                        Err(error) => return Err(error),
                    }
                }
                if let Some(error) = first_rpc_error {
                    return Err(error);
                }
            }
            ProofSystem::OutputOracle { l2_output_oracle } => {
                let latest = Self::read(
                    rpc,
                    l2_output_oracle,
                    WithdrawalOutputOracle::latestBlockNumberCall {},
                )
                .await?;
                if latest >= block {
                    let index = Self::read(
                        rpc,
                        l2_output_oracle,
                        WithdrawalOutputOracle::getL2OutputIndexAfterCall { blockNumber: block },
                    )
                    .await?;
                    let output = Self::read(
                        rpc,
                        l2_output_oracle,
                        WithdrawalOutputOracle::getL2OutputCall { index },
                    )
                    .await?;
                    if U256::from(output.l2BlockNumber) >= block {
                        let output = WithdrawalProvingOutput {
                            block_number: output
                                .l2BlockNumber
                                .try_into()
                                .map_err(|_| WithdrawerError::ValueOverflow)?,
                            index,
                            root: output.outputRoot,
                        };
                        let proof = Self::verify_output(rpc, withdrawal, output).await?;
                        return Ok((output, proof));
                    }
                }
            }
        }
        Err(WithdrawerError::NoCoverage { block_number: withdrawal.block_number })
    }

    /// Fetches and authenticates an output's header and message passer proof.
    pub async fn verify_output(
        rpc: &WithdrawalRpc,
        withdrawal: &Withdrawal,
        output: WithdrawalProvingOutput,
    ) -> Result<VerifiedWithdrawalProof, WithdrawerError> {
        let block = rpc
            .l2
            .get_block_by_number(output.block_number.into())
            .await?
            .ok_or(WithdrawerError::MissingBlock)?;
        if block.header.number != output.block_number {
            return Err(WithdrawerError::WrongProofBlock {
                expected: output.block_number,
                actual: block.header.number,
            });
        }
        let proof = rpc
            .l2
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, vec![withdrawal.slot()])
            .block_id(output.block_number.into())
            .await?;
        Ok(withdrawal.verify_proof(&block.header, &proof, output.root)?)
    }

    /// Calls a contract at the latest mined block with redacted provider and decode errors.
    pub async fn read<C: SolCall>(
        rpc: &WithdrawalRpc,
        target: Address,
        call: C,
    ) -> Result<C::Return, WithdrawerError> {
        let request =
            TransactionRequest::default().to(target).input(Bytes::from(call.abi_encode()).into());
        let bytes = rpc.l1.call(request).latest().await?;
        C::abi_decode_returns_validate(&bytes)
            .map_err(|error| WithdrawerError::decode_error(C::SIGNATURE, error))
    }

    /// Computes the first allowed timestamp for the portal's strict elapsed-delay check.
    pub fn ready_time(timestamp: U256, delay: U256) -> Result<u64, WithdrawerError> {
        timestamp
            .checked_add(delay)
            .and_then(|time| time.checked_add(U256::from(1)))
            .and_then(|time| time.try_into().ok())
            .ok_or(WithdrawerError::ValueOverflow)
    }
}

impl WithdrawerError {
    /// Removes response data from ABI diagnostics while retaining safe source errors.
    pub fn decode_error(call: &'static str, error: SolError) -> Self {
        let source = match error {
            SolError::TypeCheckFail { .. } => SolError::custom("TypeCheckFail"),
            SolError::InvalidEnumValue { .. } => SolError::custom("InvalidEnumValue"),
            SolError::InvalidEventSignatureHash { .. } => {
                SolError::custom("InvalidEventSignatureHash")
            }
            SolError::InvalidLog { .. } => SolError::custom("InvalidLog"),
            SolError::UnknownSelector { .. } => SolError::custom("UnknownSelector"),
            SolError::FromHexError(_) => SolError::custom("FromHexError"),
            SolError::Other(_) => SolError::custom("Other"),
            error @ (SolError::Overrun
            | SolError::Reserve(_)
            | SolError::BufferNotEmpty
            | SolError::ReserMismatch
            | SolError::RecursionLimitExceeded(_)) => error,
        };
        Self::Decode { call, source }
    }

    /// Removes proof-supplied values while retaining safe proof source errors.
    pub fn proof_error(proof: &'static str, error: ProofVerificationError) -> Self {
        let source: Box<dyn StdError + Send + Sync> = match error {
            ProofVerificationError::ValueMismatch { .. } => "ValueMismatch".into(),
            error => Box::new(error),
        };
        Self::Proof { proof, source }
    }

    /// Classifies execution reverts after redaction, which must preserve the code and "execution reverted" marker.
    pub fn is_execution_revert(&self) -> bool {
        matches!(self, Self::Request(error) if error.as_error_resp().is_some_and(|payload| {
            payload.code == 3 || payload.message.trim_start().to_ascii_lowercase().starts_with("execution reverted")
        }))
    }
}

impl From<TransportError> for WithdrawerError {
    fn from(error: TransportError) -> Self {
        Self::Request(WithdrawalRpcTransport::redact_diagnostic(error))
    }
}

impl From<WithdrawalError> for WithdrawerError {
    fn from(error: WithdrawalError) -> Self {
        match error {
            WithdrawalError::DecodeEvent(error) => {
                Self::decode_error(MessagePassed::SIGNATURE, error)
            }
            WithdrawalError::AccountProof(error) => Self::proof_error("account", error),
            WithdrawalError::StorageProof(error) => Self::proof_error("storage", error),
            error => Self::Withdrawal(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::SolCall;
    use axum::{Json, Router, routing::post};
    use base_proof_contracts::{IAnchorStateRegistry, IDisputeGameFactory};
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};
    use tracing::instrument::WithSubscriber;

    use super::*;
    use crate::{
        FaultProofPortal, LegacyWithdrawalPortal, ProofSystem, Withdrawal, WithdrawalDisputeGame,
        WithdrawalOutputOracle, WithdrawalPortal, WithdrawerArgs,
    };

    #[derive(Debug, Clone)]
    struct Scenario {
        fault: bool,
        proven: u64,
        finalized: bool,
        pending_finalized: bool,
        paused: bool,
        blacklisted: bool,
        blacklisted_game: Option<u64>,
        retired: bool,
        proper: bool,
        respected: bool,
        game_status: u8,
        game_finalized: bool,
        finalized_games: Vec<bool>,
        replaced: bool,
        output_error: Option<Value>,
        no_coverage: bool,
        skip_latest: bool,
        game_count: u64,
        game_blocks: Vec<u64>,
        old_games_before: u64,
        malformed_response: bool,
        other_game_type: bool,
        wrong_root: bool,
        latest_wrong_root: bool,
        latest_extra_data: Option<Bytes>,
        proof_error: Option<Value>,
        proof_error_block: Option<u64>,
        now: u64,
        reject_check: bool,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                fault: true,
                proven: 0,
                finalized: false,
                pending_finalized: false,
                paused: false,
                blacklisted: false,
                blacklisted_game: None,
                retired: false,
                proper: true,
                respected: true,
                game_status: 2,
                game_finalized: true,
                finalized_games: Vec::new(),
                replaced: false,
                output_error: None,
                no_coverage: false,
                skip_latest: false,
                game_count: 1,
                game_blocks: Vec::new(),
                old_games_before: 0,
                malformed_response: false,
                other_game_type: false,
                wrong_root: false,
                latest_wrong_root: false,
                latest_extra_data: None,
                proof_error: None,
                proof_error_block: None,
                now: 1000,
                reject_check: false,
            }
        }
    }

    #[derive(Debug)]
    struct Harness {
        config: WithdrawerConfig,
        requests: Arc<Mutex<Vec<Value>>>,
        tasks: Vec<JoinHandle<()>>,
    }

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    impl Scenario {
        fn game_proxy(index: u64) -> Address {
            Address::from_word(B256::from(U256::from(index + 9).to_be_bytes::<32>()))
        }

        async fn start(self) -> Harness {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let fixture: Value = serde_json::from_str(include_str!(
                "../fixtures/base-sepolia-withdrawal-proof.json"
            ))
            .unwrap();
            let scenario = Arc::new(self);
            let mut urls = Vec::new();
            let mut tasks = Vec::new();
            for l2 in [false, true] {
                let scenario = Arc::clone(&scenario);
                let requests = Arc::clone(&requests);
                let fixture = fixture.clone();
                let app = Router::new().route(
                    "/",
                    post(move |Json(request): Json<Value>| {
                        let scenario = Arc::clone(&scenario);
                        requests.lock().unwrap().push(request.clone());
                        let fixture = fixture.clone();
                        async move {
                            let result = scenario.respond(&request, l2, &fixture);
                            Json(match result {
                                Ok(result) => {
                                    json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                                }
                                Err(error) => {
                                    json!({"jsonrpc":"2.0", "id":request["id"], "error":error})
                                }
                            })
                        }
                    }),
                );
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                urls.push(format!("http://{}", listener.local_addr().unwrap()));
                tasks.push(tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                }));
            }
            let mut config = WithdrawerConfig::try_from(
                WithdrawerArgs::try_parse_from([
                    "withdrawer",
                    "--l1-rpc-url",
                    &urls[0],
                    "--withdrawal",
                    &B256::ZERO.to_string(),
                    "--private-key",
                    &B256::repeat_byte(1).to_string(),
                    "--gas-price",
                    "2",
                ])
                .unwrap(),
            )
            .unwrap();
            config.network.l2_rpc_url = urls[1].parse().unwrap();
            config.network.l1_chain_id = None;
            config.network.proofs = if scenario.fault {
                ProofSystem::FaultProofs {
                    dispute_game_factory: Address::repeat_byte(2),
                    anchor_state_registry: Address::repeat_byte(3),
                }
            } else {
                ProofSystem::OutputOracle { l2_output_oracle: Address::repeat_byte(2) }
            };
            config.max_retries = 0;
            config.timeout = Duration::from_secs(5);
            Harness { config, requests, tasks }
        }

        fn respond(&self, request: &Value, l2: bool, fixture: &Value) -> Result<Value, Value> {
            let params = &request["params"];
            Ok(match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => {
                    if l2 && params[0] == "0xffffffffffffffff" {
                        return Ok(Value::Null);
                    }
                    let mut block = fixture["header"].clone();
                    if l2
                        && self
                            .game_blocks
                            .iter()
                            .any(|number| params[0] == json!(format!("{number:#x}")))
                    {
                        let mut header: alloy_rpc_types_eth::Header =
                            serde_json::from_value(block).unwrap();
                        header.inner.number = u64::from_str_radix(
                            params[0].as_str().unwrap().trim_start_matches("0x"),
                            16,
                        )
                        .unwrap();
                        header.hash = header.inner.hash_slow();
                        block = serde_json::to_value(header).unwrap();
                    }
                    if !l2 {
                        block["timestamp"] = json!(format!("0x{:x}", self.now));
                    }
                    block["transactions"] = json!([]);
                    block
                }
                "eth_getProof" => {
                    if self
                        .proof_error_block
                        .is_none_or(|number| params[2] == json!(format!("{number:#x}")))
                        && let Some(error) = &self.proof_error
                    {
                        return Err(error.clone());
                    }
                    fixture["proof"].clone()
                }
                "eth_call" => {
                    if self.malformed_response {
                        return Ok(json!(B256::repeat_byte(0x61)));
                    }
                    let input: Bytes = serde_json::from_value(params[0]["input"].clone()).unwrap();
                    macro_rules! reply {
                        ($call:ty, $($value:expr),* $(,)?) => {
                            if input[..4] == <$call>::SELECTOR { return Ok(json!(Bytes::from(<$call>::abi_encode_returns_tuple(&($($value,)*))))); }
                        }
                    }
                    let claim: B256 = if self.wrong_root {
                        B256::ZERO
                    } else {
                        serde_json::from_value(fixture["game"]["rootClaim"].clone()).unwrap()
                    };
                    let block = if self.no_coverage {
                        1
                    } else {
                        u64::from_str_radix(
                            fixture["header"]["number"].as_str().unwrap().trim_start_matches("0x"),
                            16,
                        )
                        .unwrap()
                    };
                    reply!(
                        WithdrawalPortal::finalizedWithdrawalsCall,
                        self.finalized || (params[1] == "pending" && self.pending_finalized)
                    );
                    if self.fault {
                        reply!(IAnchorStateRegistry::pausedCall, self.paused);
                    } else {
                        reply!(LegacyWithdrawalPortal::pausedCall, self.paused);
                    }
                    if input[..4] == FaultProofPortal::provenWithdrawalsCall::SELECTOR {
                        let call =
                            FaultProofPortal::provenWithdrawalsCall::abi_decode(&input).unwrap();
                        assert_eq!(
                            call.submitter,
                            alloy_signer_local::PrivateKeySigner::from_bytes(&B256::repeat_byte(1))
                                .unwrap()
                                .address()
                        );
                        reply!(
                            FaultProofPortal::provenWithdrawalsCall,
                            Address::repeat_byte(8),
                            self.proven
                        );
                    }
                    reply!(
                        LegacyWithdrawalPortal::provenWithdrawalsCall,
                        if self.replaced { B256::ZERO } else { claim },
                        u128::from(self.proven),
                        1u128
                    );
                    let old_game = input.get(16..36) == Some(Address::repeat_byte(8).as_slice());
                    if input[..4] == IAnchorStateRegistry::isGameBlacklistedCall::SELECTOR {
                        let call = IAnchorStateRegistry::isGameBlacklistedCall::abi_decode(&input)
                            .unwrap();
                        reply!(
                            IAnchorStateRegistry::isGameBlacklistedCall,
                            (old_game && self.blacklisted)
                                || self
                                    .blacklisted_game
                                    .is_some_and(|index| { call.game == Self::game_proxy(index) })
                        );
                    }
                    reply!(IAnchorStateRegistry::isGameRetiredCall, old_game && self.retired);
                    reply!(
                        IAnchorStateRegistry::isGameProperCall,
                        !self.paused && (!old_game || self.proper)
                    );
                    reply!(IAnchorStateRegistry::isGameRespectedCall, !old_game || self.respected);
                    if input[..4] == IAnchorStateRegistry::isGameFinalizedCall::SELECTOR {
                        let call =
                            IAnchorStateRegistry::isGameFinalizedCall::abi_decode(&input).unwrap();
                        let finalized = self
                            .finalized_games
                            .iter()
                            .enumerate()
                            .find(|(index, _)| call.game == Self::game_proxy(*index as u64))
                            .map_or(self.game_finalized, |(_, finalized)| *finalized);
                        reply!(IAnchorStateRegistry::isGameFinalizedCall, finalized);
                    }
                    reply!(
                        IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall,
                        U256::from(200)
                    );
                    reply!(
                        WithdrawalDisputeGame::statusCall,
                        if params[0]["to"] == json!(Address::repeat_byte(8)) {
                            self.game_status
                        } else {
                            2u8
                        }
                    );
                    reply!(
                        WithdrawalDisputeGame::resolvedAtCall,
                        if self.game_status == 0 { 0u64 } else { 700u64 }
                    );
                    reply!(FaultProofPortal::proofMaturityDelaySecondsCall, U256::from(100));
                    reply!(
                        IDisputeGameFactory::gameCountCall,
                        U256::from(if self.skip_latest { 2 } else { self.game_count })
                    );
                    if input[..4] == IDisputeGameFactory::gameAtIndexCall::SELECTOR {
                        let call =
                            IDisputeGameFactory::gameAtIndexCall::abi_decode(&input).unwrap();
                        let index: u64 = call.index.try_into().unwrap();
                        let timestamp = u64::from_str_radix(
                            fixture["header"]["timestamp"]
                                .as_str()
                                .unwrap()
                                .trim_start_matches("0x"),
                            16,
                        )
                        .unwrap();
                        reply!(
                            IDisputeGameFactory::gameAtIndexCall,
                            if self.other_game_type && !(self.skip_latest && index == 1) {
                                0u32
                            } else {
                                621u32
                            },
                            if index < self.old_games_before { timestamp - 1 } else { timestamp },
                            Self::game_proxy(index)
                        );
                    }
                    let latest = params[0]["to"] == json!(Self::game_proxy(1));
                    let game_block = self
                        .game_blocks
                        .iter()
                        .enumerate()
                        .find(|(index, _)| {
                            params[0]["to"] == json!(Self::game_proxy(*index as u64))
                        })
                        .map_or(block, |(_, number)| *number);
                    if latest && let Some(extra_data) = &self.latest_extra_data {
                        reply!(WithdrawalDisputeGame::extraDataCall, extra_data.clone());
                    }
                    reply!(
                        WithdrawalDisputeGame::extraDataCall,
                        Bytes::copy_from_slice(
                            &U256::from(if latest && self.skip_latest { 1 } else { game_block })
                                .to_be_bytes::<32>()
                        )
                    );
                    let claim = if !self.game_blocks.is_empty() && !self.wrong_root {
                        let mut header: alloy_rpc_types_eth::Header =
                            serde_json::from_value(fixture["header"].clone()).unwrap();
                        header.inner.number = game_block;
                        base_protocol::OutputRoot::from_parts(
                            header.state_root,
                            serde_json::from_value(fixture["proof"]["storageHash"].clone())
                                .unwrap(),
                            header.inner.hash_slow(),
                        )
                        .hash()
                    } else {
                        claim
                    };
                    reply!(
                        WithdrawalDisputeGame::rootClaimCall,
                        if latest && self.latest_wrong_root { B256::ZERO } else { claim }
                    );
                    reply!(WithdrawalOutputOracle::latestBlockNumberCall, U256::from(block));
                    reply!(WithdrawalOutputOracle::getL2OutputIndexAfterCall, U256::ZERO);
                    reply!(
                        WithdrawalOutputOracle::FINALIZATION_PERIOD_SECONDSCall,
                        U256::from(200)
                    );
                    if input[..4] == WithdrawalOutputOracle::getL2OutputCall::SELECTOR {
                        let call =
                            WithdrawalOutputOracle::getL2OutputCall::abi_decode(&input).unwrap();
                        if call.index == U256::from(1)
                            && let Some(error) = &self.output_error
                        {
                            return Err(error.clone());
                        }
                        return Ok(json!(Bytes::from(
                            WithdrawalOutputOracle::getL2OutputCall::abi_encode_returns_tuple(&(
                                claim,
                                50u128,
                                u128::from(block)
                            ))
                        )));
                    }
                    if input[..4] == FaultProofPortal::checkWithdrawalCall::SELECTOR {
                        if self.reject_check {
                            return Err(json!({"code":3,"message":"execution reverted"}));
                        }
                        return Ok(json!("0x"));
                    }
                    panic!("unexpected contract selector {input}");
                }
                method => panic!("unexpected RPC method {method}"),
            })
        }
    }

    impl Harness {
        async fn status(&self) -> Result<WithdrawalStatus, WithdrawerError> {
            let rpc = WithdrawalRpc::new(&self.config).unwrap();
            let receipt = serde_json::from_str(include_str!(
                "../fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap();
            let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
            let submitter = alloy_signer_local::PrivateKeySigner::from_bytes(&B256::repeat_byte(1))
                .unwrap()
                .address();
            Withdrawer::new(self.config.clone()).status(&rpc, &withdrawal, submitter).await
        }

        async fn proving_output(&self) -> Result<WithdrawalProvingOutput, WithdrawerError> {
            let rpc = WithdrawalRpc::new(&self.config).unwrap();
            let receipt = serde_json::from_str(include_str!(
                "../fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap();
            let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
            Withdrawer::new(self.config.clone())
                .proving_output(&rpc, &withdrawal)
                .await
                .map(|(output, _)| output)
        }
    }

    #[tokio::test]
    async fn proves_then_finalizes_both_proof_systems() {
        for fault in [false, true] {
            for (proven, status) in
                [(0, WithdrawalStatus::Prove), (100, WithdrawalStatus::Finalize)]
            {
                let server = Scenario { fault, proven, ..Default::default() }.start().await;
                let outcomes = server.status().await.unwrap();
                assert_eq!(outcomes, status);
            }
        }
    }

    #[tokio::test]
    async fn invalidated_fault_proofs_are_proven_again() {
        for scenario in [
            Scenario { blacklisted: true, ..Default::default() },
            Scenario { retired: true, ..Default::default() },
            Scenario { proper: false, ..Default::default() },
            Scenario { respected: false, ..Default::default() },
            Scenario { game_status: 1, ..Default::default() },
        ] {
            let server = Scenario { proven: 100, ..scenario }.start().await;
            let outcomes = server.status().await.unwrap();
            assert_eq!(outcomes, WithdrawalStatus::Prove, "invalidated proof was retained");
        }
    }

    #[tokio::test]
    async fn paused_contracts_wait_without_replacing_proofs() {
        for fault in [false, true] {
            for proven in [0, 100] {
                let server =
                    Scenario { fault, paused: true, proven, ..Default::default() }.start().await;
                assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Paused);
            }
        }
    }

    #[tokio::test]
    async fn replaced_and_deleted_legacy_proofs_are_proven_again() {
        for (replaced, output_error) in [
            (true, None),
            (false, Some(json!({"code":3,"message":"execution reverted"}))),
            (false, Some(json!({"code":-32000,"message":"execution reverted: missing output"}))),
        ] {
            let server = Scenario {
                fault: false,
                proven: 100,
                replaced,
                output_error,
                ..Default::default()
            }
            .start()
            .await;
            let outcomes = server.status().await.unwrap();
            assert_eq!(outcomes, WithdrawalStatus::Prove);
        }
    }

    #[tokio::test]
    async fn legacy_rpc_failure_is_not_a_deleted_output() {
        let server = Scenario { fault: false, proven: 100, output_error: Some(json!({"code":-32000,"message":"upstream unavailable at https://user:password@host.invalid/key"})), ..Default::default() }.start().await;
        let error = server.status().await.expect_err("non-revert RPC error was ignored");
        assert!(!format!("{error:?}").contains("password"));
        let source = std::error::Error::source(&error).expect("RPC error must retain its source");
        assert!(!format!("{source:?}").contains("password"));
    }

    #[tokio::test]
    async fn finalization_waits_for_l1_time_and_game_finality() {
        for (scenario, ready_at, waiting_for_game) in [
            (Scenario { fault: false, now: 300, ..Default::default() }, 301, false),
            (Scenario { now: 200, game_finalized: false, ..Default::default() }, 901, false),
            (Scenario { game_status: 0, game_finalized: false, ..Default::default() }, 201, true),
            (Scenario { proven: 950, ..Default::default() }, 1051, false),
        ] {
            let proven = if scenario.proven == 0 { 100 } else { scenario.proven };
            let server = Scenario { proven, ..scenario }.start().await;
            let outcomes = server.status().await.unwrap();
            assert_eq!(outcomes, WithdrawalStatus::NotReady { ready_at, waiting_for_game });
        }
    }

    #[tokio::test]
    async fn portal_check_remains_authoritative() {
        let server =
            Scenario { proven: 100, reject_check: true, ..Default::default() }.start().await;
        assert!(server.status().await.is_err(), "portal refusal was ignored");
    }

    #[tokio::test]
    async fn finalized_withdrawal_needs_no_action() {
        let server = Scenario { finalized: true, ..Default::default() }.start().await;
        let outcomes = server.status().await.unwrap();
        assert_eq!(outcomes, WithdrawalStatus::Finalized);
    }

    #[tokio::test]
    async fn pending_finalization_does_not_complete_a_withdrawal() {
        for fault in [false, true] {
            let server =
                Scenario { fault, proven: 100, pending_finalized: true, ..Default::default() }
                    .start()
                    .await;
            assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Finalize);
        }
    }

    #[tokio::test]
    async fn status_uses_the_first_allowed_l1_timestamp() {
        for (now, expected) in [
            (300, WithdrawalStatus::NotReady { ready_at: 301, waiting_for_game: false }),
            (301, WithdrawalStatus::Finalize),
        ] {
            let server =
                Scenario { fault: false, proven: 100, now, ..Default::default() }.start().await;
            let rpc = WithdrawalRpc::new(&server.config).unwrap();
            let receipt = serde_json::from_str(include_str!(
                "../fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap();
            let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
            let flow = Withdrawer::new(server.config.clone());
            assert_eq!(flow.status(&rpc, &withdrawal, Address::ZERO).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn contract_decode_errors_withhold_response_bytes() {
        let server = Scenario { malformed_response: true, ..Default::default() }.start().await;
        let error = server.status().await.unwrap_err();
        assert!(matches!(error, WithdrawerError::Decode { .. }));
        let diagnostic = format!("{error} {error:?}");
        assert!(diagnostic.contains(WithdrawalPortal::finalizedWithdrawalsCall::SIGNATURE));
        assert!(diagnostic.contains("TypeCheckFail"));
        assert!(!diagnostic.contains("61616161"), "response bytes leaked: {diagnostic}");
    }

    #[tokio::test]
    async fn missing_covering_output_returns_no_coverage() {
        for fault in [false, true] {
            let server = Scenario { fault, no_coverage: true, ..Default::default() }.start().await;
            let result = server.proving_output().await;
            assert!(
                matches!(result, Err(WithdrawerError::NoCoverage { .. })),
                "missing no-coverage error: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn proof_must_authenticate_the_published_claim() {
        let server =
            Scenario { fault: false, wrong_root: true, ..Default::default() }.start().await;
        assert!(matches!(
            server.proving_output().await,
            Err(WithdrawerError::Withdrawal(WithdrawalError::OutputRootMismatch { .. }))
        ));

        for valid_older_game in [true, false] {
            let server = Scenario {
                game_count: 2,
                latest_wrong_root: true,
                wrong_root: !valid_older_game,
                ..Default::default()
            }
            .start()
            .await;
            let result = server.proving_output().await;
            if valid_older_game {
                assert_eq!(result.unwrap().index, U256::ZERO);
            } else {
                assert!(matches!(result, Err(WithdrawerError::NoCoverage { .. })));
            }
        }
    }

    #[tokio::test]
    async fn game_search_skips_unusable_claimed_blocks() {
        for extra_data in [
            Bytes::from(vec![0; 31]),
            Bytes::copy_from_slice(&U256::MAX.to_be_bytes::<32>()),
            Bytes::copy_from_slice(&U256::from(u64::MAX).to_be_bytes::<32>()),
        ] {
            let server = Scenario {
                game_count: 2,
                latest_extra_data: Some(extra_data),
                ..Default::default()
            }
            .start()
            .await;
            assert_eq!(server.proving_output().await.unwrap().index, U256::ZERO);
        }
    }

    #[tokio::test]
    async fn game_search_prefers_an_already_finalized_covering_game() {
        let server = Scenario {
            game_count: 3,
            finalized_games: vec![false, true, false],
            ..Default::default()
        }
        .start()
        .await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::from(1));
    }

    #[tokio::test]
    async fn game_search_uses_the_oldest_pending_covering_game() {
        let server =
            Scenario { game_count: 3, game_finalized: false, ..Default::default() }.start().await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::from(0));
    }

    #[tokio::test]
    async fn game_search_reads_pending_eligibility_and_roots_only_when_verifying() {
        let server =
            Scenario { game_count: 4, game_finalized: false, ..Default::default() }.start().await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::from(0));
        let requests = server.requests.lock().unwrap();
        let calls: Vec<Bytes> = requests
            .iter()
            .filter(|request| request["method"] == "eth_call")
            .map(|request| serde_json::from_value(request["params"][0]["input"].clone()).unwrap())
            .collect();
        for (selector, expected) in [
            (IDisputeGameFactory::gameAtIndexCall::SELECTOR, 4),
            (WithdrawalDisputeGame::extraDataCall::SELECTOR, 4),
            (IAnchorStateRegistry::isGameFinalizedCall::SELECTOR, 4),
            (IAnchorStateRegistry::isGameProperCall::SELECTOR, 1),
            (WithdrawalDisputeGame::rootClaimCall::SELECTOR, 1),
        ] {
            assert_eq!(calls.iter().filter(|input| input[..4] == selector).count(), expected);
        }
    }

    #[tokio::test]
    async fn game_search_skips_a_finalized_but_blacklisted_game() {
        let server = Scenario { game_count: 2, blacklisted_game: Some(1), ..Default::default() }
            .start()
            .await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::from(0));
    }

    #[tokio::test]
    async fn game_search_skips_pruned_proofs() {
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/base-sepolia-withdrawal-proof.json"))
                .unwrap();
        let block = u64::from_str_radix(
            fixture["header"]["number"].as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap();
        for (finalized_games, proof_error_block) in
            [(vec![false, false, false], block), (vec![true, true, false], block + 1)]
        {
            let server = Scenario {
                game_count: 3,
                finalized_games,
                game_blocks: vec![block, block + 1, block + 2],
                proof_error_block: Some(proof_error_block),
                proof_error: Some(
                    json!({"code": -32000, "message": "historical state unavailable"}),
                ),
                ..Default::default()
            }
            .start()
            .await;
            assert_eq!(server.proving_output().await.unwrap().index, U256::from(2));
        }
    }

    #[tokio::test]
    async fn game_search_stops_after_two_proof_rpc_failures() {
        let server = Scenario {
            game_count: 3,
            game_finalized: false,
            proof_error: Some(json!({
                "code": -32602,
                "message": "invalid at https://user:password@host.invalid/key",
                "data": "secret-value"
            })),
            ..Default::default()
        }
        .start()
        .await;
        let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let error = server.proving_output().with_subscriber(subscriber).await.unwrap_err();
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        for field in ["game_index=0", "game_index=2", "proxy=", "reason="] {
            assert!(logs.contains(field), "missing {field}: {logs}");
        }
        assert!(matches!(error, WithdrawerError::Request(_)));
        assert!(std::error::Error::source(&error).is_some());
        for secret in ["password", "host.invalid", "secret-value"] {
            assert!(!format!("{error} {error:?} {logs}").contains(secret));
        }
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "eth_getProof")
                .count(),
            2,
        );
    }

    #[tokio::test]
    async fn game_search_skips_a_newer_noncovering_game() {
        let server = Scenario { skip_latest: true, ..Default::default() }.start().await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::ZERO);
    }

    #[tokio::test]
    async fn game_search_uses_a_still_respected_earlier_game_type() {
        let server = Scenario { other_game_type: true, skip_latest: true, ..Default::default() }
            .start()
            .await;
        assert_eq!(server.proving_output().await.unwrap().index, U256::ZERO);
    }

    #[tokio::test]
    async fn game_search_stops_at_games_older_than_the_withdrawal() {
        for other_game_type in [false, true] {
            let server = Scenario {
                no_coverage: true,
                game_count: 1_000,
                old_games_before: 940,
                other_game_type,
                ..Default::default()
            }
            .start()
            .await;
            assert!(matches!(
                server.proving_output().await,
                Err(WithdrawerError::NoCoverage { .. })
            ));
            let requests = server.requests.lock().unwrap();
            let inspected: Vec<_> = requests
                .iter()
                .filter(|request| request["method"] == "eth_call")
                .filter_map(|request| {
                    let input: Bytes =
                        serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                    IDisputeGameFactory::gameAtIndexCall::abi_decode_validate(&input).ok()
                })
                .map(|call| call.index)
                .collect();
            assert_eq!(inspected, (939..1_000).rev().map(U256::from).collect::<Vec<_>>());
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request["method"] == "eth_getBlockByNumber")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn withdrawal_errors_withhold_response_bytes_through_the_source_chain() {
        let rejected = b"rejected-credential";
        let proof_error = || ProofVerificationError::ValueMismatch {
            path: alloy_trie::Nibbles::unpack(rejected),
            got: Some(Bytes::copy_from_slice(rejected)),
            expected: Some(Bytes::copy_from_slice(rejected)),
        };
        for (kind, error) in [
            (
                "TypeCheckFail",
                WithdrawalError::DecodeEvent(SolError::type_check_fail(rejected, "uint256")),
            ),
            ("ValueMismatch", WithdrawalError::AccountProof(proof_error())),
            ("ValueMismatch", WithdrawalError::StorageProof(proof_error())),
        ] {
            let error = WithdrawerError::from(error);
            assert!(error.source().is_some());
            let mut diagnostic = String::new();
            let mut source = Some(&error as &dyn StdError);
            while let Some(error) = source {
                diagnostic.push_str(&format!("{error} {error:?} "));
                source = error.source();
            }
            assert!(diagnostic.contains(kind));
            for secret in ["rejected-credential", "72656a65637465642d63726564656e7469616c"] {
                assert!(!diagnostic.contains(secret), "response bytes leaked: {diagnostic}");
            }
        }
    }
}
