use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletData {
    pub address: String,
    pub private_key: String,
    pub initial_balance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletsFile {
    pub wallets: Vec<WalletData>,
}

pub struct Wallet {
    pub signer: PrivateKeySigner,
    pub address: Address,
}

impl Wallet {
    pub fn from_private_key(private_key: &str) -> Result<Self> {
        let signer: PrivateKeySigner =
            private_key.parse().context("Failed to parse private key")?;
        let address = signer.address();
        Ok(Self { signer, address })
    }

    pub fn new_random(rng: &mut ChaCha8Rng) -> Self {
        let signer = PrivateKeySigner::random_with(rng);
        let address = signer.address();
        Self { signer, address }
    }
}

pub fn generate_wallets(num_wallets: usize, seed: Option<u64>) -> Vec<Wallet> {
    let mut rng = match seed {
        Some(s) => ChaCha8Rng::seed_from_u64(s),
        None => ChaCha8Rng::from_entropy(),
    };

    (0..num_wallets)
        .map(|_| Wallet::new_random(&mut rng))
        .collect()
}

pub fn save_wallets(wallets: &[Wallet], fund_amount: f64, path: &Path) -> Result<()> {
    let wallet_data: Vec<WalletData> = wallets
        .iter()
        .map(|w| WalletData {
            address: format!("{:?}", w.address),
            private_key: format!("0x{}", hex::encode(w.signer.to_bytes())),
            initial_balance: fund_amount.to_string(),
        })
        .collect();

    let wallets_file = WalletsFile {
        wallets: wallet_data,
    };

    let json =
        serde_json::to_string_pretty(&wallets_file).context("Failed to serialize wallets")?;
    fs::write(path, json).context("Failed to write wallets file")?;

    Ok(())
}

pub fn load_wallets(path: &Path) -> Result<Vec<Wallet>> {
    let json = fs::read_to_string(path).context("Failed to read wallets file")?;
    let wallets_file: WalletsFile =
        serde_json::from_str(&json).context("Failed to parse wallets file")?;

    let wallets: Result<Vec<Wallet>> = wallets_file
        .wallets
        .iter()
        .map(|wd| Wallet::from_private_key(&wd.private_key))
        .collect();

    wallets
}
