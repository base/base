//! Invalid intermediate-root checkpoint selection, Anvil patching, and proving.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, RootProvider};
use base_challenger::ChallengerProofAdapter;
use base_proof_contracts::{
    AggregateVerifierClient, AggregateVerifierContractClient, DisputeGameFactoryClient,
    DisputeGameFactoryContractClient, GameInfo, encode_extra_data,
};
use base_prover_service_client::{ProofRequesterClient, ProverServiceClientConfig};
use base_prover_service_protocol::{
    GetProofRequest, ProofRequest, ProofRequestKind, ProofSessionId, ProofStatus,
    ProveBlockRangeRequest, SnarkPlonkProofRequest, ZkProofRequest, ZkVm,
};
use eyre::{Context, Result, bail, eyre};
use tracing::info;

use crate::config::Config;

/// Checkpoint that the workflow will prove and dispute.
#[derive(Debug, Clone, Copy)]
pub struct Checkpoint {
    /// 0-based invalid intermediate index.
    ///
    /// Unused by the proof journal for a full-game [`Self::proposal`] proof;
    /// kept so session IDs stay unique per constructed checkpoint.
    pub index: u64,
    /// Inclusive L2 start block for the proof range.
    pub start_block: u64,
    /// Number of L2 blocks to prove.
    pub block_count: u64,
    /// Intermediate root interval committed by the proof.
    ///
    /// Equal to [`Self::block_count`] for a single-checkpoint dispute proof.
    /// Smaller than `block_count` when proving a full game for `verifyProposalProof`.
    pub interval: u64,
    /// Canonical output root expected by the dispute call.
    pub expected_root: B256,
}

impl Checkpoint {
    /// Exclusive end block of the proof range.
    ///
    /// Unchecked add is safe: values are only constructed via [`Self::from_roots`]
    /// and [`Self::proposal`], which validate the sum with `checked_add`.
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
        if root_index >= roots.len() {
            bail!("invalid index {index} out of range {}", roots.len());
        }

        let starting_block = verifier.starting_block_number(config.game_address).await?;
        let interval =
            Self::infer_interval(config.game_address, verifier, starting_block, roots.len())
                .await?;
        let mut checkpoint =
            Self::from_roots(starting_block, interval, index, &roots, roots[root_index])?;
        let canonical = config.output_root_at_block(checkpoint.target_block()).await?;
        let mut patched_root = canonical;
        *patched_root.0.last_mut().expect("B256 is non-empty") ^= 1;
        if patched_root == canonical {
            bail!("failed to derive a patched root distinct from canonical {canonical}");
        }

        AnvilPatch::apply(config, verifier, &roots, index, patched_root).await?;

        checkpoint.expected_root = canonical;
        Ok(checkpoint)
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
        let root_count = u64::try_from(roots.len())
            .map_err(|_| eyre!("intermediate root count does not fit u64"))?;
        let indices = match config.invalid_index {
            Some(index) => {
                let end = index.checked_add(1).ok_or_else(|| eyre!("invalid index overflow"))?;
                index..end
            }
            None => 0..root_count,
        };

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

    /// Builds a checkpoint covering the game's full canonical range, unpatched.
    ///
    /// The proof journal matches the roots already stored on the game, which is
    /// what `verifyProposalProof` reconstructs.
    pub async fn proposal(
        config: &Config,
        verifier: &AggregateVerifierContractClient,
    ) -> Result<Self> {
        let roots = verifier.intermediate_output_roots(config.game_address).await?;
        if roots.is_empty() {
            bail!("game {} has no intermediate output roots", config.game_address);
        }

        let starting_block = verifier.starting_block_number(config.game_address).await?;
        let interval =
            Self::infer_interval(config.game_address, verifier, starting_block, roots.len())
                .await?;
        let root_count = u64::try_from(roots.len())
            .map_err(|_| eyre!("intermediate root count does not fit u64"))?;
        let block_count =
            interval.checked_mul(root_count).ok_or_else(|| eyre!("proposal range overflow"))?;
        // Upholds the invariant `target_block` relies on for its unchecked add.
        starting_block
            .checked_add(block_count)
            .ok_or_else(|| eyre!("proposal target block overflow"))?;

        Ok(Self {
            index: 0,
            start_block: starting_block,
            block_count,
            interval,
            expected_root: roots[roots.len() - 1],
        })
    }

