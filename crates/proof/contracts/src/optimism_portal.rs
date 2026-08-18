//! Bindings for attested withdrawal redemption on OptimismPortal2.

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::RootProvider;
use alloy_sol_types::{SolCall, sol};
use async_trait::async_trait;

use crate::ContractError;

sol! {
    /// L2 message-passer event emitted for attested ETH withdrawals.
    interface IL2ToL1MessagePasser {
        event AttestedWithdrawalInitiated(bytes32 indexed authHash, address indexed recipient, address indexed token, uint256 amount, uint256 nonce);
    }

    /// Minimal `OptimismPortal2` interface used by the attested-withdrawal relay.
    #[sol(rpc)]
    interface IOptimismPortal2 {
        function attestRedeemed(bytes32 authHash) external view returns (bool);
        function redeemAttestedWithdrawal(address recipient, uint256 amount, uint256 nonce, bytes sig) external;
        event AttestedWithdrawalRedeemed(bytes32 indexed authHash, address indexed recipient, uint256 amount, uint256 nonce, address signer);
    }
}

/// Encodes `redeemAttestedWithdrawal` calldata.
#[must_use]
pub fn encode_redeem_attested_withdrawal_calldata(
    recipient: Address,
    amount: U256,
    nonce: U256,
    signature: Bytes,
) -> Bytes {
    Bytes::from(
        IOptimismPortal2::redeemAttestedWithdrawalCall { recipient, amount, nonce, sig: signature }
            .abi_encode(),
    )
}

/// Read-only portal operations used by the relayer.
#[async_trait]
pub trait OptimismPortalClient: Send + Sync {
    /// Returns whether an authorization has already been redeemed.
    async fn attest_redeemed(&self, auth_hash: B256) -> Result<bool, ContractError>;
}

/// Concrete implementation backed by Alloy's generated portal binding.
#[derive(Debug)]
pub struct OptimismPortalContractClient {
    contract: IOptimismPortal2::IOptimismPortal2Instance<RootProvider>,
}

impl OptimismPortalContractClient {
    /// Creates a portal client for the given L1 RPC endpoint.
    #[must_use]
    pub fn new(address: Address, l1_rpc_url: url::Url) -> Self {
        let provider = RootProvider::new_http(l1_rpc_url);
        let contract = IOptimismPortal2::IOptimismPortal2Instance::new(address, provider);
        Self { contract }
    }
}

#[async_trait]
impl OptimismPortalClient for OptimismPortalContractClient {
    async fn attest_redeemed(&self, auth_hash: B256) -> Result<bool, ContractError> {
        contract_call!(self.contract.attestRedeemed(auth_hash).call(), "attestRedeemed")
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, bytes};
    use alloy_sol_types::SolCall;

    use super::*;

    #[test]
    fn encodes_redemption_calldata() {
        let recipient = address!("1234567890123456789012345678901234567890");
        let signature = bytes!(
            "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001b"
        );
        let encoded = encode_redeem_attested_withdrawal_calldata(
            recipient,
            U256::from(42),
            U256::from(7),
            signature.clone(),
        );
        let expected = bytes!(
            "53520b650000000000000000000000001234567890123456789012345678901234567890"
            "000000000000000000000000000000000000000000000000000000000000002a"
            "0000000000000000000000000000000000000000000000000000000000000007"
            "0000000000000000000000000000000000000000000000000000000000000080"
            "0000000000000000000000000000000000000000000000000000000000000041"
            "0000000000000000000000000000000000000000000000000000000000000000"
            "000000000000000000000000000000000000000000000000000000000000001b"
            "00000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(encoded, expected);
        assert_eq!(&encoded[..4], &IOptimismPortal2::redeemAttestedWithdrawalCall::SELECTOR);
        assert_ne!(IOptimismPortal2::attestRedeemedCall::SELECTOR, [0; 4]);
    }
}
