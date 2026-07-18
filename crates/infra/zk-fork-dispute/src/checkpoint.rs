//! Invalid intermediate-root checkpoint selection, Anvil patching, and proving.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use base_challenger::ChallengerProofAdapter;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, encode_extra_data,
};
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    GetProofRequest, ProofStatus, SnarkPlonkProofRequest, ZkProofRequest, ZkVm,
};
use eyre::{Context, Result, bail, eyre};
use tracing::info;

use crate::config::Config;

/// Checkpoint that the workflow will prove and dispute.
#[derive(Debug, Clone, Copy)]
pub struct Checkpoint {
    /// 0-based invalid intermediate index.
    pub index: u64,
    /// Inclusive L2 start block for the proof range.
    pub start_block: u64,
    /// Number of L2 blocks to prove.
    pub block_count: u64,
    /// Canonical output root expected by the dispute call.
    pub expected_root: B256,
}

impl Checkpoint {
    /// Exclusive end block of the proof range.
    ///
    /// Unchecked add is safe: values are only constructed via [`Self::from_roots`],
    /// which validates the sum with `checked_add`.
    pub const fn target_block(self) -> u64 {
        self.start_block + self.block_count
    }

    /// Builds a checkpoint by mutating an intermediate root on the Anvil fork.
    pub async fn patch(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
    ) -> Result<Self> {
        let roots = verifier.intermediate_output_roots(config.game_address).await?;
        if roots.is_empty() {
            bail!(
                "game {} has no intermediate output roots; pass BASE_ZK_FORK_GAME_ADDRESS or BASE_ZK_FORK_GAME_INDEX",
                config.game_address
            );
        }

        let index = config.invalid_index.unwrap_or(0);
        let root_index =
            usize::try_from(index).map_err(|_| eyre!("invalid index does not fit usize"))?;
        let expected_root = *roots
            .get(root_index)
            .ok_or_else(|| eyre!("invalid index {index} out of range {}", roots.len()))?;
        let mut patched_root = expected_root;
        *patched_root.0.last_mut().expect("B256 is non-empty") ^= 1;

        AnvilPatch::apply(config, verifier, &roots, index, patched_root).await?;

        // Dispute still claims the pre-patch root as the canonical value.
        let starting_block = verifier.starting_block_number(config.game_address).await?;
        Self::from_roots(
            starting_block,
            Self::infer_interval(config.game_address, verifier, starting_block, roots.len())
                .await?,
            index,
            &roots,
            expected_root,
        )
    }

    /// Finds an already-invalid intermediate root by comparing on-chain vs canonical.
    pub async fn find(config: &Config, verifier: &AggregateVerifierContractClient) -> Result<Self> {
        let roots = verifier.intermediate_output_roots(config.game_address).await?;
        if roots.is_empty() {
            bail!("game {} has no intermediate output roots", config.game_address);
        }

        let starting_block = verifier.starting_block_number(config.game_address).await?;
        let interval =
            Self::infer_interval(config.game_address, verifier, starting_block, roots.len())
                .await?;
        let indices: Vec<u64> = config
            .invalid_index
            .map_or_else(|| (0..roots.len() as u64).collect(), |index| vec![index]);

        for index in indices {
            let root_index =
                usize::try_from(index).map_err(|_| eyre!("invalid index does not fit usize"))?;
            let onchain = *roots
                .get(root_index)
                .ok_or_else(|| eyre!("invalid index {index} out of range {}", roots.len()))?;
            let mut checkpoint =
                Self::from_roots(starting_block, interval, index, &roots, onchain)?;
            let canonical = config.output_root_at_block(checkpoint.target_block()).await?;
            if onchain != canonical {
                checkpoint.expected_root = canonical;
                return Ok(checkpoint);
            }
        }

        bail!("no invalid intermediate root found for game {}", config.game_address)
    }

