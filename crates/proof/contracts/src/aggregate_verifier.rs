//! `AggregateVerifier` contract bindings.
//!
//! Used to query individual dispute game instances (status, ZK/TEE prover
//! addresses, output roots), read configuration such as `BLOCK_INTERVAL`
//! and `INTERMEDIATE_BLOCK_INTERVAL` from the implementation contract, and
//! construct state-changing calls like `nullify` via
//! [`encode_nullify_calldata`] or `challenge` via
//! [`encode_challenge_calldata`].

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, SolError, sol};
use async_trait::async_trait;

use crate::{
    ContractError,
    anchor_state_registry::{AnchorPreflight, AnchorRoot, IAnchorStateRegistry},
};

sol! {
    /// `AggregateVerifier` (dispute game) contract interface.
    ///
    /// Each game instance is a clone created by `DisputeGameFactory.create()`.
    #[sol(rpc)]
    interface IAggregateVerifier {
        /// Error returned when the proof's L1 origin is older than the EIP-2935 history window.
        error L1OriginTooOld(uint256 l1OriginNumber, uint256 currentBlock);

        /// Error returned by `initialize` when the parent game is unregistered,
        /// unrespected, blacklisted, retired, or resolved as `CHALLENGER_WINS`.
        error InvalidParentGame();

        /// Error bubbled from `TEEVerifier` when a proof signer is not registered.
        error InvalidSigner(address signer);

        /// Returns the root claim (output root) of this game.
        function rootClaim() external pure returns (bytes32);

        /// Returns the L2 block number this game proposes.
        function l2SequenceNumber() external pure returns (uint256);

        /// Returns the current game status. See [`GameStatus`] for values.
        function status() external view returns (uint8);

        /// Returns the address that provided a TEE proof.
        function teeProver() external view returns (address);

        /// Returns the address that provided a ZK proof.
        function zkProver() external view returns (address);

        /// Returns the parent game's address.
        function parentAddress() external pure returns (address);

        /// Returns the block interval between proposals (immutable on the implementation).
        function BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the intermediate block interval for intermediate output root checkpoints.
        function INTERMEDIATE_BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the game type.
        function gameType() external view returns (uint32);

        /// Returns the starting block number.
        function startingBlockNumber() external view returns (uint256);

        /// Returns the L1 head block hash stored in CWIA at game creation time.
        function l1Head() external pure returns (bytes32);

        /// Returns the intermediate output roots submitted with this game.
        function intermediateOutputRoots() external view returns (bytes memory);

        /// Returns the intermediate output root at the given index.
        function intermediateOutputRoot(uint256 index) external view returns (bytes32);

        /// Returns the 1-based index of the challenged intermediate root.
        ///
        /// `0` means the game has not been challenged. When non-zero the
        /// actual 0-based index is `counteredByIntermediateRootIndexPlusOne - 1`.
        function counteredByIntermediateRootIndexPlusOne() external view returns (uint256);

        /// Nullifies an intermediate root checkpoint for the given game.
        ///
        /// The first byte of `proofBytes` is the proof type discriminator:
        /// `0` for TEE, `1` for ZK. Use [`encode_nullify_calldata`] to
        /// construct ABI-encoded calldata for this function.
        function nullify(
            bytes calldata proofBytes,
            uint256 intermediateRootIndex,
            bytes32 intermediateRootToProve
        ) external;

        /// Challenges the TEE proof with a ZK proof.
        ///
        /// The first byte of `proofBytes` must be `1` (ZK). Use
        /// [`encode_challenge_calldata`] to construct ABI-encoded calldata
        /// for this function.
        function challenge(
            bytes calldata proofBytes,
            uint256 intermediateRootIndex,
            bytes32 intermediateRootToProve
        ) external;

        /// Resolves the game after a proof has been provided and enough time
        /// has passed. Returns the resulting `GameStatus`.
        ///
        /// Use [`encode_resolve_calldata`] to construct ABI-encoded calldata
        /// for this function.
        function resolve() external returns (uint8);

        /// Claims the bond credit for the bond recipient. Must be called
        /// twice: the first call triggers `DelayedWETH.unlock()`, and the
        /// second call (after `DELAY_SECONDS`) triggers the actual withdrawal.
        ///
        /// Use [`encode_claim_credit_calldata`] to construct ABI-encoded
        /// calldata for this function.
        function claimCredit() external;

        /// Returns whether the game's finalization delay has passed
        /// (`expectedResolution <= block.timestamp`).
        function gameOver() external view returns (bool);

        /// Returns the timestamp at which the game was resolved (`0` if unresolved).
        function resolvedAt() external view returns (uint64);

        /// Returns the address that will receive the bond.
        ///
        /// Defaults to the game creator; set to the ZK challenger if the
        /// game resolves as `CHALLENGER_WINS`.
        function bondRecipient() external view returns (address);

        /// Returns whether the bond has been unlocked via `DelayedWETH.unlock()`.
        function bondUnlocked() external view returns (bool);

        /// Returns whether the bond has been fully claimed (withdrawn).
        function bondClaimed() external view returns (bool);

        /// Returns the timestamp at which the game can be resolved.
        ///
        /// Initialized to `type(uint64).max` (never), decreased when proofs
        /// are verified, and increased when proofs are nullified.
        function expectedResolution() external view returns (uint64);

        /// Returns the number of verified proofs for this game.
        function proofCount() external view returns (uint8);

        /// Returns the timestamp at which the game was created.
        function createdAt() external view returns (uint64);

        /// Returns the address of the `DelayedWETH` contract used by this game.
        function DELAYED_WETH() external view returns (address);

        /// Returns the address of the `AnchorStateRegistry` contract.
        function anchorStateRegistry() external view returns (address);
    }
}

