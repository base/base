//! Private keys the generator controls.

use std::sync::OnceLock;

use alloy_primitives::{Address, B256, keccak256};
use alloy_signer_local::PrivateKeySigner;

/// The fixed set of keys used by generated chains.
///
/// Keys are derived from fixed labels, not from the seed, so the same addresses appear in every
/// case; balances and roles vary per case.
#[derive(Debug)]
pub struct Keys {
    /// Signers; index [`Self::OPERATOR`] holds every privileged role the generator can assign.
    pub signers: Vec<PrivateKeySigner>,
}

impl Keys {
    /// Number of generated keys.
    pub const COUNT: usize = 24;

    /// Index of the operator key: activation admin from Cobalt, policy and token admin.
    pub const OPERATOR: usize = 0;

    /// Index of the key whose account nonce is `u64::MAX`, so its transactions are invalid.
    pub const MAX_NONCE: usize = 20;

    /// Index of the key whose account has non-delegation code, so its transactions are invalid
    /// (EIP-3607).
    pub const WITH_CODE: usize = 21;

    /// The shared key set.
    pub fn get() -> &'static Self {
        static KEYS: OnceLock<Keys> = OnceLock::new();
        KEYS.get_or_init(|| {
            let signers = (0..Self::COUNT)
                .map(|i| {
                    let secret: B256 = keccak256(format!("base-diff-fuzz key {i}"));
                    PrivateKeySigner::from_bytes(&secret).expect("keccak output is a valid key")
                })
                .collect();
            Self { signers }
        })
    }

    /// Address of key `index`.
    pub fn address(&self, index: usize) -> Address {
        self.signers[index].address()
    }

    /// Address of the operator key.
    pub fn operator(&self) -> Address {
        self.address(Self::OPERATOR)
    }

    /// Index of the key controlling `address`.
    pub fn index_of(&self, address: &Address) -> Option<usize> {
        self.signers.iter().position(|signer| signer.address() == *address)
    }
}
