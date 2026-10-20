use std::{collections::VecDeque, error::Error as StdError, sync::Arc};

use alloy_eips::BlockNumberOrTag;
use alloy_network::{EthereumWallet, TxSigner};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, RootProvider};
use alloy_rpc_types_eth::{TransactionReceipt, TransactionRequest};
use alloy_sol_types::{Error as SolError, SolCall, SolEvent};
use alloy_transport::TransportError;
use alloy_trie::proof::ProofVerificationError;
use base_common_consensus::Predeploys;
use base_proof_contracts::{GameStatus, IAnchorStateRegistry, IDisputeGameFactory};
use base_tx_manager::{NoopTxMetrics, SimpleTxManager, TxCandidate, TxManager, TxManagerError};
use thiserror::Error;

use crate::{
    BroadcastJournal, FaultProofContracts, FaultProofPortal, LegacyWithdrawalPortal, MessagePassed,
    PreparedWithdrawalTransaction, ProofSystem, ResolvedProofSystem, VerifiedWithdrawalProof,
    Withdrawal, WithdrawalDisputeGame, WithdrawalError, WithdrawalOutputOracle, WithdrawalPortal,
    WithdrawalRpc, WithdrawalRpcError, WithdrawalRpcTransport, WithdrawalSigner,
    WithdrawalSignerError, WithdrawalTransactionError, WithdrawerConfig,
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

/// Result for one withdrawal in the L2 receipt.
#[derive(Debug)]
pub struct WithdrawalOutcome {
    /// Hash of the withdrawal fields.
    pub withdrawal_hash: B256,
    /// Decision before any submission.
    pub status: WithdrawalStatus,
    /// Confirmed L1 transaction, absent for dry runs and no-op decisions.
    pub transaction_hash: Option<B256>,
}

/// Failure of a complete withdrawal operation.
#[derive(Debug, Error)]
pub enum WithdrawerError {
    /// Custom proof contracts differ from the portal's trusted contracts.
    #[error("configured {configured} do not match portal-reported {reported}")]
    ProofContractsMismatch {
        /// Contracts required by the custom configuration.
        configured: FaultProofContracts,
        /// Contracts reported by the configured portal.
        reported: FaultProofContracts,
    },
    /// The fault-proof portal reported an unusable contract address.
    #[error("portal returned a zero address for {contract}")]
    ZeroPortalProofContract {
        /// Contract view that returned zero.
        contract: &'static str,
    },
    /// A lower-level RPC operation failed.
    #[error(transparent)]
    Rpc(WithdrawalRpcError),
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
    /// Signer construction failed.
    #[error(transparent)]
    Signer(#[from] WithdrawalSignerError),
    /// Transaction preparation failed.
    #[error(transparent)]
    Transaction(WithdrawalTransactionError),
    /// Transaction manager submission failed after credential redaction.
    #[error("transaction manager failed: {0}")]
    TxManager(#[from] TxManagerError),
    /// A transaction-manager RPC request exceeded its network timeout.
    #[error("transaction manager network timeout elapsed")]
    NetworkTimeout(#[source] tokio::time::error::Elapsed),
    /// The operation deadline elapsed.
    #[error("withdrawal operation timed out")]
    Timeout(#[source] tokio::time::error::Elapsed),
    /// Broadcast transactions may have reached L1; check their hashes for inclusion.
    #[error(
        "transactions {transaction_hashes:?} may have been broadcast or included; check them before retrying: {source}"
    )]
    Unconfirmed {
        /// Every broadcast hash that may have reached L1.
        transaction_hashes: Vec<B256>,
        /// Submission, receipt, or event validation failure, with credential-bearing details removed.
        #[source]
        source: Box<Self>,
    },
    /// The portal consumed the withdrawal, but its target call failed; it cannot be retried.
    #[error(
        "withdrawal {withdrawal_hash} finalized in transaction {transaction_hash}, but its target call failed; broadcast transactions: {transaction_hashes:?}"
    )]
    TargetCallFailed {
        /// Hash of the consumed withdrawal.
        withdrawal_hash: B256,
        /// Confirmed L1 finalization transaction.
        transaction_hash: B256,
        /// All signed transaction versions attempted for this finalization.
        transaction_hashes: Vec<B256>,
    },
    /// A successful receipt has no matching finalization event from the configured portal.
    #[error(
        "transaction {transaction_hash} has no portal finalization event for withdrawal {withdrawal_hash}; broadcast transactions: {transaction_hashes:?}"
    )]
    MissingFinalizationEvent {
        /// Expected withdrawal commitment.
        withdrawal_hash: B256,
        /// Confirmed L1 transaction whose logs were checked.
        transaction_hash: B256,
        /// All signed transaction versions attempted for this finalization.
        transaction_hashes: Vec<B256>,
    },
    /// The configured L2 transaction is not included or not found.
    #[error("L2 withdrawal receipt is unavailable")]
    MissingReceipt,
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

