use alloy_network::EthereumWallet;
use alloy_primitives::{Address, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use rand::{Rng, SeedableRng, rngs::StdRng};

use crate::utils::{BaselineError, Result};

/// An account with funding and signing capability.
#[derive(Debug, Clone)]
pub struct FundedAccount {
    /// Account address.
    pub address: Address,
    /// Signer for the account.
    pub signer: PrivateKeySigner,
    /// Current balance.
    pub balance: U256,
    /// Current nonce.
    pub nonce: u64,
}

impl FundedAccount {
    /// Creates a new account from a signer.
    pub const fn new(signer: PrivateKeySigner) -> Self {
        let address = signer.address();
        Self { address, signer, balance: U256::ZERO, nonce: 0 }
    }

    /// Sets the account balance.
    pub const fn with_balance(mut self, balance: U256) -> Self {
        self.balance = balance;
        self
    }

    /// Sets the account nonce.
    pub const fn with_nonce(mut self, nonce: u64) -> Self {
        self.nonce = nonce;
        self
    }

    /// Converts the account into an Ethereum wallet.
    pub fn into_wallet(self) -> EthereumWallet {
        EthereumWallet::from(self.signer)
    }

    /// Gets the next nonce and increments.
    pub const fn next_nonce(&mut self) -> u64 {
        let nonce = self.nonce;
        self.nonce += 1;
        nonce
    }
}

/// A pool of funded accounts for workload execution.
#[derive(Debug, Clone)]
pub struct AccountPool {
    accounts: Vec<FundedAccount>,
    rng: StdRng,
}

impl AccountPool {
    /// Creates a new pool of accounts.
    pub fn new(seed: u64, count: usize) -> Result<Self> {
        Self::with_offset(seed, count, 0)
    }

    /// Creates a new pool of accounts, skipping the first `offset` accounts.
    pub fn with_offset(seed: u64, count: usize, offset: usize) -> Result<Self> {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut accounts = Vec::with_capacity(count);

        for _ in 0..offset {
            let mut skip_bytes = [0u8; 32];
            rng.fill(&mut skip_bytes);
        }

        for _ in 0..count {
            let mut key_bytes = [0u8; 32];
            rng.fill(&mut key_bytes);

            let signer = PrivateKeySigner::from_bytes(&key_bytes.into()).map_err(|e| {
                BaselineError::Account { address: Address::ZERO, message: e.to_string() }
            })?;

            accounts.push(FundedAccount::new(signer));
        }

        Ok(Self { accounts, rng })
    }

    /// Creates a pool of accounts derived from a mnemonic phrase.
    pub fn from_mnemonic(mnemonic: &str, count: usize, offset: usize) -> Result<Self> {
        let mut accounts = Vec::with_capacity(count);

        for i in 0..count {
            let index = u32::try_from(offset + i).map_err(|_| {
                BaselineError::Config(format!("mnemonic index {} exceeds u32::MAX", offset + i))
            })?;
            let signer = MnemonicBuilder::<English>::default()
                .phrase(mnemonic)
                .index(index)
                .map_err(|e| BaselineError::Config(format!("invalid mnemonic index {index}: {e}")))?
                .build()
                .map_err(|e| BaselineError::Config(format!("failed to derive key: {e}")))?;

            accounts.push(FundedAccount::new(signer));
        }

        let rng = StdRng::seed_from_u64(0);
        Ok(Self { accounts, rng })
    }

    /// Returns the number of accounts in the pool.
    pub const fn len(&self) -> usize {
        self.accounts.len()
    }

    /// Checks if the pool is empty.
    pub const fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Returns a slice of all accounts.
    pub fn accounts(&self) -> &[FundedAccount] {
        &self.accounts
    }

    /// Returns a mutable slice of all accounts.
    pub fn accounts_mut(&mut self) -> &mut [FundedAccount] {
        &mut self.accounts
    }

    /// Gets an account by index.
    pub fn get(&self, index: usize) -> Option<&FundedAccount> {
        self.accounts.get(index)
    }

    /// Gets a mutable account by index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut FundedAccount> {
        self.accounts.get_mut(index)
    }

    /// Gets a random account from the pool.
    pub fn random_account(&mut self) -> &mut FundedAccount {
        let index = self.rng.random_range(0..self.accounts.len());
        &mut self.accounts[index]
    }

    /// Returns all addresses in the pool.
    pub fn addresses(&self) -> Vec<Address> {
        self.accounts.iter().map(|a| a.address).collect()
    }
}
