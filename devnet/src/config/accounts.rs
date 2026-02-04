//! Anvil default test accounts derived from mnemonic.

use alloy_primitives::{Address, B256, FixedBytes};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};

/// Standard Anvil test mnemonic.
pub const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";

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

/// Derives an account from the test mnemonic at the given index.
fn derive_account(index: u32) -> Account {
    let path = format!("m/44'/60'/0'/0/{index}");
    let signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .derivation_path(&path)
        .expect("valid derivation path")
        .build()
        .expect("valid signer");
    let key_bytes = FixedBytes::<32>::from_slice(signer.credential().to_bytes().as_slice());
    Account { address: signer.address(), private_key: B256::from(key_bytes) }
}

/// Anvil account #0.
pub static ANVIL_ACCOUNT_0: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(0));
/// Anvil account #1.
pub static ANVIL_ACCOUNT_1: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(1));
/// Anvil account #2.
pub static ANVIL_ACCOUNT_2: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(2));
/// Anvil account #3.
pub static ANVIL_ACCOUNT_3: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(3));
/// Anvil account #4.
pub static ANVIL_ACCOUNT_4: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(4));
/// Anvil account #5.
pub static ANVIL_ACCOUNT_5: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(5));
/// Anvil account #6.
pub static ANVIL_ACCOUNT_6: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(6));
/// Anvil account #7.
pub static ANVIL_ACCOUNT_7: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(7));
/// Anvil account #8.
pub static ANVIL_ACCOUNT_8: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(8));
/// Anvil account #9.
pub static ANVIL_ACCOUNT_9: std::sync::LazyLock<Account> =
    std::sync::LazyLock::new(|| derive_account(9));

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
/// Builder role account alias (Anvil account 9).
pub static BUILDER: std::sync::LazyLock<Account> = std::sync::LazyLock::new(|| *ANVIL_ACCOUNT_9);

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
