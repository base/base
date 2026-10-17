use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{Error as SolError, SolCall};
use alloy_transport::TransportError;
use base_proof_contracts::{GameStatus, IAnchorStateRegistry};
use thiserror::Error;

use crate::{
    FaultProofPortal, LegacyWithdrawalPortal, ProofSystem, Withdrawal, WithdrawalDisputeGame,
    WithdrawalOutputOracle, WithdrawalPortal, WithdrawalRpc, WithdrawalRpcTransport,
    WithdrawerConfig,
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
    /// The anchor state registry must be unpaused before proving or finalizing.
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
    /// A header required for proof verification or finality is missing.
    #[error("required block header is unavailable")]
    MissingBlock,
    /// An onchain block number or timestamp cannot be represented safely.
    #[error("onchain block number or timestamp exceeds u64")]
    ValueOverflow,
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

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_sol_types::SolCall;
    use axum::{Json, Router, routing::post};
    use base_proof_contracts::IAnchorStateRegistry;
    use clap::Parser;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};

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
        retired: bool,
        proper: bool,
        respected: bool,
        game_status: u8,
        game_finalized: bool,
        replaced: bool,
        output_error: Option<Value>,
        malformed_response: bool,
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
                retired: false,
                proper: true,
                respected: true,
                game_status: 2,
                game_finalized: true,
                replaced: false,
                output_error: None,
                malformed_response: false,
                now: 1000,
                reject_check: false,
            }
        }
    }

    #[derive(Debug)]
    struct Harness {
        config: WithdrawerConfig,
        tasks: Vec<JoinHandle<()>>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    impl Scenario {
        async fn start(self) -> Harness {
            let fixture: Value = serde_json::from_str(include_str!(
                "../fixtures/base-sepolia-withdrawal-proof.json"
            ))
            .unwrap();
            let scenario = Arc::new(self);
            let mut urls = Vec::new();
            let mut tasks = Vec::new();
            for l2 in [false, true] {
                let scenario = Arc::clone(&scenario);
                let fixture = fixture.clone();
                let app = Router::new().route(
                    "/",
                    post(move |Json(request): Json<Value>| {
                        let scenario = Arc::clone(&scenario);
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
            Harness { config, tasks }
        }

        fn respond(&self, request: &Value, l2: bool, fixture: &Value) -> Result<Value, Value> {
            let params = &request["params"];
            Ok(match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => {
                    let mut block = fixture["header"].clone();
                    if !l2 {
                        block["timestamp"] = json!(format!("0x{:x}", self.now));
                    }
                    block["transactions"] = json!([]);
                    block
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
                    let claim: B256 =
                        serde_json::from_value(fixture["game"]["rootClaim"].clone()).unwrap();
                    let block = u64::from_str_radix(
                        fixture["header"]["number"].as_str().unwrap().trim_start_matches("0x"),
                        16,
                    )
                    .unwrap();
                    reply!(
                        WithdrawalPortal::finalizedWithdrawalsCall,
                        self.finalized || (params[1] == "pending" && self.pending_finalized)
                    );
                    reply!(IAnchorStateRegistry::pausedCall, self.paused);
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
                    reply!(
                        IAnchorStateRegistry::isGameBlacklistedCall,
                        old_game && self.blacklisted
                    );
                    reply!(IAnchorStateRegistry::isGameRetiredCall, old_game && self.retired);
                    reply!(
                        IAnchorStateRegistry::isGameProperCall,
                        !self.paused && (!old_game || self.proper)
                    );
                    reply!(IAnchorStateRegistry::isGameRespectedCall, !old_game || self.respected);
                    reply!(IAnchorStateRegistry::isGameFinalizedCall, self.game_finalized);
                    reply!(
                        IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall,
                        U256::from(200)
                    );
                    reply!(WithdrawalDisputeGame::statusCall, self.game_status);
                    reply!(
                        WithdrawalDisputeGame::resolvedAtCall,
                        if self.game_status == 0 { 0u64 } else { 700u64 }
                    );
                    reply!(FaultProofPortal::proofMaturityDelaySecondsCall, U256::from(100));
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
    async fn paused_registry_waits_without_replacing_proofs() {
        for proven in [0, 100] {
            let server = Scenario { paused: true, proven, ..Default::default() }.start().await;
            assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Paused);
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
}
