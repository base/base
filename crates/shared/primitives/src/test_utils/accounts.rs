//! Test accounts with pre-funded balances for integration testing.

use alloy_consensus::{SignableTransaction, Transaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, TxHash, address, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_alloy_network::TransactionBuilder;
use base_alloy_rpc_types::OpTransactionRequest;
use eyre::{Result, eyre};

use super::DEVNET_CHAIN_ID;

/// EIP-1559 transaction type constant.
const EIP1559_TX_TYPE: u8 = 2;

/// Hardcoded test accounts using Anvil's deterministic keys.
/// Derived from the test mnemonic: "test test test test test test test test test test test junk"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Account {
    /// Alice (Anvil account #0)
    Alice,
    /// Bob (Anvil account #1)
    Bob,
    /// Charlie (Anvil account #2)
    Charlie,
    /// Deployer (Anvil account #3)
    Deployer,
}

impl Account {
    /// Returns the Ethereum address for this account.
    pub const fn address(&self) -> Address {
        match self {
            Self::Alice => address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
            Self::Bob => address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
            Self::Charlie => address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
            Self::Deployer => address!("90F79bf6EB2c4f870365E785982E1f101E93b906"),
        }
    }

    /// Returns the private key (hex string without 0x prefix).
    pub const fn private_key(&self) -> &'static str {
        match self {
            Self::Alice => "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            Self::Bob => "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
            Self::Charlie => "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
            Self::Deployer => "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
        }
    }

    /// Returns all available test accounts.
    pub const fn all() -> [Self; 4] {
        [Self::Alice, Self::Bob, Self::Charlie, Self::Deployer]
    }

    /// Constructs and returns a `PrivateKeySigner` for this account.
    pub fn signer(&self) -> PrivateKeySigner {
        let key_bytes =
            hex::decode(self.private_key()).expect("should be able to decode private key");
        let key_fixed: FixedBytes<32> = FixedBytes::from_slice(&key_bytes);
        PrivateKeySigner::from_bytes(&key_fixed)
            .expect("should be able to build the PrivateKeySigner")
    }

    /// Returns the private key as a B256 for use with `TransactionBuilder`.
    pub fn signer_b256(&self) -> B256 {
        let key_bytes =
            hex::decode(self.private_key()).expect("should be able to decode private key");
        B256::from_slice(&key_bytes)
    }

    /// Constructs a signed CREATE transaction with a given nonce and
    /// returns the signed bytes, contract address, and transaction hash.
    pub fn create_deployment_tx(
        &self,
        bytecode: Bytes,
        nonce: u64,
    ) -> Result<(Bytes, Address, TxHash)> {
        let tx_request = OpTransactionRequest::default()
            .from(self.address())
            .transaction_type(EIP1559_TX_TYPE)
            .with_gas_limit(3_000_000)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(DEVNET_CHAIN_ID)
            .with_deploy_code(bytecode)
            .with_nonce(nonce);

        let tx = tx_request.build_typed_tx().map_err(|_| eyre!("invalid transaction request"))?;
        let signature = self.signer().sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let signed_tx_bytes = signed_tx.encoded_2718().into();

        let contract_address = self.address().create(signed_tx.nonce());
        Ok((signed_tx_bytes, contract_address, *signed_tx.hash()))
    }

    /// Sign a `TransactionRequest` and return the signed bytes.
    pub fn sign_txn_request(&self, tx_request: OpTransactionRequest) -> Result<(Bytes, TxHash)> {
        let tx_request = tx_request
            .from(self.address())
            .transaction_type(EIP1559_TX_TYPE)
            .with_gas_limit(500_000)
            .with_chain_id(DEVNET_CHAIN_ID)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0);

        let tx = tx_request.build_typed_tx().map_err(|_| eyre!("invalid transaction request"))?;
        let signature = self.signer().sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let signed_tx_bytes = signed_tx.encoded_2718().into();
        let tx_hash = signed_tx.hash();
        Ok((signed_tx_bytes, *tx_hash))
    }
}