/// Information about a dispute game instance.
#[derive(Debug, Clone, Copy)]
pub struct GameInfo {
    /// The output root claimed by this game.
    pub root_claim: B256,
    /// The L2 block number of this game.
    pub l2_block_number: u64,
    /// The parent game's proxy address.
    pub parent_address: Address,
}

/// Status values returned by `AggregateVerifier.status()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GameStatus {
    /// The game is still in progress.
    InProgress = 0,
    /// The challenger won the dispute.
    ChallengerWins = 1,
    /// The defender won the dispute.
    DefenderWins = 2,
}

impl std::fmt::Display for GameStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InProgress => write!(f, "InProgress (0)"),
            Self::ChallengerWins => write!(f, "ChallengerWins (1)"),
            Self::DefenderWins => write!(f, "DefenderWins (2)"),
        }
    }
}

impl TryFrom<u8> for GameStatus {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::InProgress),
            1 => Ok(Self::ChallengerWins),
            2 => Ok(Self::DefenderWins),
            other => Err(other),
        }
    }
}

/// Async trait for querying `AggregateVerifier` game instances.
#[async_trait]
pub trait AggregateVerifierClient: Send + Sync {
    /// Queries game details from a game proxy address.
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError>;

    /// Returns the current game status.
    async fn status(&self, game_address: Address) -> Result<GameStatus, ContractError>;

    /// Returns the address that provided a ZK proof for the given game.
    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns the address that provided a TEE proof for the given game.
    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns the starting block number for the given game.
    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError>;

    /// Returns the L1 head block hash stored at game creation time.
    async fn l1_head(&self, game_address: Address) -> Result<B256, ContractError>;

    /// Reads `BLOCK_INTERVAL` from the `AggregateVerifier` implementation contract.
    async fn read_block_interval(&self, impl_address: Address) -> Result<u64, ContractError>;

    /// Reads `INTERMEDIATE_BLOCK_INTERVAL` from the `AggregateVerifier` implementation contract.
    async fn read_intermediate_block_interval(
        &self,
        impl_address: Address,
    ) -> Result<u64, ContractError>;

    /// Returns the intermediate output roots for the given game.
    ///
    /// The raw bytes are expected to be a concatenation of 32-byte hashes.
    async fn intermediate_output_roots(
        &self,
        game_address: Address,
    ) -> Result<Vec<B256>, ContractError>;

    /// Returns a single intermediate output root at the given 0-based index.
    async fn intermediate_output_root(
        &self,
        game_address: Address,
        index: u64,
    ) -> Result<B256, ContractError>;

