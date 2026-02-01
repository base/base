//! Anvil default test accounts with known private keys.

use alloy_primitives::{Address, B256, b256};
use alloy_signer_local::PrivateKeySigner;

/// Anvil account with address and private key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    /// Account address.
    pub address: Address,
    /// Account private key.
    pub private_key: B256,
}

impl Account {
    /// Creates a new account by deriving the address from the private key.
    pub fn from_private_key(private_key: B256) -> Self {
        let signer = PrivateKeySigner::from_bytes(&private_key).expect("valid private key");
        Self { address: signer.address(), private_key }
    }
}

/// Anvil account #0.
pub static ANVIL_ACCOUNT_0: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_0));
/// Anvil account #1.
pub static ANVIL_ACCOUNT_1: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_1));
/// Anvil account #2.
pub static ANVIL_ACCOUNT_2: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_2));
/// Anvil account #3.
pub static ANVIL_ACCOUNT_3: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_3));
/// Anvil account #4.
pub static ANVIL_ACCOUNT_4: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_4));
/// Anvil account #5.
pub static ANVIL_ACCOUNT_5: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_5));
/// Anvil account #6.
pub static ANVIL_ACCOUNT_6: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_6));
/// Anvil account #7.
pub static ANVIL_ACCOUNT_7: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_7));
/// Anvil account #8.
pub static ANVIL_ACCOUNT_8: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_8));
/// Anvil account #9.
pub static ANVIL_ACCOUNT_9: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| Account::from_private_key(ANVIL_PRIVATE_KEY_9));

const ANVIL_PRIVATE_KEY_0: B256 =
    b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
const ANVIL_PRIVATE_KEY_1: B256 =
    b256!("0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");
const ANVIL_PRIVATE_KEY_2: B256 =
    b256!("0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a");
const ANVIL_PRIVATE_KEY_3: B256 =
    b256!("0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6");
const ANVIL_PRIVATE_KEY_4: B256 =
    b256!("0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a");
const ANVIL_PRIVATE_KEY_5: B256 =
    b256!("0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba");
const ANVIL_PRIVATE_KEY_6: B256 =
    b256!("0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e");
const ANVIL_PRIVATE_KEY_7: B256 =
    b256!("0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356");
const ANVIL_PRIVATE_KEY_8: B256 =
    b256!("0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97");
const ANVIL_PRIVATE_KEY_9: B256 =
    b256!("0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6");

/// Deployer role account alias (Anvil account 0).
pub static DEPLOYER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_0);
/// Sequencer role account alias (Anvil account 5).
pub static SEQUENCER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_5);
/// Batcher role account alias (Anvil account 6).
pub static BATCHER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_6);
/// Proposer role account alias (Anvil account 7).
pub static PROPOSER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_7);
/// Challenger role account alias (Anvil account 8).
pub static CHALLENGER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_8);

/// Returns the default Anvil account addresses.
pub fn anvil_addresses() -> Vec<Address> {
    [
        ANVIL_ACCOUNT_0.address,
        ANVIL_ACCOUNT_1.address,
        ANVIL_ACCOUNT_2.address,
        ANVIL_ACCOUNT_3.address,
        ANVIL_ACCOUNT_4.address,
        ANVIL_ACCOUNT_5.address,
        ANVIL_ACCOUNT_6.address,
        ANVIL_ACCOUNT_7.address,
        ANVIL_ACCOUNT_8.address,
        ANVIL_ACCOUNT_9.address,
    ]
    .to_vec()
}
