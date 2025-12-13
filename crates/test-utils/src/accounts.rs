//! Test accounts with pre-funded balances for integration testing

use alloy_consensus::{SignableTransaction, Transaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, FixedBytes, TxHash, address, hex};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::Result;
use op_alloy_network::TransactionBuilder;
use op_alloy_rpc_types::OpTransactionRequest;
use reth::{revm::context::TransactionType, rpc::compat::SignTxRequestError};

use crate::node::BASE_CHAIN_ID;

/// Hardcoded test account with a fixed private key
#[derive(Debug, Clone)]
pub struct Account {
    /// Account name for easy identification
    pub name: &'static str,
    /// Ethereum address
    pub address: Address,
    /// Private key (hex string without 0x prefix)
    pub private_key: &'static str,
}

impl Account {
    /// Constructs a signed CREATE transaction with a given nonce and
    /// returns the signed bytes, contract address, and transaction hash
    pub fn create_deployment_tx(
        &self,
        bytecode: Bytes,
        nonce: u64,
    ) -> Result<(Bytes, Address, TxHash)> {
        let tx_request = OpTransactionRequest::default()
            .from(self.address)
            .transaction_type(TransactionType::Eip1559.into())
            .with_gas_limit(3_000_000) // Increased for larger contracts like ERC-20
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(BASE_CHAIN_ID)
            .with_deploy_code(bytecode)
            .with_nonce(nonce);

        let tx = tx_request
            .build_typed_tx()
            .map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;
        let signature = self.signer().sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let signed_tx_bytes = signed_tx.encoded_2718().into();

        let contract_address = self.address.create(signed_tx.nonce());
        Ok((signed_tx_bytes, contract_address, signed_tx.hash().clone()))
    }

    /// Sign a TransactionRequest and return the signed bytes
    pub fn sign_txn_request(&self, tx_request: OpTransactionRequest) -> Result<(Bytes, TxHash)> {
        let tx_request = tx_request
            .from(self.address)
            .transaction_type(TransactionType::Eip1559.into())
            .with_gas_limit(500_000)
            .with_chain_id(BASE_CHAIN_ID)
            .with_max_fee_per_gas(1_000_000_000)
            .with_max_priority_fee_per_gas(0);

        let tx = tx_request
            .build_typed_tx()
            .map_err(|_| SignTxRequestError::InvalidTransactionRequest)?;
        let signature = self.signer().sign_hash_sync(&tx.signature_hash())?;
        let signed_tx = tx.into_signed(signature);
        let signed_tx_bytes = signed_tx.encoded_2718().into();
        let tx_hash = signed_tx.hash();
        Ok((signed_tx_bytes, tx_hash.clone()))
    }

    /// Constructs and returns a PrivateKeySigner for the TestAccount
    pub fn signer(&self) -> PrivateKeySigner {
        let key_bytes =
            hex::decode(self.private_key).expect("should be able to decode private key");
        let key_fixed: FixedBytes<32> = FixedBytes::from_slice(&key_bytes);
        PrivateKeySigner::from_bytes(&key_fixed)
            .expect("should be able to build the PrivateKeySigner")
            .into()
    }
}

/// Handy alias used throughout tests to refer to the deterministic `Account`.
pub type TestAccount = Account;

/// Collection of all test accounts
#[derive(Debug, Clone)]
pub struct TestAccounts {
    /// Alice (Anvil account #0) with a large starting balance.
    pub alice: TestAccount,
    /// Bob (Anvil account #1) handy for bilateral tests.
    pub bob: TestAccount,
    /// Charlie (Anvil account #2) used when three participants are required.
    pub charlie: TestAccount,
    /// Deterministic account intended for contract deployments.
    pub deployer: TestAccount,
}

impl TestAccounts {
    /// Create a new instance with all test accounts
    pub fn new() -> Self {
        Self { alice: ALICE, bob: BOB, charlie: CHARLIE, deployer: DEPLOYER }
    }

    /// Get all accounts as a vector
    pub fn all(&self) -> Vec<&TestAccount> {
        vec![&self.alice, &self.bob, &self.charlie, &self.deployer]
    }

    /// Get account by name
    pub fn get(&self, name: &str) -> Option<&TestAccount> {
        match name {
            "alice" => Some(&self.alice),
            "bob" => Some(&self.bob),
            "charlie" => Some(&self.charlie),
            "deployer" => Some(&self.deployer),
            _ => None,
        }
    }
}

impl Default for TestAccounts {
    fn default() -> Self {
        Self::new()
    }
}

// Hardcoded test accounts using Anvil's deterministic keys
// These are derived from the test mnemonic: "test test test test test test test test test test test junk"

/// Alice - First test account (Anvil account #0)
pub const ALICE: TestAccount = TestAccount {
    name: "Alice",
    address: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
    private_key: "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
};

/// Bob - Second test account (Anvil account #1)
pub const BOB: TestAccount = TestAccount {
    name: "Bob",
    address: address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
    private_key: "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
};

/// Charlie - Third test account (Anvil account #2)
pub const CHARLIE: TestAccount = TestAccount {
    name: "Charlie",
    address: address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
    private_key: "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
};

/// Deployer - Account for deploying smart contracts (Anvil account #3)
pub const DEPLOYER: TestAccount = TestAccount {
    name: "Deployer",
    address: address!("90F79bf6EB2c4f870365E785982E1f101E93b906"),
    private_key: "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
};
