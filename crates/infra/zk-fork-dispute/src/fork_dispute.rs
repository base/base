//! ZK fork dispute workflow orchestration.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{Provider, RootProvider};
use base_challenger::{ChallengeSubmitter, DisputeIntent};
use base_proof_contracts::{AggregateVerifierClient, AggregateVerifierContractClient, GameStatus};
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use eyre::{Result, bail, eyre};
use tracing::info;

use crate::{checkpoint::Checkpoint, config::Config};

/// Runner for the ZK fork dispute workflow.
#[derive(Debug)]
pub struct ZkForkDispute;

impl ZkForkDispute {
    /// Runs the full fork-dispute workflow with the given config.
    pub async fn run(config: Config) -> Result<()> {
        let verifier = AggregateVerifierContractClient::new(config.l1_rpc_url.clone())?;

        let checkpoint = if config.patch_invalid_game {
            Checkpoint::patch(&config, &verifier).await?
        } else {
            Checkpoint::find(&config, &verifier).await?
        };

        let l1_head = verifier.l1_head(config.game_address).await?;
        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let chain_id = provider.get_chain_id().await?;
        let submitter = ChallengeSubmitter::new(
            SimpleTxManager::new(
                provider,
                SignerConfig::local(config.private_key.clone()),
                Self::tx_manager_config(),
                chain_id,
                Arc::new(NoopTxMetrics),
            )
            .await?,
        );

        let challenger = submitter.sender_address();
        let before_zk = verifier.zk_prover(config.game_address).await?;
        let before_tee = verifier.tee_prover(config.game_address).await?;
        let before_countered = verifier.countered_index(config.game_address).await?;
        // Fail fast before spending hours on proof generation.
        Self::validate_dispute_preconditions(&config, before_zk, before_tee, before_countered)?;

        let proof_bytes =
            checkpoint.request_proof(&config, submitter.sender_address(), l1_head).await?;

        let tx_hash = submitter
            .submit_dispute(
                config.game_address,
                proof_bytes,
                checkpoint.index,
                checkpoint.expected_root,
                config.intent,
            )
            .await?;

        let status = verifier.status(config.game_address).await?;
        if status != GameStatus::InProgress {
            bail!("expected game to remain in progress after dispute tx, got {status}");
        }
        Self::assert_dispute_effect(&config, &verifier, challenger, checkpoint.index).await?;

        info!(
            intent = ?config.intent,
            game = %config.game_address,
            invalid_index = checkpoint.index,
            tx_hash = %tx_hash,
            "submitted dispute transaction"
        );
        Ok(())
    }

    fn validate_dispute_preconditions(
        config: &Config,
        before_zk: Address,
        before_tee: Address,
        before_countered: u64,
    ) -> Result<()> {
        match config.intent {
            DisputeIntent::Nullify => {
                if before_zk == Address::ZERO {
                    bail!("nullify requires an existing ZK prover on the game");
                }
            }
            DisputeIntent::Challenge => {
                if before_tee == Address::ZERO {
                    bail!("challenge requires an existing TEE prover on the game");
                }
                if before_countered != 0 {
                    bail!("challenge requires an unchallenged game");
                }
            }
        }
        Ok(())
    }

    async fn assert_dispute_effect(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        challenger: Address,
        invalid_index: u64,
    ) -> Result<()> {
        match config.intent {
            DisputeIntent::Nullify => {
                let after = verifier.zk_prover(config.game_address).await?;
                if after != Address::ZERO {
                    bail!("expected ZK prover to be cleared after nullify, got {after}");
                }
            }
            DisputeIntent::Challenge => {
                let after_zk = verifier.zk_prover(config.game_address).await?;
                let after_countered = verifier.countered_index(config.game_address).await?;
                if after_zk != challenger {
                    bail!("expected ZK prover to be challenger {challenger}, got {after_zk}");
                }
                let expected_countered =
                    invalid_index.checked_add(1).ok_or_else(|| eyre!("invalid index overflow"))?;
                if after_countered != expected_countered {
                    bail!("expected countered index {expected_countered}, got {after_countered}");
                }
            }
        }
        Ok(())
    }

    fn tx_manager_config() -> TxManagerConfig {
        TxManagerConfig {
            num_confirmations: 1,
            resubmission_timeout: Duration::from_secs(10),
            receipt_query_interval: Duration::from_secs(1),
            tx_send_timeout: Duration::from_secs(180),
            tx_not_in_mempool_timeout: Duration::from_secs(30),
            confirmation_timeout: Duration::from_secs(120),
            ..Default::default()
        }
    }
}