    /// Returns the 1-based index of the challenged intermediate root.
    ///
    /// `0` means the game has not been challenged. When non-zero the
    /// actual 0-based index is `value - 1`.
    async fn countered_index(&self, game_address: Address) -> Result<u64, ContractError>;

    /// Returns whether the game's finalization delay has elapsed.
    async fn game_over(&self, game_address: Address) -> Result<bool, ContractError>;

    /// Returns the timestamp at which the game was resolved (`0` if unresolved).
    async fn resolved_at(&self, game_address: Address) -> Result<u64, ContractError>;

    /// Returns the address that will receive the bond.
    async fn bond_recipient(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns whether the bond has been unlocked via `DelayedWETH.unlock()`.
    async fn bond_unlocked(&self, game_address: Address) -> Result<bool, ContractError>;

    /// Returns whether the bond has been fully claimed (withdrawn).
    async fn bond_claimed(&self, game_address: Address) -> Result<bool, ContractError>;

    /// Returns the timestamp at which the game can be resolved.
    async fn expected_resolution(&self, game_address: Address) -> Result<u64, ContractError>;

    /// Returns the number of verified proofs for this game.
    async fn proof_count(&self, game_address: Address) -> Result<u8, ContractError>;

    /// Returns the timestamp at which the game was created.
    async fn created_at(&self, game_address: Address) -> Result<u64, ContractError>;

    /// Returns the address of the `DelayedWETH` contract used by this game.
    async fn delayed_weth(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns the address of the `AnchorStateRegistry` contract for this game.
    async fn anchor_state_registry(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns whether the game is finalized in its `AnchorStateRegistry`.
    /// Cheap single call that gates the heavier [`anchor_preflight`] read.
    async fn is_game_finalized(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<bool, ContractError>;

    /// Reads the eligibility flags and current anchor root for a game from
    /// the `AnchorStateRegistry` in a single batched call. Should only be
    /// called after [`is_game_finalized`] returns true.
    async fn anchor_preflight(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<AnchorPreflight, ContractError>;
}

/// The 4-byte selector for `L1OriginTooOld()`.
pub const fn l1_origin_too_old_selector() -> [u8; 4] {
    IAggregateVerifier::L1OriginTooOld::SELECTOR
}

/// The 4-byte selector for `InvalidParentGame()`.
pub const fn invalid_parent_game_selector() -> [u8; 4] {
    IAggregateVerifier::InvalidParentGame::SELECTOR
}

/// The 4-byte selector for `TEEVerifier.InvalidSigner(address)`.
pub const fn invalid_signer_selector() -> [u8; 4] {
    IAggregateVerifier::InvalidSigner::SELECTOR
}

/// Concrete implementation backed by Alloy's sol-generated contract bindings.
#[derive(Debug)]
pub struct AggregateVerifierContractClient {
    provider: RootProvider,
}

impl AggregateVerifierContractClient {
    /// Creates a new client connected to the given L1 RPC URL.
    pub fn new(l1_rpc_url: url::Url) -> Result<Self, ContractError> {
        let provider = RootProvider::new_http(l1_rpc_url);
        Ok(Self { provider })
    }
}

#[async_trait]
impl AggregateVerifierClient for AggregateVerifierContractClient {
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let (root_claim, l2_seq, parent_address) = futures::try_join!(
            async { contract_call!(contract.rootClaim().call(), "rootClaim failed") },
            async { contract_call!(contract.l2SequenceNumber().call(), "l2SequenceNumber failed") },
            async { contract_call!(contract.parentAddress().call(), "parentAddress failed") },
        )?;

        let l2_block_number: u64 = l2_seq
            .try_into()
            .map_err(|_| ContractError::validation("l2SequenceNumber overflows u64"))?;

        Ok(GameInfo { root_claim, l2_block_number, parent_address })
    }

    async fn status(&self, game_address: Address) -> Result<GameStatus, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let raw: u8 = contract_call!(contract.status().call(), "status failed")?;

        GameStatus::try_from(raw).map_err(|unknown| {
            ContractError::validation(format!(
                "game {game_address} returned unrecognized status {unknown}"
            ))
        })
    }

    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.zkProver().call(), "zkProver failed")
    }

    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.teeProver().call(), "teeProver failed")
    }

    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let block_u256: U256 =
            contract_call!(contract.startingBlockNumber().call(), "startingBlockNumber failed")?;

        block_u256
            .try_into()
            .map_err(|_| ContractError::validation("startingBlockNumber overflows u64"))
    }

    async fn l1_head(&self, game_address: Address) -> Result<B256, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.l1Head().call(), "l1Head failed")
    }

    async fn read_block_interval(&self, impl_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(impl_address, &self.provider);
        let interval_u256: U256 =
            contract_call!(contract.BLOCK_INTERVAL().call(), "BLOCK_INTERVAL failed")?;

        let interval: u64 = interval_u256
            .try_into()
            .map_err(|_| ContractError::validation("BLOCK_INTERVAL overflows u64"))?;

        // Also validated at startup in main.rs; duplicated here for defense-in-depth.
        if interval < 2 {
            return Err(ContractError::validation(
                "BLOCK_INTERVAL must be at least 2 (single-block proposals are not supported)",
            ));
        }

        Ok(interval)
    }

    async fn read_intermediate_block_interval(
        &self,
        impl_address: Address,
    ) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(impl_address, &self.provider);
        let interval_u256: U256 = contract_call!(
            contract.INTERMEDIATE_BLOCK_INTERVAL().call(),
            "INTERMEDIATE_BLOCK_INTERVAL failed"
        )?;

        let interval: u64 = interval_u256
            .try_into()
            .map_err(|_| ContractError::validation("INTERMEDIATE_BLOCK_INTERVAL overflows u64"))?;

        if interval == 0 {
            return Err(ContractError::validation("INTERMEDIATE_BLOCK_INTERVAL cannot be 0"));
        }

        Ok(interval)
    }

    async fn intermediate_output_roots(
        &self,
        game_address: Address,
    ) -> Result<Vec<B256>, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let raw: Bytes = contract_call!(
            contract.intermediateOutputRoots().call(),
            "intermediateOutputRoots failed"
        )?;

        if !raw.len().is_multiple_of(32) {
            return Err(ContractError::validation(format!(
                "intermediateOutputRoots length {} is not a multiple of 32",
                raw.len()
            )));
        }

        let roots = raw.chunks_exact(32).map(|chunk| B256::from_slice(chunk)).collect();

        Ok(roots)
    }

    async fn intermediate_output_root(
        &self,
        game_address: Address,
        index: u64,
    ) -> Result<B256, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let root = contract_call!(
            contract.intermediateOutputRoot(U256::from(index)).call(),
            "intermediateOutputRoot failed"
        )?;

        Ok(root)
    }

    async fn countered_index(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let value: U256 = contract_call!(
            contract.counteredByIntermediateRootIndexPlusOne().call(),
            "counteredByIntermediateRootIndexPlusOne failed"
        )?;

        value.try_into().map_err(|_| {
            ContractError::validation("counteredByIntermediateRootIndexPlusOne overflows u64")
        })
    }

    async fn game_over(&self, game_address: Address) -> Result<bool, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.gameOver().call(), "gameOver failed")
    }

    async fn resolved_at(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.resolvedAt().call(), "resolvedAt failed")
    }

    async fn bond_recipient(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.bondRecipient().call(), "bondRecipient failed")
    }

    async fn bond_unlocked(&self, game_address: Address) -> Result<bool, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.bondUnlocked().call(), "bondUnlocked failed")
    }

    async fn bond_claimed(&self, game_address: Address) -> Result<bool, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.bondClaimed().call(), "bondClaimed failed")
    }

    async fn expected_resolution(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.expectedResolution().call(), "expectedResolution failed")
    }

    async fn proof_count(&self, game_address: Address) -> Result<u8, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.proofCount().call(), "proofCount failed")
    }

    async fn created_at(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.createdAt().call(), "createdAt failed")
    }

    async fn delayed_weth(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.DELAYED_WETH().call(), "DELAYED_WETH failed")
    }

    async fn anchor_state_registry(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract_call!(contract.anchorStateRegistry().call(), "anchorStateRegistry failed")
    }

    async fn is_game_finalized(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<bool, ContractError> {
        let contract =
            IAnchorStateRegistry::IAnchorStateRegistryInstance::new(asr_address, &self.provider);

        contract_call!(contract.isGameFinalized(game_address).call(), "isGameFinalized failed")
    }

    async fn anchor_preflight(
        &self,
        asr_address: Address,
        game_address: Address,
    ) -> Result<AnchorPreflight, ContractError> {
        let contract =
            IAnchorStateRegistry::IAnchorStateRegistryInstance::new(asr_address, &self.provider);

        let (blacklisted, retired, respected, paused, anchor) = futures::try_join!(
            async {
                contract_call!(
                    contract.isGameBlacklisted(game_address).call(),
                    "isGameBlacklisted failed"
                )
            },
            async {
                contract_call!(contract.isGameRetired(game_address).call(), "isGameRetired failed")
            },
            async {
                contract_call!(
                    contract.isGameRespected(game_address).call(),
                    "isGameRespected failed"
                )
            },
            async { contract_call!(contract.paused().call(), "paused failed") },
            async { contract_call!(contract.getAnchorRoot().call(), "getAnchorRoot failed") },
        )?;

        let l2_block_number: u64 = anchor
            .l2SequenceNumber
            .try_into()
            .map_err(|_| ContractError::validation("anchor l2SequenceNumber overflows u64"))?;

        Ok(AnchorPreflight {
            blacklisted,
            retired,
            respected,
            paused,
            anchor_root: AnchorRoot { root: anchor.root, l2_block_number },
        })
    }
}

