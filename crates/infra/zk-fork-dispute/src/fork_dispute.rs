//! ZK fork dispute workflow orchestration.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, U256};
use alloy_provider::{Provider, RootProvider};
use base_challenger::{ChallengeSubmitter, DisputeIntent};
use base_proof_contracts::{AggregateVerifierClient, AggregateVerifierContractClient, GameStatus};
use base_tx_manager::{NoopTxMetrics, SignerConfig, SimpleTxManager, TxManagerConfig};
use eyre::{Context, Result, bail, eyre};
use tracing::info;

use crate::{checkpoint::Checkpoint, config::Config};

/// Runner for the ZK fork dispute workflow.
#[derive(Debug)]
pub struct ZkForkDispute;

impl ZkForkDispute {
    /// Runs the full fork-dispute workflow with the given config.
    pub async fn run(config: Config) -> Result<()> {
        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let verifier = AggregateVerifierContractClient::new(provider.clone());

        let status = verifier.status(config.game_address).await?;
        let before_zk = verifier.zk_prover(config.game_address).await?;
        let before_tee = verifier.tee_prover(config.game_address).await?;
        let before_countered = verifier.countered_index(config.game_address).await?;
        let intent = Self::resolve_intent(config.intent, before_zk, before_tee)?;
        // Validate before mutating the fork so incompatible games are not patched.
        Self::validate_dispute_preconditions(
            intent,
            status,
            before_zk,
            before_tee,
            before_countered,
        )?;

        let challenger = config.private_key.address();
        provider
            .client()
            .request::<_, ()>(
                "anvil_setBalance",
                (challenger, U256::from(1_000_000_000_000_000_000_000u128)),
            )
            .await
            .context(
                "anvil_setBalance failed; ensure BASE_ZK_FORK_L1_RPC_URL points to an Anvil fork",
            )?;

        let checkpoint = if config.patch_invalid_game {
            Checkpoint::patch(&config, &verifier).await?
        } else {
            Checkpoint::find(&config, &verifier).await?
        };

        let l1_head = verifier.l1_head(config.game_address).await?;
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

        let game_l2_block_number = verifier.game_info(config.game_address).await?.l2_block_number;
        let zk_artifact_hash =
            verifier.proof_artifacts(config.game_address).await?.zk_artifact_hash();
        let proof_bytes = checkpoint
            .request_proof(&config, challenger, l1_head, game_l2_block_number, zk_artifact_hash)
            .await?;

        let tx_hash = submitter
            .submit_dispute(
                config.game_address,
                proof_bytes,
                checkpoint.index,
                checkpoint.expected_root,
                intent,
            )
            .await?;
        info!(
            intent = ?intent,
            game = %config.game_address,
            invalid_index = checkpoint.index,
            tx_hash = %tx_hash,
            "submitted dispute transaction"
        );

        let status = verifier.status(config.game_address).await?;
        if status != GameStatus::InProgress {
            bail!("expected game to remain in progress after dispute tx, got {status}");
        }
        Self::assert_dispute_effect(intent, &config, &verifier, challenger, checkpoint.index)
            .await?;

        Ok(())
    }

    fn resolve_intent(
        configured: Option<DisputeIntent>,
        before_zk: Address,
        before_tee: Address,
    ) -> Result<DisputeIntent> {
        if let Some(intent) = configured {
            return Ok(intent);
        }
        // Normal proposers create TEE-backed games; default ZK flow is challenge.
        if before_tee != Address::ZERO && before_zk == Address::ZERO {
            return Ok(DisputeIntent::Challenge);
        }
        if before_zk != Address::ZERO {
            return Ok(DisputeIntent::Nullify);
        }
        bail!(
            "could not infer dispute intent (tee_prover={before_tee}, zk_prover={before_zk}); \
             pass --dispute-intent challenge|nullify"
        )
    }

    fn validate_dispute_preconditions(
        intent: DisputeIntent,
        status: GameStatus,
        before_zk: Address,
        before_tee: Address,
        before_countered: u64,
    ) -> Result<()> {
        if status != GameStatus::InProgress {
            bail!("dispute requires an InProgress game, got {status}");
        }
        if before_countered != 0 {
            bail!(
                "game already challenged (countered_index={before_countered}); \
                 fraudulent-ZK nullify (countered_index - 1) is not implemented"
            );
        }
        match intent {
            DisputeIntent::Nullify => {
                if before_zk == Address::ZERO {
                    bail!("nullify requires an existing ZK prover on the game");
                }
            }
            DisputeIntent::Challenge => {
                if before_tee == Address::ZERO {
                    bail!("challenge requires an existing TEE prover on the game");
                }
                if before_zk != Address::ZERO {
                    bail!("challenge requires no existing ZK prover on the game");
                }
            }
        }
        Ok(())
    }

    async fn assert_dispute_effect(
        intent: DisputeIntent,
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        challenger: Address,
        invalid_index: u64,
    ) -> Result<()> {
        match intent {
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