    /// Requests a SNARK PLONK proof for this checkpoint and returns dispute-ready bytes.
    ///
    /// Pins the schedule to the game's final L2 block, which may be after [`Self::target_block`],
    /// so the proof commits to the same schedule as the game it disputes.
    ///
    /// Polling stops when this future is dropped or times out, but the prover-service
    /// session continues server-side (acceptable for this one-shot Anvil tool).
    pub async fn request_proof(
        self,
        config: &Config,
        prover_address: Address,
        l1_head: B256,
        game_l2_block_number: u64,
        zk_artifact_hash: B256,
    ) -> Result<Bytes> {
        let client = ProofRequesterClient::connect(&ProverServiceClientConfig::new(
            config.prover_service_url.as_str(),
        ))
        .context("failed to connect to prover-service")?;

        info!(
            prover_service_url = %config.prover_service_url,
            start_block = self.start_block,
            target_block = self.target_block(),
            interval = self.interval,
            l1_head = %l1_head,
            schedule_l2_block_number = game_l2_block_number,
            "requesting SNARK PLONK proof"
        );

        let snark_request = SnarkPlonkProofRequest {
            proof: ZkProofRequest {
                start_block_number: self.start_block,
                number_of_blocks_to_prove: self.block_count,
                sequence_window: None,
                l1_head: Some(l1_head),
                intermediate_root_interval: Some(self.interval),
                schedule_l2_block_number: Some(game_l2_block_number),
                zk_artifact_hash: Some(zk_artifact_hash),
                zk_vm: ZkVm::Sp1,
                zk_backend: config.zk_backend,
            },
            prover_address,
        };
        let session_id = Self::proof_session_id(
            config.game_address,
            self.index,
            prover_address,
            config.zk_backend.as_str(),
            zk_artifact_hash,
        );
        let response = client
            .prove_block_range(ProveBlockRangeRequest {
                proof: ProofRequest {
                    session_id: session_id.clone(),
                    request: ProofRequestKind::SnarkPlonk(snark_request),
                },
                retry_failed: true,
            })
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
        Ok(Self { index, start_block, block_count: interval, interval, expected_root })
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
        let root_count = u64::try_from(root_count)
            .map_err(|_| eyre!("intermediate root count does not fit u64"))?;
        if !span.is_multiple_of(root_count) {
            bail!(
                "cannot infer intermediate interval: span {span} is not divisible by root count {root_count}"
            );
        }
        Ok(span / root_count)
    }

