//! Test accounts with pre-funded balances for integration testing

use alloy_primitives::{address, Address};

/// Hardcoded test account with a fixed private key
#[derive(Debug, Clone)]
pub struct TestAccount {
    /// Account name for easy identification
    pub name: &'static str,
    /// Ethereum address
    pub address: Address,
    /// Private key (hex string without 0x prefix)
    pub private_key: &'static str,
}

/// Collection of all test accounts
#[derive(Debug, Clone)]
pub struct TestAccounts {
    pub alice: TestAccount,
    pub bob: TestAccount,
    pub charlie: TestAccount,
    pub deployer: TestAccount,
}

impl TestAccounts {
    /// Create a new instance with all test accounts
    pub fn new() -> Self {
        Self {
            alice: ALICE,
            bob: BOB,
            charlie: CHARLIE,
            deployer: DEPLOYER,
        }
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