    /// Requests a SNARK PLONK proof for this checkpoint and returns dispute-ready bytes.
    ///
    /// Polling stops when this future is dropped or times out, but the prover-service
    /// session continues server-side (acceptable for this one-shot Anvil tool).
    pub async fn request_proof(
        self,
        config: &Config,
        prover_address: Address,
        l1_head: B256,
    ) -> Result<Bytes> {
        let client = ProofRequesterClient::connect(&ProverServiceClientConfig::new(
            config.prover_service_url.as_str(),
        ))
        .context("failed to connect to prover-service")?;

        info!(
            prover_service_url = %config.prover_service_url,
            start_block = self.start_block,
            target_block = self.target_block(),
            l1_head = %l1_head,
            "requesting SNARK PLONK proof"
        );

        let response = client
            .prove_block_range(ChallengerProofAdapter::snark_plonk_prove_block_range_request(
                config.game_address,
                self.index,
                SnarkPlonkProofRequest {
                    proof: ZkProofRequest {
                        start_block_number: self.start_block,
                        number_of_blocks_to_prove: self.block_count,
                        sequence_window: None,
                        l1_head: Some(l1_head),
                        intermediate_root_interval: Some(self.block_count),
                        zk_vm: ZkVm::Sp1,
                        zk_backend: config.zk_backend,
                    },
                    prover_address,
                },
            ))
            .await
            .context("failed to submit proveBlockRange")?;
        let session_id = response.session_id;
        info!(session_id = %session_id, "proveBlockRange accepted");

        tokio::time::timeout(config.poll_timeout, async {
            loop {
                tokio::time::sleep(config.poll_interval).await;

                let proof = client
                    .get_proof(GetProofRequest { session_id: session_id.clone() })
                    .await
                    .with_context(|| format!("failed to poll getProof for session {session_id}"))?;

                match proof.status {
                    ProofStatus::Succeeded => {
                        let Some(result) = proof.result else {
                            bail!("SNARK proof session {session_id} succeeded without a result");
                        };
                        return ChallengerProofAdapter::snark_plonk_dispute_proof_bytes(result)
                            .context("failed to encode dispute proof bytes");
                    }
                    ProofStatus::Failed => {
                        let error_message =
                            proof.error_message.unwrap_or_else(|| "no error message".to_string());
                        bail!("SNARK proof session {session_id} failed: {error_message}");
                    }
                    ProofStatus::Queued | ProofStatus::Running => {}
                }
            }
        })
        .await
        .map_err(|_| {
            eyre!(
                "timed out after {:?} waiting for SNARK proof session {session_id}",
                config.poll_timeout
            )
        })?
    }

    fn from_roots(
        starting_block: u64,
        interval: u64,
        index: u64,
        roots: &[B256],
        expected_root: B256,
    ) -> Result<Self> {
        let root_index =
            usize::try_from(index).map_err(|_| eyre!("invalid index does not fit usize"))?;
        if root_index >= roots.len() {
            bail!("invalid index {index} out of range {}", roots.len());
        }
        let offset_steps = index.checked_add(1).ok_or_else(|| eyre!("invalid index overflow"))?;
        let target_block = starting_block
            .checked_add(
                interval.checked_mul(offset_steps).ok_or_else(|| eyre!("offset overflow"))?,
            )
            .ok_or_else(|| eyre!("target block overflow"))?;
        let start_block =
            target_block.checked_sub(interval).ok_or_else(|| eyre!("start block underflow"))?;
        Ok(Self { index, start_block, block_count: interval, expected_root })
    }

    async fn infer_interval(
        game_address: Address,
        verifier: &AggregateVerifierContractClient,
        starting_block: u64,
        root_count: usize,
    ) -> Result<u64> {
        let info = verifier.game_info(game_address).await?;
        let span = info
            .l2_block_number
            .checked_sub(starting_block)
            .ok_or_else(|| eyre!("game target block precedes starting block"))?;
        if root_count == 0 {
            bail!("cannot infer interval for a game with no intermediate roots");
        }
        if !span.is_multiple_of(root_count as u64) {
            bail!(
                "cannot infer intermediate interval: span {span} is not divisible by root count {root_count}"
            );
        }
        Ok(span / root_count as u64)
    }
}

/// Anvil storage/code patcher for dispute-game mutation.
#[derive(Debug, Default)]
struct AnvilPatch;

impl AnvilPatch {
    /// Patches game bytecode and factory registration so `index` holds `patched_root`.
    async fn apply(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        original_roots: &[B256],
        index: u64,
        patched_root: B256,
    ) -> Result<()> {
        let root_index =
            usize::try_from(index).map_err(|_| eyre!("invalid index does not fit usize"))?;
        let original_root = *original_roots
            .get(root_index)
            .ok_or_else(|| eyre!("invalid index {index} out of range {}", original_roots.len()))?;

        Self::patch_factory_registration(config, verifier, original_roots, index, patched_root)
            .await?;
        Self::patch_game_code(config, original_root, patched_root).await?;

        let onchain = verifier.intermediate_output_root(config.game_address, index).await?;
        if onchain != patched_root {
            bail!(
                "patched game code but intermediate root {index} stayed {onchain}; expected {patched_root}"
            );
        }

        info!(
            game = %config.game_address,
            invalid_index = index,
            from = %original_root,
            to = %patched_root,
            "patched invalid intermediate root on fork"
        );
        Ok(())
    }