/// Encodes the calldata for `IAggregateVerifier.nullify()`.
///
/// The first byte of `proof_bytes` is the proof type discriminator:
/// `0` for TEE, `1` for ZK.
pub fn encode_nullify_calldata(
    proof_bytes: Bytes,
    intermediate_root_index: u64,
    intermediate_root_to_prove: B256,
) -> Bytes {
    let call = IAggregateVerifier::nullifyCall {
        proofBytes: proof_bytes,
        intermediateRootIndex: U256::from(intermediate_root_index),
        intermediateRootToProve: intermediate_root_to_prove,
    };
    Bytes::from(call.abi_encode())
}

/// Encodes the calldata for `IAggregateVerifier.challenge()`.
///
/// Used when challenging a TEE-proven game with a ZK proof. The first
/// byte of `proof_bytes` must be `1` (ZK).
pub fn encode_challenge_calldata(
    proof_bytes: Bytes,
    intermediate_root_index: u64,
    intermediate_root_to_prove: B256,
) -> Bytes {
    let call = IAggregateVerifier::challengeCall {
        proofBytes: proof_bytes,
        intermediateRootIndex: U256::from(intermediate_root_index),
        intermediateRootToProve: intermediate_root_to_prove,
    };
    Bytes::from(call.abi_encode())
}

