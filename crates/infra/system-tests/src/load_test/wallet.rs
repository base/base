use std::{fs, path::Path};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

/// Serializable wallet data for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WalletData {
    /// Hex-encoded wallet address.
    pub address: String,
    /// Hex-encoded private key.
    pub private_key: String,
    /// Initial balance as a string.
    pub initial_balance: String,
}

/// Container for serializing/deserializing a collection of wallets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WalletsFile {
    /// List of wallet data entries.
    pub wallets: Vec<WalletData>,
}

/// A wallet with a signer and address for load testing.
#[derive(Debug)]
pub(crate) struct Wallet {
    /// Local private key signer.
    pub signer: PrivateKeySigner,
    /// Wallet address.
    pub address: Address,
}

impl Wallet {
    /// Creates a wallet from a hex-encoded private key string.
    pub(crate) fn from_private_key(private_key: &str) -> Result<Self> {
        let signer: PrivateKeySigner =
            private_key.parse().context("Failed to parse private key")?;
        let address = signer.address();
        Ok(Self { signer, address })
    }

    /// Generates a random wallet from the given RNG.
    pub(crate) fn new_random(rng: &mut ChaCha8Rng) -> Self {
        let mut key_bytes = [0u8; 32];
        rng.fill_bytes(&mut key_bytes);
        let signer = PrivateKeySigner::from_slice(&key_bytes).expect("valid random key bytes");
        let address = signer.address();
        Self { signer, address }
    }
}

/// Generates a set of random wallets, optionally seeded for reproducibility.
pub(crate) fn generate_wallets(num_wallets: usize, seed: Option<u64>) -> Vec<Wallet> {
    let mut rng = seed.map_or_else(ChaCha8Rng::from_os_rng, ChaCha8Rng::seed_from_u64);

    (0..num_wallets).map(|_| Wallet::new_random(&mut rng)).collect()
}

/// Saves wallet data to a JSON file.
pub(crate) fn save_wallets(wallets: &[Wallet], fund_amount: f64, path: &Path) -> Result<()> {
    let wallet_data: Vec<WalletData> = wallets
        .iter()
        .map(|w| WalletData {
            address: format!("{:?}", w.address),
            private_key: format!("0x{}", hex::encode(w.signer.to_bytes())),
            initial_balance: fund_amount.to_string(),
        })
        .collect();

    let wallets_file = WalletsFile { wallets: wallet_data };

    let json =
        serde_json::to_string_pretty(&wallets_file).context("Failed to serialize wallets")?;
    fs::write(path, json).context("Failed to write wallets file")?;

    Ok(())
}

/// Loads wallets from a JSON file.
pub(crate) fn load_wallets(path: &Path) -> Result<Vec<Wallet>> {
    let json = fs::read_to_string(path).context("Failed to read wallets file")?;
    let wallets_file: WalletsFile =
        serde_json::from_str(&json).context("Failed to parse wallets file")?;

    let wallets: Result<Vec<Wallet>> =
        wallets_file.wallets.iter().map(|wd| Wallet::from_private_key(&wd.private_key)).collect();

    wallets
}