/// One receipt's sequential, deadline-bounded prove/finalize operation.
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

    /// Processes every withdrawal in receipt log order within one shared deadline.
    pub async fn run(&self) -> Result<Vec<WithdrawalOutcome>, WithdrawerError> {
        let pending_transaction = BroadcastJournal::default();
        let mut manager = None;
        let result = tokio::time::timeout(self.config.timeout, async {
            let rpc = WithdrawalRpc::new(&self.config)?;
            let proofs = self.resolve_proof_system(&rpc).await?;
            let signer = WithdrawalSigner::new(&self.config.signer).await?;
            let sender = signer.address();
            if !self.config.dry_run {
                manager = Some(self.manager(signer, pending_transaction.clone()).await?);
            }
            let receipt = rpc
                .l2
                .get_transaction_receipt(self.config.withdrawal)
                .await?
                .ok_or(WithdrawerError::MissingReceipt)?;
            let withdrawals = Withdrawal::from_receipt(&receipt)?;
            let mut outcomes = Vec::with_capacity(withdrawals.len());
            for withdrawal in withdrawals {
                pending_transaction.clear();
                let status = self.status(&rpc, proofs, &withdrawal, sender).await?;
                let hash = withdrawal.hash();
                tracing::info!(withdrawal_hash = %hash, ?status, "withdrawal decision");
                let transaction_hash = match status {
                    WithdrawalStatus::Prove | WithdrawalStatus::Finalize => {
                        let calldata = if status == WithdrawalStatus::Prove {
                            self.prove_calldata(&rpc, proofs, &withdrawal).await?
                        } else {
                            WithdrawalPortal::finalizeWithdrawalTransactionCall {
                                withdrawal: withdrawal.transaction.clone(),
                            }
                            .abi_encode()
                            .into()
                        };
                        self.submit(
                            &rpc,
                            sender,
                            manager.as_ref(),
                            calldata,
                            (status == WithdrawalStatus::Finalize).then_some(hash),
                            &pending_transaction,
                        )
                        .await?
                    }
                    WithdrawalStatus::Finalized
                    | WithdrawalStatus::Paused
                    | WithdrawalStatus::NotReady { .. } => None,
                };
                outcomes.push(WithdrawalOutcome {
                    withdrawal_hash: hash,
                    status,
                    transaction_hash,
                });
            }
            Ok(outcomes)
        })
        .await
        .map_err(|source| {
            let transaction_hashes = pending_transaction.hashes();
            if transaction_hashes.is_empty() {
                WithdrawerError::Timeout(source)
            } else {
                WithdrawerError::Unconfirmed {
                    transaction_hashes,
                    source: Box::new(WithdrawerError::Timeout(source)),
                }
            }
        });
        if let Some(manager) = manager {
            manager.close();
        }
        result?
    }

    /// Builds a serialized wallet-backed sender using pending nonces.
    pub async fn manager(
        &self,
        signer: WithdrawalSigner,
        journal: BroadcastJournal,
    ) -> Result<SimpleTxManager<RootProvider>, WithdrawerError> {
        let provider = WithdrawalRpcTransport::connect_for_submission(
            &self.config.l1_rpc_url,
            &self.config,
            journal,
        )
        .map_err(WithdrawerError::from)?;
        let chain_id =
            tokio::time::timeout(self.config.tx_manager.network_timeout, provider.get_chain_id())
                .await
                .map_err(WithdrawerError::NetworkTimeout)??;
        if let Some(expected) = self.config.network.l1_chain_id
            && expected != chain_id
        {
            return Err(
                WithdrawalTransactionError::L1ChainMismatch { expected, actual: chain_id }.into()
            );
        }
        Ok(SimpleTxManager::from_wallet(
            provider,
            EthereumWallet::from(signer),
            self.config.tx_manager.clone(),
            chain_id,
            Arc::new(NoopTxMetrics),
        )
        .await?
        .with_pending_nonce())
    }

    /// Reads and validates the portal's proof contracts, or keeps the configured legacy oracle.
    pub async fn resolve_proof_system(
        &self,
        rpc: &WithdrawalRpc,
    ) -> Result<ResolvedProofSystem, WithdrawerError> {
        match self.config.network.proofs {
            ProofSystem::FaultProofs { expected } => {
                let portal = self.config.network.portal;
                let reported = FaultProofContracts {
                    dispute_game_factory: Self::read(
                        rpc,
                        portal,
                        FaultProofPortal::disputeGameFactoryCall {},
                    )
                    .await?,
                    anchor_state_registry: Self::read(
                        rpc,
                        portal,
                        FaultProofPortal::anchorStateRegistryCall {},
                    )
                    .await?,
                };
                for (contract, address) in [
                    (
                        FaultProofPortal::disputeGameFactoryCall::SIGNATURE,
                        reported.dispute_game_factory,
                    ),
                    (
                        FaultProofPortal::anchorStateRegistryCall::SIGNATURE,
                        reported.anchor_state_registry,
                    ),
                ] {
                    if address.is_zero() {
                        return Err(WithdrawerError::ZeroPortalProofContract { contract });
                    }
                }
                if let Some(configured) = expected
                    && configured != reported
                {
                    return Err(WithdrawerError::ProofContractsMismatch { configured, reported });
                }
                Ok(ResolvedProofSystem::FaultProofs(reported))
            }
            ProofSystem::OutputOracle { l2_output_oracle } => {
                Ok(ResolvedProofSystem::OutputOracle { l2_output_oracle })
            }
        }
    }

    /// Reads the next action using this run's resolved contracts without preparing or signing.
    pub async fn status(
        &self,
        rpc: &WithdrawalRpc,
        proofs: ResolvedProofSystem,
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
        let (ready_at, waiting_for_game, game_finalized) = match proofs {
            ResolvedProofSystem::FaultProofs(contracts) => {
                if Self::read(
                    rpc,
                    contracts.anchor_state_registry,
                    IAnchorStateRegistry::pausedCall {},
                )
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
                    || !Self::game_eligible(
                        rpc,
                        contracts.anchor_state_registry,
                        proof.disputeGameProxy,
                    )
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
                        contracts.anchor_state_registry,
                        IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall {},
                    )
                    .await?;
                    ready_at = ready_at.max(Self::ready_time(U256::from(resolved), delay)?);
                }
                let finalized = Self::read(
                    rpc,
                    contracts.anchor_state_registry,
                    IAnchorStateRegistry::isGameFinalizedCall { game: proof.disputeGameProxy },
                )
                .await?;
                (ready_at, waiting, finalized)
            }
            ResolvedProofSystem::OutputOracle { l2_output_oracle } => {
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
        if matches!(proofs, ResolvedProofSystem::FaultProofs(_)) {
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
        proofs: ResolvedProofSystem,
        withdrawal: &Withdrawal,
    ) -> Result<(WithdrawalProvingOutput, VerifiedWithdrawalProof), WithdrawerError> {
        let block = U256::from(withdrawal.block_number);
        match proofs {
            ResolvedProofSystem::FaultProofs(contracts) => {
                let mut count = Self::read(
                    rpc,
                    contracts.dispute_game_factory,
                    IDisputeGameFactory::gameCountCall {},
                )
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
                            contracts.dispute_game_factory,
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
                            contracts.anchor_state_registry,
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
                        Self::game_proving_output(rpc, contracts.anchor_state_registry, candidate)
                            .await?
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
            ResolvedProofSystem::OutputOracle { l2_output_oracle } => {
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

    /// Fetches and authenticates a covering commitment before encoding the portal proof call.
    pub async fn prove_calldata(
        &self,
        rpc: &WithdrawalRpc,
        proofs: ResolvedProofSystem,
        withdrawal: &Withdrawal,
    ) -> Result<Bytes, WithdrawerError> {
        let (output, verified) = self.proving_output(rpc, proofs, withdrawal).await?;
        Ok(WithdrawalPortal::proveWithdrawalTransactionCall {
            withdrawal: withdrawal.transaction.clone(),
            outputIndex: output.index,
            outputRootProof: verified.output_root_proof,
            withdrawalProof: verified.storage_proof,
        }
        .abi_encode()
        .into())
    }

    /// Estimates a dry run or sends through the manager, retaining hashes on errors and cancellation.
    ///
    /// The manager must use this journal and pending nonces, with no concurrent sends.
    /// Clear the journal before each submission and retain it until the send or deadline resolves.
    pub async fn submit(
        &self,
        rpc: &WithdrawalRpc,
        sender: Address,
        manager: Option<&SimpleTxManager<RootProvider>>,
        calldata: Bytes,
        finalize_withdrawal: Option<B256>,
        published: &BroadcastJournal,
    ) -> Result<Option<B256>, WithdrawerError> {
        if self.config.dry_run {
            let prepared = PreparedWithdrawalTransaction::prepare(
                &self.config,
                rpc,
                sender,
                self.config.network.portal,
                calldata,
            )
            .await?;
            let transaction = &prepared.candidate;
            tracing::info!(
                target = %self.config.network.portal,
                calldata_length = transaction.tx_data.len(),
                selector = %Bytes::copy_from_slice(&transaction.tx_data[..transaction.tx_data.len().min(4)]),
                gas_limit = transaction.gas_limit,
                max_fee_per_gas = prepared.max_fee_per_gas,
                maximum_gas_cost = %prepared.maximum_gas_cost(),
                "dry run prepared withdrawal transaction"
            );
            return Ok(None);
        }
        let manager = manager.ok_or_else(|| {
            TxManagerError::InvalidConfig("missing withdrawal transaction manager".into())
        })?;
        // Withdrawals are serialized, but other jobs can publish with the same account.
        // Refresh pending state after both completed and cancelled sends.
        manager.nonce_manager().reset().await;
        let receipt = manager
            .send(TxCandidate {
                to: Some(self.config.network.portal),
                tx_data: calldata,
                gas_limit: self.config.gas_limit.unwrap_or_default(),
                ..Default::default()
            })
            .await
            .map_err(|source| {
                let source = WithdrawerError::TxManager(source);
                let transaction_hashes = published.hashes();
                if transaction_hashes.is_empty() {
                    source
                } else {
                    WithdrawerError::Unconfirmed { transaction_hashes, source: Box::new(source) }
                }
            })?;
        let hash = receipt.transaction_hash;
        if !receipt.status() {
            return Err(WithdrawerError::Unconfirmed {
                transaction_hashes: published.hashes(),
                source: Box::new(WithdrawalRpcError::Reverted(hash).into()),
            });
        }
        if let Some(withdrawal_hash) = finalize_withdrawal {
            self.check_finalization(&receipt, withdrawal_hash, hash, &published.hashes())?;
        }
        tracing::info!(transaction_hash = %hash, "withdrawal transaction confirmed");
        Ok(Some(hash))
    }

    /// Checks the portal's finalization result, which can fail despite a successful L1 receipt.
    pub fn check_finalization(
        &self,
        receipt: &TransactionReceipt,
        withdrawal_hash: B256,
        transaction_hash: B256,
        transaction_hashes: &[B256],
    ) -> Result<(), WithdrawerError> {
        for log in receipt.inner.logs() {
            if log.address() != self.config.network.portal
                || log.topics().first()
                    != Some(&WithdrawalPortal::WithdrawalFinalized::SIGNATURE_HASH)
                || log.topics().get(1) != Some(&withdrawal_hash)
            {
                continue;
            }
            let event = WithdrawalPortal::WithdrawalFinalized::decode_log_validate(&log.inner)
                .map_err(|error| WithdrawerError::Unconfirmed {
                    transaction_hashes: transaction_hashes.to_vec(),
                    source: Box::new(WithdrawerError::decode_error(
                        WithdrawalPortal::WithdrawalFinalized::SIGNATURE,
                        error,
                    )),
                })?
                .data;
            return if event.success {
                Ok(())
            } else {
                Err(WithdrawerError::TargetCallFailed {
                    withdrawal_hash,
                    transaction_hash,
                    transaction_hashes: transaction_hashes.to_vec(),
                })
            };
        }
        Err(WithdrawerError::MissingFinalizationEvent {
            withdrawal_hash,
            transaction_hash,
            transaction_hashes: transaction_hashes.to_vec(),
        })
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

impl From<WithdrawalRpcError> for WithdrawerError {
    fn from(error: WithdrawalRpcError) -> Self {
        match error {
            WithdrawalRpcError::Request(error) => error.into(),
            error => Self::Rpc(error),
        }
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

impl From<WithdrawalTransactionError> for WithdrawerError {
    fn from(error: WithdrawalTransactionError) -> Self {
        match error {
            WithdrawalTransactionError::Request(error) => error.into(),
            error => Self::Transaction(error),
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

    use alloy_consensus::{Transaction, TxEnvelope};
    use alloy_eips::Decodable2718;
    use alloy_primitives::{Address, Bytes, U256};
    use alloy_sol_types::SolCall;
    use axum::{Json, Router, response::IntoResponse, routing::post};
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
        portal_factory: Address,
        portal_registry: Address,
        portal_error: Option<Value>,
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
        broadcast_error: Option<Value>,
        broadcast_hang: bool,
        initial_broadcast_hang: bool,
        nonce_step: usize,
        receipt_error: Option<Value>,
        receipt_pending: bool,
        receipt_reverted: bool,
        finalization_success: Option<bool>,
        finalization_emitter: Address,
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
        dry: bool,
        hang: bool,
        second_withdrawal: bool,
        reject_check: bool,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                fault: true,
                portal_factory: Address::repeat_byte(2),
                portal_registry: Address::repeat_byte(3),
                portal_error: None,
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
                broadcast_error: None,
                broadcast_hang: false,
                initial_broadcast_hang: false,
                nonce_step: 1,
                receipt_error: None,
                receipt_pending: false,
                receipt_reverted: false,
                finalization_success: Some(true),
                finalization_emitter: crate::NetworkPreset::BaseMainnet.resolve().portal,
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
                dry: false,
                hang: false,
                second_withdrawal: false,
                reject_check: false,
            }
        }
    }

    #[derive(Debug)]
    struct Harness {
        config: WithdrawerConfig,
        sent: Arc<Mutex<Vec<TxEnvelope>>>,
        estimates: Arc<Mutex<usize>>,
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
            let sent = Arc::new(Mutex::new(Vec::<TxEnvelope>::new()));
            let estimates = Arc::new(Mutex::new(0));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let fixture: Value = serde_json::from_str(include_str!(
                "../fixtures/base-sepolia-withdrawal-proof.json"
            ))
            .unwrap();
            let mut receipt: Value = serde_json::from_str(include_str!(
                "../fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap();
            if self.second_withdrawal {
                let mut second = receipt["logs"][0].clone();
                let mut event =
                    Withdrawal::from_receipt(&serde_json::from_value(receipt.clone()).unwrap())
                        .unwrap()
                        .remove(0);
                event.transaction.nonce += U256::from(1);
                let message = crate::MessagePassed {
                    nonce: event.transaction.nonce,
                    sender: event.transaction.sender,
                    target: event.transaction.target,
                    value: event.transaction.value,
                    gasLimit: event.transaction.gasLimit,
                    data: event.transaction.data.clone(),
                    withdrawalHash: event.hash(),
                };
                let log = alloy_sol_types::SolEvent::encode_log_data(&message);
                second["topics"] = json!(log.topics());
                second["data"] = json!(log.data);
                receipt["logs"].as_array_mut().unwrap().push(second);
            }
            let scenario = Arc::new(self);
            receipt["blockHash"] = fixture["header"]["hash"].clone();
            let mut urls = Vec::new();
            let mut tasks = Vec::new();
            for l2 in [false, true] {
                let scenario = Arc::clone(&scenario);
                let sent = Arc::clone(&sent);
                let estimates = Arc::clone(&estimates);
                let requests = Arc::clone(&requests);
                let fixture = fixture.clone();
                let receipt = receipt.clone();
                let app = Router::new().route(
                    "/",
                    post(move |Json(request): Json<Value>| {
                        let scenario = Arc::clone(&scenario);
                        let sent = Arc::clone(&sent);
                        let estimates = Arc::clone(&estimates);
                        requests.lock().unwrap().push(request.clone());
                        let broadcast_attempt = requests
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|request| request["method"] == "eth_sendRawTransaction")
                            .count();
                        let fixture = fixture.clone();
                        let receipt = receipt.clone();
                        async move {
                            if scenario.hang {
                                std::future::pending::<()>().await;
                            }
                            if scenario.initial_broadcast_hang
                                && request["method"] == "eth_sendRawTransaction"
                                && broadcast_attempt == 1
                            {
                                std::future::pending::<()>().await;
                            }
                            let result = scenario
                                .respond(&request, l2, &fixture, &receipt, &sent, &estimates);
                            if scenario.broadcast_hang
                                && request["method"] == "eth_sendRawTransaction"
                            {
                                std::future::pending::<()>().await;
                            }
                            Json(match result {
                                Ok(result) => {
                                    json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
                                }
                                Err(error) => {
                                    json!({"jsonrpc":"2.0", "id":request["id"], "error":error})
                                }
                            })
                            .into_response()
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
                ])
                .unwrap(),
            )
            .unwrap();
            config.network.l2_rpc_url = urls[1].parse().unwrap();
            config.network.l1_chain_id = None;
            config.network.proofs = if scenario.fault {
                ProofSystem::FaultProofs {
                    expected: Some(FaultProofContracts {
                        dispute_game_factory: Address::repeat_byte(2),
                        anchor_state_registry: Address::repeat_byte(3),
                    }),
                }
            } else {
                ProofSystem::OutputOracle { l2_output_oracle: Address::repeat_byte(2) }
            };
            config.dry_run = scenario.dry;
            config.max_retries = 0;
            config.timeout = Duration::from_secs(5);
            config.tx_manager.num_confirmations = 1;
            config.tx_manager.resubmission_timeout = Duration::from_secs(1);
            config.tx_manager.receipt_query_interval = Duration::from_millis(100);
            Harness { config, sent, estimates, requests, tasks }
        }

        fn respond(
            &self,
            request: &Value,
            l2: bool,
            fixture: &Value,
            receipt: &Value,
            sent: &Mutex<Vec<TxEnvelope>>,
            estimates: &Mutex<usize>,
        ) -> Result<Value, Value> {
            let params = &request["params"];
            Ok(match request["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x1"),
                "eth_maxPriorityFeePerGas" => json!("0x2"),
                "eth_blockNumber" => json!("0xffffffffffff"),
                "eth_getTransactionCount" => {
                    assert_eq!(params[1], "pending");
                    json!(format!("0x{:x}", sent.lock().unwrap().len() * self.nonce_step))
                }
                "eth_estimateGas" => {
                    *estimates.lock().unwrap() += 1;
                    json!("0x10000")
                }
                "eth_getTransactionReceipt" if l2 => receipt.clone(),
                "eth_getTransactionReceipt" => {
                    if let Some(error) = &self.receipt_error {
                        return Err(error.clone());
                    }
                    if self.receipt_pending {
                        return Ok(Value::Null);
                    }
                    let mut result = receipt.clone();
                    result["type"] = json!("0x0");
                    result["transactionHash"] = params[0].clone();
                    if self.receipt_reverted {
                        result["status"] = json!("0x0");
                    }
                    result["logs"] = json!([]);
                    if let Some(success) = self.finalization_success {
                        let tx = sent.lock().unwrap().last().unwrap().clone();
                        if let Ok(call) =
                            WithdrawalPortal::finalizeWithdrawalTransactionCall::abi_decode_validate(
                                tx.input(),
                            )
                        {
                            let withdrawal =
                                Withdrawal { transaction: call.withdrawal, block_number: 0 };
                            let event = WithdrawalPortal::WithdrawalFinalized {
                                withdrawalHash: withdrawal.hash(),
                                success,
                            }
                            .encode_log_data();
                            let mut log = receipt["logs"][0].clone();
                            log["address"] = json!(self.finalization_emitter);
                            log["transactionHash"] = params[0].clone();
                            log["topics"] = json!(event.topics());
                            log["data"] = json!(event.data);
                            result["logs"] = json!([log]);
                        }
                    }
                    result
                }
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
                        block["baseFeePerGas"] = json!("0x2");
                        block["hash"] = receipt["blockHash"].clone();
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
                "eth_sendRawTransaction" => {
                    let raw: Bytes = serde_json::from_value(params[0].clone()).unwrap();
                    let tx = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
                    let hash = *tx.tx_hash();
                    sent.lock().unwrap().push(tx);
                    if let Some(error) = &self.broadcast_error {
                        return Err(error.clone());
                    }
                    json!(hash)
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
                    let target: Address = serde_json::from_value(params[0]["to"].clone()).unwrap();
                    if input[..4] == FaultProofPortal::disputeGameFactoryCall::SELECTOR
                        || input[..4] == FaultProofPortal::anchorStateRegistryCall::SELECTOR
                    {
                        assert_eq!(target, crate::NetworkPreset::BaseMainnet.resolve().portal);
                        if let Some(error) = &self.portal_error {
                            return Err(error.clone());
                        }
                        reply!(FaultProofPortal::disputeGameFactoryCall, self.portal_factory);
                        reply!(FaultProofPortal::anchorStateRegistryCall, self.portal_registry);
                    }
                    if self.fault {
                        if [
                            IDisputeGameFactory::gameCountCall::SELECTOR,
                            IDisputeGameFactory::gameAtIndexCall::SELECTOR,
                        ]
                        .contains(&input[..4].try_into().unwrap())
                            && target != self.portal_factory
                        {
                            return Err(
                                json!({"code":3,"message":"execution reverted: wrong factory"}),
                            );
                        }
                        if [
                            IAnchorStateRegistry::pausedCall::SELECTOR,
                            IAnchorStateRegistry::isGameBlacklistedCall::SELECTOR,
                            IAnchorStateRegistry::isGameRetiredCall::SELECTOR,
                            IAnchorStateRegistry::isGameProperCall::SELECTOR,
                            IAnchorStateRegistry::isGameRespectedCall::SELECTOR,
                            IAnchorStateRegistry::isGameFinalizedCall::SELECTOR,
                            IAnchorStateRegistry::disputeGameFinalityDelaySecondsCall::SELECTOR,
                        ]
                        .contains(&input[..4].try_into().unwrap())
                            && target != self.portal_registry
                        {
                            return Err(
                                json!({"code":3,"message":"execution reverted: wrong registry"}),
                            );
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
        fn assert_portal_reads(&self, expected: usize) {
            let requests = self.requests.lock().unwrap();
            for selector in [
                FaultProofPortal::disputeGameFactoryCall::SELECTOR,
                FaultProofPortal::anchorStateRegistryCall::SELECTOR,
            ] {
                let input = json!(Bytes::copy_from_slice(&selector));
                let count = requests
                    .iter()
                    .filter(|request| {
                        request["method"] == "eth_call" && request["params"][0]["input"] == input
                    })
                    .count();
                assert_eq!(count, expected, "portal address view must be read once per run");
            }
        }

        fn assert_proven_game(&self, index: u64) {
            let sent = self.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            let call = WithdrawalPortal::proveWithdrawalTransactionCall::abi_decode_validate(
                sent[0].input(),
            )
            .unwrap();
            assert_eq!(call.outputIndex, U256::from(index));
        }

        async fn run(&self) -> Result<Vec<WithdrawalOutcome>, WithdrawerError> {
            Withdrawer::new(self.config.clone()).run().await
        }

        async fn status(&self) -> Result<WithdrawalStatus, WithdrawerError> {
            let rpc = WithdrawalRpc::new(&self.config).unwrap();
            let receipt = serde_json::from_str(include_str!(
                "../fixtures/synthetic-deposit-withdrawal-receipt.json"
            ))
            .unwrap();
            let withdrawal = Withdrawal::from_receipt(&receipt).unwrap().remove(0);
            let signer = WithdrawalSigner::new(&self.config.signer).await.unwrap();
            let flow = Withdrawer::new(self.config.clone());
            let proofs = flow.resolve_proof_system(&rpc).await?;
            flow.status(&rpc, proofs, &withdrawal, signer.address()).await
        }

        fn assert_sent(&self, selector: [u8; 4], count: usize) {
            let sent = self.sent.lock().unwrap();
            assert_eq!(sent.len(), count, "unexpected number of signed submissions");
            for (nonce, tx) in sent.iter().enumerate() {
                assert_eq!(&tx.input()[..4], &selector);
                assert_eq!(tx.nonce(), nonce as u64);
                assert_eq!(tx.gas_limit(), 65536);
                assert_eq!(tx.to(), Some(self.config.network.portal));
                if selector == WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR {
                    let call =
                        WithdrawalPortal::proveWithdrawalTransactionCall::abi_decode_validate(
                            tx.input(),
                        )
                        .unwrap();
                    let fixture: Value = serde_json::from_str(include_str!(
                        "../fixtures/base-sepolia-withdrawal-proof.json"
                    ))
                    .unwrap();
                    assert_eq!(
                        call.outputRootProof.latestBlockhash,
                        serde_json::from_value::<B256>(fixture["header"]["hash"].clone()).unwrap()
                    );
                    assert!(!call.withdrawalProof.is_empty());
                    assert_eq!(call.outputIndex, U256::ZERO);
                }
                assert_eq!(
                    tx.signature().recover_address_from_prehash(&tx.signature_hash()).unwrap(),
                    alloy_signer_local::PrivateKeySigner::from_bytes(&B256::repeat_byte(1))
                        .unwrap()
                        .address()
                );
            }
        }
    }

    #[tokio::test]
    async fn preset_uses_portal_contracts_for_proving_and_finalizing() {
        for (proven, selector) in [
            (0, WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR),
            (100, WithdrawalPortal::finalizeWithdrawalTransactionCall::SELECTOR),
        ] {
            let mut server = Scenario {
                proven,
                portal_factory: Address::repeat_byte(4),
                portal_registry: Address::repeat_byte(5),
                ..Default::default()
            }
            .start()
            .await;
            server.config.network.proofs = crate::NetworkPreset::BaseMainnet.resolve().proofs;
            let outcomes =
                server.run().await.expect("preset must use the portal's proof contracts");
            assert!(outcomes[0].transaction_hash.is_some());
            server.assert_sent(selector, 1);
            server.assert_portal_reads(1);
        }
    }

    #[tokio::test]
    async fn custom_contract_mismatch_stops_before_signing() {
        for (factory, registry, dry) in [(4, 3, false), (2, 5, false), (4, 5, false), (4, 5, true)]
        {
            let mut server = Scenario {
                portal_factory: Address::repeat_byte(factory),
                portal_registry: Address::repeat_byte(registry),
                dry,
                ..Default::default()
            }
            .start()
            .await;
            // An invalid signing key makes signer construction observable without a signing mock.
            server.config.signer = crate::SignerSource::PrivateKey(crate::Secret(B256::ZERO));
            let error = server.run().await.unwrap_err();
            assert!(
                matches!(error, WithdrawerError::ProofContractsMismatch { .. }),
                "expected a contract mismatch before signer construction, got {error}"
            );
            let message = error.to_string();
            for (configured, reported) in [(2, factory), (3, registry)] {
                if configured != reported {
                    assert!(
                        message.contains(&Address::repeat_byte(configured).to_string()),
                        "missing configured address: {message}"
                    );
                    assert!(
                        message.contains(&Address::repeat_byte(reported).to_string()),
                        "missing portal address: {message}"
                    );
                }
            }
            assert!(server.sent.lock().unwrap().is_empty());
            assert!(server.requests.lock().unwrap().iter().all(|request| !matches!(
                request["method"].as_str(),
                Some("eth_estimateGas" | "eth_getTransactionCount" | "eth_sendRawTransaction")
            )));
        }
    }

    #[tokio::test]
    async fn portal_contract_failure_keeps_a_redacted_source_before_signing() {
        let mut server = Scenario {
            portal_error: Some(json!({
                "code": -32000,
                "message": "https://user:portal-secret@rpc.invalid/key",
                "data": "portal-secret",
            })),
            ..Default::default()
        }
        .start()
        .await;
        server.config.signer = crate::SignerSource::PrivateKey(crate::Secret(B256::ZERO));
        let error = server.run().await.unwrap_err();
        assert!(matches!(error, WithdrawerError::Request(_)));
        assert!(error.source().is_some());
        let mut diagnostic = String::new();
        let mut source = Some(&error as &dyn StdError);
        while let Some(error) = source {
            diagnostic.push_str(&format!("{error} {error:?} "));
            source = error.source();
        }
        assert!(diagnostic.contains("-32000"));
        assert!(!diagnostic.contains("portal-secret"), "credentials leaked: {diagnostic}");
        assert!(server.sent.lock().unwrap().is_empty());
        assert_eq!(*server.estimates.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn zero_portal_contracts_fail_before_signing_including_dry_runs() {
        for (factory, registry, contract) in [
            (
                Address::ZERO,
                Address::repeat_byte(3),
                FaultProofPortal::disputeGameFactoryCall::SIGNATURE,
            ),
            (
                Address::repeat_byte(2),
                Address::ZERO,
                FaultProofPortal::anchorStateRegistryCall::SIGNATURE,
            ),
        ] {
            for dry in [false, true] {
                let mut server = Scenario {
                    portal_factory: factory,
                    portal_registry: registry,
                    dry,
                    ..Default::default()
                }
                .start()
                .await;
                server.config.signer = crate::SignerSource::PrivateKey(crate::Secret(B256::ZERO));
                let error = server.run().await.unwrap_err();
                assert!(
                    matches!(error, WithdrawerError::ZeroPortalProofContract { contract: actual } if actual == contract)
                );
                assert!(server.sent.lock().unwrap().is_empty());
                assert_eq!(*server.estimates.lock().unwrap(), 0);
            }
        }
    }

    #[tokio::test]
    async fn preset_dry_run_resolves_contracts_before_preparing() {
        let mut server = Scenario {
            dry: true,
            portal_factory: Address::repeat_byte(4),
            portal_registry: Address::repeat_byte(5),
            ..Default::default()
        }
        .start()
        .await;
        server.config.network.proofs = crate::NetworkPreset::BaseMainnet.resolve().proofs;
        let outcomes = server.run().await.unwrap();
        assert_eq!(outcomes[0].status, WithdrawalStatus::Prove);
        assert!(outcomes[0].transaction_hash.is_none());
        assert_eq!(*server.estimates.lock().unwrap(), 1);
        assert!(server.sent.lock().unwrap().is_empty());
        server.assert_portal_reads(1);
    }

    #[tokio::test]
    async fn portal_contracts_are_read_once_per_receipt_run() {
        let server =
            Scenario { proven: 100, second_withdrawal: true, ..Default::default() }.start().await;
        for run in 1..=2 {
            let outcomes = server.run().await.unwrap();
            assert_eq!(outcomes.len(), 2);
            server.assert_sent(
                WithdrawalPortal::finalizeWithdrawalTransactionCall::SELECTOR,
                2 * run,
            );
            server.assert_portal_reads(run);
        }
    }

    #[tokio::test]
    async fn proves_then_finalizes_both_proof_systems() {
        for fault in [false, true] {
            for (proven, status, selector) in [
                (
                    0,
                    WithdrawalStatus::Prove,
                    WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR,
                ),
                (
                    100,
                    WithdrawalStatus::Finalize,
                    WithdrawalPortal::finalizeWithdrawalTransactionCall::SELECTOR,
                ),
            ] {
                let server = Scenario { fault, proven, ..Default::default() }.start().await;
                assert_eq!(server.status().await.unwrap(), status);
                assert_eq!(*server.estimates.lock().unwrap(), 0);
                assert!(server.sent.lock().unwrap().is_empty());
                let outcomes = server.run().await.unwrap();
                assert_eq!(outcomes[0].status, status);
                assert!(outcomes[0].transaction_hash.is_some());
                server.assert_sent(selector, 1);
                server.assert_portal_reads(if fault { 2 } else { 0 });
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
            assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Prove);
            let outcomes = server.run().await.unwrap();
            assert_eq!(
                outcomes[0].status,
                WithdrawalStatus::Prove,
                "invalidated proof was retained"
            );
            server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
        }
    }

    #[tokio::test]
    async fn paused_contracts_wait_without_replacing_proofs() {
        for fault in [false, true] {
            for proven in [0, 100] {
                let server =
                    Scenario { fault, paused: true, proven, ..Default::default() }.start().await;
                assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Paused);
                let outcomes = server.run().await.unwrap();
                assert_eq!(outcomes[0].status, WithdrawalStatus::Paused);
                assert!(outcomes[0].transaction_hash.is_none());
                assert_eq!(*server.estimates.lock().unwrap(), 0);
                assert!(server.sent.lock().unwrap().is_empty());
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
            assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Prove);
            let outcomes = server.run().await.unwrap();
            assert_eq!(outcomes[0].status, WithdrawalStatus::Prove);
            server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
        }
    }

    #[tokio::test]
    async fn legacy_rpc_failure_is_not_a_deleted_output() {
        let server = Scenario { fault: false, proven: 100, output_error: Some(json!({"code":-32000,"message":"upstream unavailable at https://user:password@host.invalid/key"})), ..Default::default() }.start().await;
        assert!(server.status().await.is_err(), "non-revert RPC error was ignored");
        let error = server.run().await.expect_err("non-revert RPC error was ignored");
        assert!(!format!("{error:?}").contains("password"));
        let source = std::error::Error::source(&error).expect("RPC error must retain its source");
        assert!(!format!("{source:?}").contains("password"));
        assert!(server.sent.lock().unwrap().is_empty());
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
            assert_eq!(
                server.status().await.unwrap(),
                WithdrawalStatus::NotReady { ready_at, waiting_for_game }
            );
            let outcomes = server.run().await.unwrap();
            assert_eq!(
                outcomes[0].status,
                WithdrawalStatus::NotReady { ready_at, waiting_for_game }
            );
            assert!(server.sent.lock().unwrap().is_empty());
            assert_eq!(*server.estimates.lock().unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn portal_check_remains_authoritative() {
        let server =
            Scenario { proven: 100, reject_check: true, ..Default::default() }.start().await;
        assert!(server.status().await.is_err(), "portal refusal was ignored");
        assert!(server.run().await.is_err(), "portal refusal was ignored");
        assert!(server.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn finalized_withdrawal_never_prepares_a_transaction() {
        let server = Scenario { finalized: true, ..Default::default() }.start().await;
        assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Finalized);
        let outcomes = server.run().await.unwrap();
        assert_eq!(outcomes[0].status, WithdrawalStatus::Finalized);
        assert!(outcomes[0].transaction_hash.is_none());
        assert_eq!(*server.estimates.lock().unwrap(), 0);
        assert!(server.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dry_run_estimates_without_signing_or_submission() {
        for fault in [false, true] {
            for proven in [0, 100] {
                let server =
                    Scenario { fault, proven, dry: true, ..Default::default() }.start().await;
                let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
                let writer = capture.clone();
                let subscriber = tracing_subscriber::fmt()
                    .with_ansi(false)
                    .with_writer(move || writer.clone())
                    .finish();
                let outcomes = server.run().with_subscriber(subscriber).await.unwrap();
                let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
                for field in [
                    "target=",
                    "calldata_length=",
                    "selector=",
                    "gas_limit=65536",
                    "maximum_gas_cost=",
                ] {
                    assert!(logs.contains(field), "missing dry-run field {field}: {logs}");
                }
                assert_eq!(outcomes.len(), 1);
                assert!(outcomes[0].transaction_hash.is_none());
                assert_eq!(*server.estimates.lock().unwrap(), 1);
                assert!(server.sent.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn receipt_warnings_do_not_expose_server_supplied_secrets() {
        let mut server = Scenario {
            proven: 100,
            receipt_error: Some(json!({
                "code": -32005, "message": "rate limit at https://user:password@host.invalid/key",
                "data": "request token secret-value"
            })),
            ..Default::default()
        }
        .start()
        .await;
        server.config.timeout = Duration::from_secs(1);
        let capture = LogCapture(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber =
            tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let result = server.run().with_subscriber(subscriber).await;
        let error = result.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = &error else {
            panic!("missing broadcast hash: {error:?}");
        };
        let transaction_hash = &transaction_hashes[0];
        assert_eq!(*transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(matches!(**source, WithdrawerError::Timeout(_)), "{source:?}");
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        for secret in ["password", "host.invalid", "secret-value"] {
            assert!(!logs.contains(secret), "server-supplied secret leaked in warning: {logs}");
        }
    }

    #[tokio::test]
    async fn broadcast_failure_retains_signed_hash_and_redacted_source() {
        let server = Scenario {
            broadcast_error: Some(json!({
                "code": -32602,
                "message": "invalid at https://user:password@host.invalid/key",
                "data": "secret-value"
            })),
            ..Default::default()
        }
        .start()
        .await;
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = &error else {
            panic!("missing signed hash: {error:?}");
        };
        let transaction_hash = &transaction_hashes[0];
        assert_eq!(*transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(matches!(**source, WithdrawerError::TxManager(_)), "{source:?}");
        assert!(std::error::Error::source(source.as_ref()).is_some());
        for secret in ["password", "host.invalid", "secret-value"] {
            assert!(!format!("{error} {error:?}").contains(secret));
        }
    }

    #[tokio::test]
    async fn cancelled_initial_publish_refreshes_pending_nonce_for_next_withdrawal() {
        let server = Scenario { initial_broadcast_hang: true, ..Default::default() }.start().await;
        let flow = Withdrawer::new(server.config.clone());
        let rpc = WithdrawalRpc::new(&server.config).unwrap();
        let signer = WithdrawalSigner::new(&server.config.signer).await.unwrap();
        let sender = signer.address();
        let published = BroadcastJournal::default();
        let manager = flow.manager(signer, published.clone()).await.unwrap();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                flow.submit(&rpc, sender, Some(&manager), Bytes::new(), None, &published),
            )
            .await
            .is_err()
        );
        assert_eq!(published.hashes().len(), 1);
        published.clear();
        flow.submit(&rpc, sender, Some(&manager), Bytes::new(), None, &published).await.unwrap();
        let sent = server.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].nonce(), 0);
    }

    #[tokio::test]
    async fn serialized_withdrawals_refresh_pending_nonce_advanced_by_another_sender() {
        let server =
            Scenario { proven: 100, nonce_step: 3, second_withdrawal: true, ..Default::default() }
                .start()
                .await;
        let outcomes = server.run().await.unwrap();
        assert_eq!(outcomes.len(), 2);
        let sent = server.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].nonce(), 0);
        assert_eq!(sent[1].nonce(), 3);
    }

    #[tokio::test]
    async fn gas_limit_is_a_floor_for_both_dry_runs_and_submissions() {
        for minimum in [1, 100_000] {
            let mut server = Scenario::default().start().await;
            server.config.gas_limit = Some(minimum);
            let rpc = WithdrawalRpc::new(&server.config).unwrap();
            let prepared = PreparedWithdrawalTransaction::prepare(
                &server.config,
                &rpc,
                Address::ZERO,
                server.config.network.portal,
                Bytes::new(),
            )
            .await
            .unwrap();
            assert_eq!(prepared.candidate.gas_limit, minimum.max(65536));
            server.run().await.unwrap();
            assert_eq!(server.sent.lock().unwrap()[0].gas_limit(), prepared.candidate.gas_limit);
            assert_eq!(*server.estimates.lock().unwrap(), 2);
        }
    }

    #[tokio::test]
    async fn shared_deadline_retains_hash_during_broadcast() {
        let mut server = Scenario { broadcast_hang: true, ..Default::default() }.start().await;
        server.config.timeout = Duration::from_secs(1);
        server.config.max_retries = 2;
        server.config.retry_delay = Duration::from_millis(1);
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = error else {
            panic!("missing signed hash: {error:?}");
        };
        let transaction_hash = transaction_hashes[0];
        assert_eq!(transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(matches!(*source, WithdrawerError::Timeout(_)), "{source:?}");
    }

    #[tokio::test]
    async fn receipt_failure_retains_broadcast_hash_and_redacted_source() {
        let server = Scenario {
            receipt_error: Some(json!({
                "code": -32602,
                "message": "invalid at https://user:password@host.invalid/key",
                "data": "secret-value"
            })),
            ..Default::default()
        }
        .start()
        .await;
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = &error else {
            panic!("missing broadcast hash: {error:?}");
        };
        let transaction_hash = &transaction_hashes[0];
        assert_eq!(*transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(matches!(**source, WithdrawerError::Timeout(_)), "{source:?}");
        assert!(std::error::Error::source(source.as_ref()).is_some());
        for secret in ["password", "host.invalid", "secret-value"] {
            assert!(!format!("{error} {error:?}").contains(secret));
        }
    }

    #[tokio::test]
    async fn shared_deadline_retains_hash_while_receipt_is_pending() {
        let mut server = Scenario { receipt_pending: true, ..Default::default() }.start().await;
        server.config.timeout = Duration::from_secs(1);
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = error else {
            panic!("missing broadcast hash: {error:?}");
        };
        let transaction_hash = transaction_hashes[0];
        assert_eq!(transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(matches!(*source, WithdrawerError::Timeout(_)), "{source:?}");
    }

    #[tokio::test]
    async fn reverted_receipt_retains_broadcast_hash() {
        let server = Scenario { receipt_reverted: true, ..Default::default() }.start().await;
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = error else {
            panic!("missing broadcast hash: {error:?}");
        };
        let transaction_hash = transaction_hashes[0];
        assert_eq!(transaction_hash, *server.sent.lock().unwrap()[0].tx_hash());
        assert!(
            matches!(*source, WithdrawerError::Rpc(WithdrawalRpcError::Reverted(hash)) if hash == transaction_hash)
        );
    }

    #[tokio::test]
    async fn operation_deadline_after_fee_bump_retains_every_hash() {
        let mut server = Scenario {
            proven: 100,
            receipt_pending: true,
            finalization_success: None,
            ..Default::default()
        }
        .start()
        .await;
        server.config.timeout = Duration::from_secs(3);
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = error else {
            panic!("missing hashes: {error:?}");
        };
        assert!(matches!(*source, WithdrawerError::Timeout(_)));
        let sent = server.sent.lock().unwrap();
        assert!(sent.len() >= 2, "withdrawal must resubmit with bumped fees");
        assert_eq!(transaction_hashes, sent.iter().map(|tx| *tx.tx_hash()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn manager_send_timeout_after_fee_bump_retains_every_hash() {
        let mut server = Scenario { receipt_pending: true, ..Default::default() }.start().await;
        server.config.tx_manager.tx_send_timeout = Duration::from_millis(1500);
        let error = server.run().await.unwrap_err();
        let WithdrawerError::Unconfirmed { transaction_hashes, source } = error else {
            panic!("missing hashes: {error:?}");
        };
        assert!(matches!(*source, WithdrawerError::TxManager(TxManagerError::SendTimeout)));
        let sent = server.sent.lock().unwrap();
        assert!(sent.len() >= 2);
        assert_eq!(transaction_hashes, sent.iter().map(|tx| *tx.tx_hash()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn finalization_reports_the_target_call_result() {
        for fault in [false, true] {
            for success in [false, true] {
                let server = Scenario {
                    fault,
                    proven: 100,
                    finalization_success: Some(success),
                    ..Default::default()
                }
                .start()
                .await;
                let result = server.run().await;
                let transaction_hash = *server.sent.lock().unwrap()[0].tx_hash();
                assert!(
                    server.sent.lock().unwrap()[0].is_eip1559(),
                    "finalization must use tx-manager EIP-1559 submission"
                );
                if success {
                    assert_eq!(result.unwrap()[0].transaction_hash, Some(transaction_hash));
                } else {
                    let WithdrawerError::TargetCallFailed {
                        withdrawal_hash,
                        transaction_hash: actual,
                        ..
                    } = result.unwrap_err()
                    else {
                        panic!("target failure was not reported");
                    };
                    assert_eq!(actual, transaction_hash);
                    let receipt = serde_json::from_str(include_str!(
                        "../fixtures/synthetic-deposit-withdrawal-receipt.json"
                    ))
                    .unwrap();
                    assert_eq!(
                        withdrawal_hash,
                        Withdrawal::from_receipt(&receipt).unwrap()[0].hash()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn finalization_requires_an_event_from_the_configured_portal() {
        for scenario in [
            Scenario { finalization_success: None, ..Default::default() },
            Scenario { finalization_emitter: Address::repeat_byte(0xff), ..Default::default() },
        ] {
            let server = Scenario { proven: 100, ..scenario }.start().await;
            let error = server.run().await.unwrap_err();
            assert!(
                matches!(error, WithdrawerError::MissingFinalizationEvent { transaction_hash, .. } if transaction_hash == *server.sent.lock().unwrap()[0].tx_hash())
            );
            assert!(
                server.sent.lock().unwrap()[0].is_eip1559(),
                "finalization must use tx-manager EIP-1559 submission"
            );
        }
    }

    #[tokio::test]
    async fn manager_network_timeout_is_not_the_operation_deadline() {
        let mut server = Scenario { hang: true, ..Default::default() }.start().await;
        server.config.tx_manager.network_timeout = Duration::from_millis(100);
        server.config.max_retries = 3;
        server.config.retry_delay = Duration::from_millis(1);
        let flow = Withdrawer::new(server.config.clone());
        let signer = WithdrawalSigner::new(&server.config.signer).await.unwrap();
        let error = flow.manager(signer, BroadcastJournal::default()).await.unwrap_err();
        assert!(matches!(error, WithdrawerError::NetworkTimeout(_)), "{error:?}");
        assert_eq!(error.to_string(), "transaction manager network timeout elapsed");
    }

    #[tokio::test]
    async fn operation_timeout_is_typed_and_bounded() {
        let mut server = Scenario { hang: true, ..Default::default() }.start().await;
        server.config.timeout = Duration::from_millis(50);
        server.config.max_retries = 1;
        let result = server.run().await;
        assert!(
            matches!(result, Err(WithdrawerError::Timeout(_))),
            "missing shared operation deadline: {result:?}"
        );
        assert!(server.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_covering_output_never_submits() {
        for fault in [false, true] {
            let server = Scenario { fault, no_coverage: true, ..Default::default() }.start().await;
            let result = server.run().await;
            assert!(
                matches!(result, Err(WithdrawerError::NoCoverage { .. })),
                "missing no-coverage error: {result:?}"
            );
            assert!(server.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn pending_finalization_does_not_complete_a_withdrawal() {
        for fault in [false, true] {
            let server =
                Scenario { fault, proven: 100, pending_finalized: true, ..Default::default() }
                    .start()
                    .await;
            assert_eq!(server.status().await.unwrap(), WithdrawalStatus::Finalize);
            let outcomes = server.run().await.unwrap();
            assert_eq!(outcomes[0].status, WithdrawalStatus::Finalize);
            assert!(outcomes[0].transaction_hash.is_some());
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
            let proofs = flow.resolve_proof_system(&rpc).await.unwrap();
            assert_eq!(
                flow.status(&rpc, proofs, &withdrawal, Address::ZERO).await.unwrap(),
                expected
            );
            assert!(server.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn proof_must_authenticate_the_published_claim() {
        let server =
            Scenario { fault: false, wrong_root: true, ..Default::default() }.start().await;
        assert!(matches!(
            server.run().await,
            Err(WithdrawerError::Withdrawal(WithdrawalError::OutputRootMismatch { .. }))
        ));
        assert_eq!(*server.estimates.lock().unwrap(), 0);
        assert!(server.sent.lock().unwrap().is_empty());

        for valid_older_game in [true, false] {
            let server = Scenario {
                game_count: 2,
                latest_wrong_root: true,
                wrong_root: !valid_older_game,
                ..Default::default()
            }
            .start()
            .await;
            let result = server.run().await;
            if valid_older_game {
                assert_eq!(result.unwrap()[0].status, WithdrawalStatus::Prove);
                server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
            } else {
                assert!(matches!(result, Err(WithdrawerError::NoCoverage { .. })));
                assert_eq!(*server.estimates.lock().unwrap(), 0);
                assert!(server.sent.lock().unwrap().is_empty());
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
            assert_eq!(server.run().await.unwrap()[0].status, WithdrawalStatus::Prove);
            server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
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
        server.run().await.unwrap();
        server.assert_proven_game(1);
    }

    #[tokio::test]
    async fn game_search_uses_the_oldest_pending_covering_game() {
        let server =
            Scenario { game_count: 3, game_finalized: false, ..Default::default() }.start().await;
        server.run().await.unwrap();
        server.assert_proven_game(0);
    }

    #[tokio::test]
    async fn game_search_reads_pending_eligibility_and_roots_only_when_verifying() {
        let server =
            Scenario { game_count: 4, game_finalized: false, ..Default::default() }.start().await;
        server.run().await.unwrap();
        server.assert_proven_game(0);
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
        server.run().await.unwrap();
        server.assert_proven_game(0);
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
            server.run().await.unwrap();
            server.assert_proven_game(2);
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
        let error = server.run().with_subscriber(subscriber).await.unwrap_err();
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
        assert_eq!(*server.estimates.lock().unwrap(), 0);
        assert!(server.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn game_search_skips_a_newer_noncovering_game() {
        let server = Scenario { skip_latest: true, ..Default::default() }.start().await;
        assert_eq!(server.run().await.unwrap()[0].status, WithdrawalStatus::Prove);
        server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
    }

    #[tokio::test]
    async fn game_search_uses_a_still_respected_earlier_game_type() {
        let server = Scenario { other_game_type: true, skip_latest: true, ..Default::default() }
            .start()
            .await;
        assert_eq!(server.run().await.unwrap()[0].status, WithdrawalStatus::Prove);
        server.assert_sent(WithdrawalPortal::proveWithdrawalTransactionCall::SELECTOR, 1);
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
            assert!(matches!(server.run().await, Err(WithdrawerError::NoCoverage { .. })));
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
            assert!(server.sent.lock().unwrap().is_empty());
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

    #[tokio::test]
    async fn contract_decode_errors_withhold_response_bytes() {
        let server = Scenario { malformed_response: true, ..Default::default() }.start().await;
        for error in [server.status().await.unwrap_err(), server.run().await.unwrap_err()] {
            assert!(matches!(error, WithdrawerError::Decode { .. }));
            let diagnostic = format!("{error} {error:?}");
            assert!(diagnostic.contains(FaultProofPortal::disputeGameFactoryCall::SIGNATURE));
            assert!(diagnostic.contains("TypeCheckFail"));
            assert!(!diagnostic.contains("61616161"), "response bytes leaked: {diagnostic}");
        }
    }

    #[tokio::test]
    async fn receipt_withdrawals_are_sequential_and_independent() {
        let server =
            Scenario { proven: 100, second_withdrawal: true, ..Default::default() }.start().await;
        let outcomes = server.run().await.unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_ne!(outcomes[0].withdrawal_hash, outcomes[1].withdrawal_hash);
        server.assert_sent(WithdrawalPortal::finalizeWithdrawalTransactionCall::SELECTOR, 2);
    }
}