/// Encodes the calldata for `IAggregateVerifier.resolve()`.
///
/// Resolves the game after its dispute period has elapsed. Returns the
/// resulting `GameStatus` on-chain.
pub fn encode_resolve_calldata() -> Bytes {
    let call = IAggregateVerifier::resolveCall {};
    Bytes::from(call.abi_encode())
}

/// Encodes the calldata for `IAggregateVerifier.claimCredit()`.
///
/// Must be called twice: the first call unlocks the bond via
/// `DelayedWETH.unlock()`, and the second call (after `DELAY_SECONDS`)
/// withdraws and transfers ETH to the `bondRecipient`.
pub fn encode_claim_credit_calldata() -> Bytes {
    let call = IAggregateVerifier::claimCreditCall {};
    Bytes::from(call.abi_encode())
}

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolCall as _;

    use super::*;

    #[test]
    fn test_encode_nullify_calldata_has_selector() {
        let calldata = encode_nullify_calldata(
            Bytes::from(vec![0x00, 0xAA, 0xBB]),
            42,
            B256::repeat_byte(0xFF),
        );
        assert_eq!(&calldata[..4], &IAggregateVerifier::nullifyCall::SELECTOR);
    }

    #[test]
    fn test_encode_nullify_calldata_roundtrip() {
        let proof_bytes = Bytes::from(vec![0x01, 0xDE, 0xAD]);
        let index = 7u64;
        let root = B256::repeat_byte(0xAB);

        let calldata = encode_nullify_calldata(proof_bytes.clone(), index, root);

        let decoded = IAggregateVerifier::nullifyCall::abi_decode(&calldata)
            .expect("round-trip decode should succeed");

        assert_eq!(decoded.proofBytes, proof_bytes);
        assert_eq!(decoded.intermediateRootIndex, U256::from(index));
        assert_eq!(decoded.intermediateRootToProve, root);
    }

    #[test]
    fn test_encode_nullify_calldata_empty_proof_bytes() {
        let calldata = encode_nullify_calldata(Bytes::new(), 0, B256::ZERO);

        assert_eq!(&calldata[..4], &IAggregateVerifier::nullifyCall::SELECTOR);

        let decoded = IAggregateVerifier::nullifyCall::abi_decode(&calldata)
            .expect("decode with empty proof bytes should succeed");

        assert!(decoded.proofBytes.is_empty());
        assert_eq!(decoded.intermediateRootIndex, U256::ZERO);
        assert_eq!(decoded.intermediateRootToProve, B256::ZERO);
    }

    #[test]
    fn test_encode_challenge_calldata_has_selector() {
        let calldata = encode_challenge_calldata(
            Bytes::from(vec![0x01, 0xAA, 0xBB]),
            42,
            B256::repeat_byte(0xFF),
        );
        assert_eq!(&calldata[..4], &IAggregateVerifier::challengeCall::SELECTOR);
    }

    #[test]
    fn test_encode_challenge_calldata_roundtrip() {
        let proof_bytes = Bytes::from(vec![0x01, 0xDE, 0xAD]);
        let index = 7u64;
        let root = B256::repeat_byte(0xAB);

        let calldata = encode_challenge_calldata(proof_bytes.clone(), index, root);

        let decoded = IAggregateVerifier::challengeCall::abi_decode(&calldata)
            .expect("round-trip decode should succeed");

        assert_eq!(decoded.proofBytes, proof_bytes);
        assert_eq!(decoded.intermediateRootIndex, U256::from(index));
        assert_eq!(decoded.intermediateRootToProve, root);
    }

    #[test]
    fn test_challenge_and_nullify_selectors_differ() {
        let calldata_nullify = encode_nullify_calldata(Bytes::from(vec![0x00]), 0, B256::ZERO);
        let calldata_challenge = encode_challenge_calldata(Bytes::from(vec![0x01]), 0, B256::ZERO);
        assert_ne!(
            &calldata_nullify[..4],
            &calldata_challenge[..4],
            "nullify and challenge must have different selectors"
        );
    }

    #[test]
    fn test_encode_resolve_calldata_has_selector() {
        let calldata = encode_resolve_calldata();
        assert_eq!(&calldata[..4], &IAggregateVerifier::resolveCall::SELECTOR);
    }

    #[test]
    fn test_encode_claim_credit_calldata_has_selector() {
        let calldata = encode_claim_credit_calldata();
        assert_eq!(&calldata[..4], &IAggregateVerifier::claimCreditCall::SELECTOR);
    }

    #[test]
    fn test_l1_origin_too_old_selector() {
        let selector = l1_origin_too_old_selector();
        assert_eq!(selector.len(), 4);
        assert_ne!(selector, [0u8; 4]);
    }

    #[test]
    fn test_invalid_parent_game_selector() {
        let selector = invalid_parent_game_selector();
        assert_eq!(selector.len(), 4);
        assert_ne!(selector, [0u8; 4]);
        assert_ne!(selector, l1_origin_too_old_selector());
    }

    #[test]
    fn test_invalid_signer_selector() {
        let selector = invalid_signer_selector();
        assert_eq!(selector.len(), 4);
        assert_ne!(selector, [0u8; 4]);
        assert_ne!(selector, l1_origin_too_old_selector());
        assert_ne!(selector, invalid_parent_game_selector());
    }

    #[test]
    fn test_resolve_and_claim_credit_selectors_differ() {
        let resolve = encode_resolve_calldata();
        let claim = encode_claim_credit_calldata();
        assert_ne!(
            &resolve[..4],
            &claim[..4],
            "resolve and claimCredit must have different selectors"
        );
    }
}