    async fn patch_factory_registration(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
        original_roots: &[B256],
        index: u64,
        patched_root: B256,
    ) -> Result<()> {
        let root_index = usize::try_from(index).context("invalid root index does not fit usize")?;
        let info = verifier.game_info(config.game_address).await?;
        let original_extra =
            encode_extra_data(info.l2_block_number, info.parent_address, original_roots);
        let original_uuid = Self::game_uuid(config.game_type, info.root_claim, &original_extra);

        let mut patched_roots = original_roots.to_vec();
        patched_roots[root_index] = patched_root;
        let patched_extra =
            encode_extra_data(info.l2_block_number, info.parent_address, &patched_roots);
        let patched_uuid = Self::game_uuid(config.game_type, info.root_claim, &patched_extra);

        let factory = DisputeGameFactoryContractClient::new(
            config.dispute_game_factory,
            config.l1_rpc_url.clone(),
        )?;
        let original_lookup = factory
            .games(config.game_type, info.root_claim, original_extra)
            .await
            .context("failed to look up original factory game")?;
        if original_lookup != config.game_address {
            bail!(
                "factory lookup for original game data returned {original_lookup}, expected {}",
                config.game_address
            );
        }

        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let (mapping_slot, packed_game_id) = Self::find_mapping_slot(
            &provider,
            config.dispute_game_factory,
            original_uuid,
            config.game_type,
            config.game_address,
        )
        .await?;

        let storage_updated = provider
            .client()
            .request::<_, bool>(
                "anvil_setStorageAt",
                (
                    config.dispute_game_factory,
                    Self::mapping_storage_key(patched_uuid, mapping_slot),
                    packed_game_id,
                ),
            )
            .await
            .context(
                "anvil_setStorageAt failed; ensure BASE_ZK_FORK_L1_RPC_URL points to an Anvil fork",
            )?;
        if !storage_updated {
            bail!("anvil_setStorageAt returned false for patched factory registration");
        }

        let patched_lookup = factory
            .games(config.game_type, info.root_claim, patched_extra)
            .await
            .context("failed to look up patched factory game")?;
        if patched_lookup != config.game_address {
            bail!(
                "patched factory lookup returned {patched_lookup}, expected {}",
                config.game_address
            );
        }

        info!(
            game = %config.game_address,
            mapping_slot,
            "patched factory registration for mutated game"
        );
        Ok(())
    }

    async fn patch_game_code(
        config: &Config,
        original_root: B256,
        patched_root: B256,
    ) -> Result<()> {
        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let code = provider.get_code_at(config.game_address).await?;
        let mut patched_code = code.to_vec();
        let original = original_root.as_slice();
        let mut replacements = 0usize;
        let mut offset = 0;
        while offset + original.len() <= patched_code.len() {
            if &patched_code[offset..offset + original.len()] == original {
                patched_code[offset..offset + original.len()]
                    .copy_from_slice(patched_root.as_slice());
                replacements += 1;
                offset += original.len();
            } else {
                offset += 1;
            }
        }
        if replacements == 0 {
            bail!(
                "could not find intermediate root {original_root} in game {} bytecode",
                config.game_address
            );
        }

        provider
            .client()
            .request::<_, ()>("anvil_setCode", (config.game_address, Bytes::from(patched_code)))
            .await
            .context(
                "anvil_setCode failed; ensure BASE_ZK_FORK_L1_RPC_URL points to an Anvil fork",
            )?;
        Ok(())
    }

    async fn find_mapping_slot(
        provider: &RootProvider,
        factory_address: Address,
        original_uuid: B256,
        game_type: u32,
        game_address: Address,
    ) -> Result<(u64, B256)> {
        for mapping_slot in 0..256u64 {
            let value = provider
                .get_storage_at(
                    factory_address,
                    U256::from_be_slice(
                        Self::mapping_storage_key(original_uuid, mapping_slot).as_slice(),
                    ),
                )
                .await
                .with_context(|| {
                    format!("failed to read factory storage slot candidate {mapping_slot}")
                })?;
            if value == U256::ZERO {
                continue;
            }
            let bytes = value.to_be_bytes::<32>();
            let stored_type = u32::from_be_bytes(bytes[..4].try_into().expect("4-byte game type"));
            let stored_game = Address::from_slice(&bytes[12..]);
            if stored_type == game_type && stored_game == game_address {
                return Ok((mapping_slot, B256::from_slice(&bytes)));
            }
        }
        bail!(
            "could not discover DisputeGameFactory _disputeGames mapping slot for game {}",
            game_address
        )
    }

    fn game_uuid(game_type: u32, root_claim: B256, extra_data: &Bytes) -> B256 {
        let mut encoded = Vec::with_capacity(128 + extra_data.len().div_ceil(32) * 32);
        encoded.extend_from_slice(&U256::from(game_type).to_be_bytes::<32>());
        encoded.extend_from_slice(root_claim.as_slice());
        encoded.extend_from_slice(&U256::from(96).to_be_bytes::<32>());
        encoded.extend_from_slice(&U256::from(extra_data.len()).to_be_bytes::<32>());
        encoded.extend_from_slice(extra_data);
        let padding = (32 - extra_data.len() % 32) % 32;
        encoded.resize(encoded.len() + padding, 0);
        keccak256(encoded)
    }

    fn mapping_storage_key(key: B256, mapping_slot: u64) -> B256 {
        let mut encoded = [0u8; 64];
        encoded[..32].copy_from_slice(key.as_slice());
        encoded[32..].copy_from_slice(&U256::from(mapping_slot).to_be_bytes::<32>());
        keccak256(encoded)
    }
}
