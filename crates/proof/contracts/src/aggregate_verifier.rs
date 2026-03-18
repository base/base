//! `AggregateVerifier` contract bindings.
//!
//! Used to query individual dispute game instances (status, ZK/TEE prover
//! addresses, output roots), read configuration such as `BLOCK_INTERVAL`
//! and `INTERMEDIATE_BLOCK_INTERVAL` from the implementation contract, and
//! construct state-changing calls like `nullify` via
//! [`encode_nullify_calldata`].

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// `AggregateVerifier` (dispute game) contract interface.
    ///
    /// Each game instance is a clone created by `DisputeGameFactory.create()`.
    #[sol(rpc)]
    interface IAggregateVerifier {
        /// Returns the root claim (output root) of this game.
        function rootClaim() external pure returns (bytes32);

        /// Returns the L2 block number this game proposes.
        function l2SequenceNumber() external pure returns (uint256);

        /// Returns the current game status (`0=IN_PROGRESS`, `1=CHALLENGER_WINS`, `2=DEFENDER_WINS`).
        function status() external view returns (uint8);

        /// Returns the address that provided a TEE proof.
        function teeProver() external view returns (address);

        /// Returns the address that provided a ZK proof.
        function zkProver() external view returns (address);

        /// Returns the parent game's factory index.
        function parentIndex() external pure returns (uint32);

        /// Returns the block interval between proposals (immutable on the implementation).
        function BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the intermediate block interval for intermediate output root checkpoints.
        function INTERMEDIATE_BLOCK_INTERVAL() external view returns (uint256);

        /// Returns the game type.
        function gameType() external view returns (uint32);

        /// Returns the starting block number.
        function startingBlockNumber() external view returns (uint256);

        /// Returns the intermediate output roots submitted with this game.
        function intermediateOutputRoots() external view returns (bytes memory);

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
    }
}

/// Information about a dispute game instance.
#[derive(Debug, Clone)]
pub struct GameInfo {
    /// The output root claimed by this game.
    pub root_claim: B256,
    /// The L2 block number of this game.
    pub l2_block_number: u64,
    /// The parent game's factory index.
    pub parent_index: u32,
}

/// Async trait for querying `AggregateVerifier` game instances.
#[async_trait]
pub trait AggregateVerifierClient: Send + Sync {
    /// Queries game details from a game proxy address.
    async fn game_info(&self, game_address: Address) -> Result<GameInfo, ContractError>;

    /// Returns the current game status (`0=IN_PROGRESS`, `1=CHALLENGER_WINS`, `2=DEFENDER_WINS`).
    async fn status(&self, game_address: Address) -> Result<u8, ContractError>;

    /// Returns the address that provided a ZK proof for the given game.
    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns the address that provided a TEE proof for the given game.
    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError>;

    /// Returns the starting block number for the given game.
    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError>;

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

        let root_claim: B256 = contract
            .rootClaim()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "rootClaim failed".into(), source: e })?;

        let l2_seq: U256 = contract.l2SequenceNumber().call().await.map_err(|e| {
            ContractError::Call { context: "l2SequenceNumber failed".into(), source: e }
        })?;

        let l2_block_number: u64 = l2_seq
            .try_into()
            .map_err(|_| ContractError::Validation("l2SequenceNumber overflows u64".into()))?;

        let parent_index: u32 = contract
            .parentIndex()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "parentIndex failed".into(), source: e })?;

        Ok(GameInfo { root_claim, l2_block_number, parent_index })
    }

    async fn status(&self, game_address: Address) -> Result<u8, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract
            .status()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "status failed".into(), source: e })
    }

    async fn zk_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract
            .zkProver()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "zkProver failed".into(), source: e })
    }

    async fn tee_prover(&self, game_address: Address) -> Result<Address, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        contract
            .teeProver()
            .call()
            .await
            .map_err(|e| ContractError::Call { context: "teeProver failed".into(), source: e })
    }

    async fn starting_block_number(&self, game_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let block_u256: U256 = contract.startingBlockNumber().call().await.map_err(|e| {
            ContractError::Call { context: "startingBlockNumber failed".into(), source: e }
        })?;

        block_u256
            .try_into()
            .map_err(|_| ContractError::Validation("startingBlockNumber overflows u64".into()))
    }

    async fn read_block_interval(&self, impl_address: Address) -> Result<u64, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(impl_address, &self.provider);
        let interval_u256: U256 = contract.BLOCK_INTERVAL().call().await.map_err(|e| {
            ContractError::Call { context: "BLOCK_INTERVAL failed".into(), source: e }
        })?;

        let interval: u64 = interval_u256
            .try_into()
            .map_err(|_| ContractError::Validation("BLOCK_INTERVAL overflows u64".into()))?;

        // Also validated at startup in main.rs; duplicated here for defense-in-depth.
        if interval < 2 {
            return Err(ContractError::Validation(
                "BLOCK_INTERVAL must be at least 2 (single-block proposals are not supported)"
                    .into(),
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
        let interval_u256: U256 =
            contract.INTERMEDIATE_BLOCK_INTERVAL().call().await.map_err(|e| {
                ContractError::Call {
                    context: "INTERMEDIATE_BLOCK_INTERVAL failed".into(),
                    source: e,
                }
            })?;

        let interval: u64 = interval_u256.try_into().map_err(|_| {
            ContractError::Validation("INTERMEDIATE_BLOCK_INTERVAL overflows u64".into())
        })?;

        if interval == 0 {
            return Err(ContractError::Validation(
                "INTERMEDIATE_BLOCK_INTERVAL cannot be 0".into(),
            ));
        }

        Ok(interval)
    }

    async fn intermediate_output_roots(
        &self,
        game_address: Address,
    ) -> Result<Vec<B256>, ContractError> {
        let contract =
            IAggregateVerifier::IAggregateVerifierInstance::new(game_address, &self.provider);

        let raw: Bytes = contract.intermediateOutputRoots().call().await.map_err(|e| {
            ContractError::Call { context: "intermediateOutputRoots failed".into(), source: e }
        })?;

        if !raw.len().is_multiple_of(32) {
            return Err(ContractError::Validation(format!(
                "intermediateOutputRoots length {} is not a multiple of 32",
                raw.len()
            )));
        }

        let roots = raw.chunks_exact(32).map(|chunk| B256::from_slice(chunk)).collect();

        Ok(roots)
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
}