    /// Session ID unique per fork-tool run identity (game/index/signer/backend).
    fn proof_session_id(
        game_address: Address,
        invalid_index: u64,
        prover_address: Address,
        zk_backend: &str,
        zk_artifact_hash: B256,
    ) -> String {
        let invalid_index = invalid_index.to_be_bytes();
        ProofSessionId::derive_from_components(
            b"base/zk-fork-dispute/proof-session/v2",
            "zk/sp1/snark_plonk",
            &[
                game_address.as_slice(),
                &invalid_index,
                prover_address.as_slice(),
                zk_backend.as_bytes(),
                zk_artifact_hash.as_slice(),
            ],
        )
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

        let info = verifier.game_info(config.game_address).await?;
        let root_claim = info.root_claim;

        Self::patch_factory_registration(config, info, original_roots, root_index, patched_root)
            .await?;
        Self::patch_game_code(
            config,
            info.l2_block_number,
            info.parent_address,
            original_roots,
            root_index,
            patched_root,
        )
        .await?;

        let onchain_roots = verifier.intermediate_output_roots(config.game_address).await?;
        if onchain_roots.len() != original_roots.len() {
            bail!(
                "root count changed after patch: before {}, after {}",
                original_roots.len(),
                onchain_roots.len()
            );
        }
        for (i, original) in original_roots.iter().enumerate() {
            let expected = if i == root_index { patched_root } else { *original };
            if onchain_roots[i] != expected {
                bail!(
                    "intermediate root {i} is {} after patch; expected {expected}",
                    onchain_roots[i]
                );
            }
        }
        let after_claim = verifier.game_info(config.game_address).await?.root_claim;
        if after_claim != root_claim {
            bail!(
                "bytecode patch mutated rootClaim from {root_claim} to {after_claim}; expected unchanged"
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
        info: GameInfo,
        original_roots: &[B256],
        root_index: usize,
        patched_root: B256,
    ) -> Result<()> {
        let original_extra =
            encode_extra_data(info.l2_block_number, info.parent_address, original_roots);
        let original_uuid = Self::game_uuid(config.game_type, info.root_claim, &original_extra);

        let mut patched_roots = original_roots.to_vec();
        patched_roots[root_index] = patched_root;
        let patched_extra =
            encode_extra_data(info.l2_block_number, info.parent_address, &patched_roots);
        let patched_uuid = Self::game_uuid(config.game_type, info.root_claim, &patched_extra);

        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let factory =
            DisputeGameFactoryContractClient::new(config.dispute_game_factory, provider.clone());
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
        l2_block_number: u64,
        parent_address: Address,
        original_roots: &[B256],
        root_index: usize,
        patched_root: B256,
    ) -> Result<()> {
        let provider: RootProvider = RootProvider::new_http(config.l1_rpc_url.clone());
        let code = provider.get_code_at(config.game_address).await?;
        let patched_code = patch_cwia_root_in_bytecode(
            code.as_ref(),
            l2_block_number,
            parent_address,
            original_roots,
            root_index,
            patched_root,
        )?;

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

/// Patches intermediate root `root_index` inside clone CWIA args.
///
/// Locates packed [`encode_extra_data`] in the CWIA args region and writes the
/// indexed 32-byte slot, so a final root equal to `rootClaim` is not confused
/// with the separate CWIA `rootClaim` field.
fn patch_cwia_root_in_bytecode(
    code: &[u8],
    l2_block_number: u64,
    parent_address: Address,
    original_roots: &[B256],
    root_index: usize,
    patched_root: B256,
) -> Result<Vec<u8>> {
    if original_roots.is_empty() {
        bail!("cannot patch CWIA roots for a game with no intermediate roots");
    }
    if root_index >= original_roots.len() {
        bail!("root index {root_index} out of range {}", original_roots.len());
    }
    if code.len() < 2 {
        bail!("game bytecode is too short to contain CWIA args");
    }

    let args_len = usize::from(u16::from_be_bytes([code[code.len() - 2], code[code.len() - 1]]));
    if code.len() < 2 + args_len {
        bail!("CWIA args length {args_len} exceeds bytecode size {}", code.len());
    }
    let args_start = code.len() - 2 - args_len;
    let args = &code[args_start..code.len() - 2];

    let extra = encode_extra_data(l2_block_number, parent_address, original_roots);
    let Some(extra_rel) = find_subslice(args, extra.as_ref()) else {
        bail!("could not find packed CWIA extraData (intermediate roots region) in game bytecode");
    };
    let root_offset =
        root_index.checked_mul(32).ok_or_else(|| eyre!("CWIA root offset overflow"))?;
    let root_rel = extra_rel
        .checked_add(52)
        .and_then(|offset| offset.checked_add(root_offset))
        .ok_or_else(|| eyre!("CWIA root offset overflow"))?;
    let root_end = root_rel.checked_add(32).ok_or_else(|| eyre!("CWIA root offset overflow"))?;
    if root_end > args.len() {
        bail!("computed CWIA root offset is outside args region");
    }
    let root_abs =
        args_start.checked_add(root_rel).ok_or_else(|| eyre!("CWIA root offset overflow"))?;
    let root_abs_end =
        args_start.checked_add(root_end).ok_or_else(|| eyre!("CWIA root offset overflow"))?;

    let mut patched = code.to_vec();
    patched[root_abs..root_abs_end].copy_from_slice(patched_root.as_slice());
    Ok(patched)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use alloy_node_bindings::Anvil;
    use alloy_primitives::{Address, B256, Bytes, U256};
    use alloy_provider::{Provider, RootProvider};
    use base_proof_contracts::encode_extra_data;

    use super::{AnvilPatch, find_subslice, patch_cwia_root_in_bytecode};

    fn synthetic_cwia_bytecode(
        root_claim: B256,
        l1_head: B256,
        l2_block_number: u64,
        parent_address: Address,
        roots: &[B256],
    ) -> Vec<u8> {
        let extra = encode_extra_data(l2_block_number, parent_address, roots);
        let mut args = Vec::with_capacity(84 + extra.len());
        args.extend_from_slice(Address::repeat_byte(0x11).as_slice());
        args.extend_from_slice(root_claim.as_slice());
        args.extend_from_slice(l1_head.as_slice());
        args.extend_from_slice(extra.as_ref());

        let mut code = vec![0x60; 32];
        code.extend_from_slice(&args);
        let args_len = u16::try_from(args.len()).expect("test CWIA args fit u16");
        code.extend_from_slice(&args_len.to_be_bytes());
        code
    }

    fn read_cwia_roots(
        code: &[u8],
        l2_block_number: u64,
        parent_address: Address,
        root_count: usize,
    ) -> Vec<B256> {
        let args_len =
            usize::from(u16::from_be_bytes([code[code.len() - 2], code[code.len() - 1]]));
        let args_start = code.len() - 2 - args_len;
        let args = &code[args_start..code.len() - 2];
        let placeholder = vec![B256::ZERO; root_count];
        let extra = encode_extra_data(l2_block_number, parent_address, &placeholder);
        let extra_rel = find_subslice(args, &extra[..52]).expect("CWIA extraData header");

        (0..root_count)
            .map(|index| {
                let start = args_start + extra_rel + 52 + index * 32;
                B256::from_slice(&code[start..start + 32])
            })
            .collect()
    }

    fn roots() -> [B256; 3] {
        [B256::repeat_byte(0xaa), B256::repeat_byte(0xbb), B256::repeat_byte(0xcc)]
    }

    #[tokio::test]
    async fn anvil_set_code_patches_each_root_index() {
        let anvil = Anvil::new().spawn();
        let provider: RootProvider = RootProvider::new_http(anvil.endpoint_url());
        let game = Address::repeat_byte(0x42);
        let parent = Address::repeat_byte(0x22);
        let roots = roots();
        let code = synthetic_cwia_bytecode(roots[2], B256::repeat_byte(0x33), 1000, parent, &roots);

        provider
            .client()
            .request::<_, ()>("anvil_setCode", (game, Bytes::from(code)))
            .await
            .expect("set synthetic game code");

        for (index, patched_root) in [
            (0usize, B256::repeat_byte(0x01)),
            (1, B256::repeat_byte(0x02)),
            (2, B256::repeat_byte(0x03)),
        ] {
            let current = provider.get_code_at(game).await.expect("read game code");
            let current_roots = read_cwia_roots(current.as_ref(), 1000, parent, roots.len());
            let patched = patch_cwia_root_in_bytecode(
                current.as_ref(),
                1000,
                parent,
                &current_roots,
                index,
                patched_root,
            )
            .expect("patch CWIA root");

            provider
                .client()
                .request::<_, ()>("anvil_setCode", (game, Bytes::from(patched)))
                .await
                .expect("set patched game code");

            let after = provider.get_code_at(game).await.expect("read patched game code");
            let after_roots = read_cwia_roots(after.as_ref(), 1000, parent, roots.len());
            assert_eq!(after_roots[index], patched_root);
            for (other_index, root) in current_roots.iter().enumerate() {
                if other_index != index {
                    assert_eq!(after_roots[other_index], *root);
                }
            }

            let args_len =
                usize::from(u16::from_be_bytes([after[after.len() - 2], after[after.len() - 1]]));
            let args_start = after.len() - 2 - args_len;
            assert_eq!(
                &after[args_start + 20..args_start + 52],
                roots[2].as_slice(),
                "rootClaim must remain unchanged"
            );
        }
    }

    #[tokio::test]
    async fn anvil_storage_discovers_factory_mapping_slot() {
        let anvil = Anvil::new().spawn();
        let provider: RootProvider = RootProvider::new_http(anvil.endpoint_url());
        let factory = Address::repeat_byte(0xf1);
        let game = Address::repeat_byte(0x42);
        let game_type = 7u32;
        let uuid = B256::repeat_byte(0xab);
        let mapping_slot = 3u64;

        let mut packed = [0u8; 32];
        packed[..4].copy_from_slice(&game_type.to_be_bytes());
        packed[12..].copy_from_slice(game.as_slice());
        let packed_id = B256::from_slice(&packed);
        let storage_key = AnvilPatch::mapping_storage_key(uuid, mapping_slot);

        let updated = provider
            .client()
            .request::<_, bool>("anvil_setStorageAt", (factory, storage_key, packed_id))
            .await
            .expect("set factory storage");
        assert!(updated);

        let (found_slot, found_id) =
            AnvilPatch::find_mapping_slot(&provider, factory, uuid, game_type, game)
                .await
                .expect("discover factory mapping slot");
        assert_eq!(found_slot, mapping_slot);
        assert_eq!(found_id, packed_id);

        let value = provider
            .get_storage_at(factory, U256::from_be_slice(storage_key.as_slice()))
            .await
            .expect("read factory storage");
        assert_eq!(B256::from(value.to_be_bytes::<32>()), packed_id);
    }
}
