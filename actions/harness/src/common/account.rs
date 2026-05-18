use alloy_consensus::SignableTransaction;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base_common_consensus::BaseTxEnvelope;

/// Hardcoded private key for the test account used across all action tests.
///
/// The corresponding address is deterministic: derive it via
/// `PrivateKeySigner::from_bytes(&TEST_ACCOUNT_KEY).unwrap().address()`.
/// Tests that need to fund the account should include it in the genesis
/// allocation with a sufficient ETH balance.
pub const TEST_ACCOUNT_KEY: B256 = B256::new([0x01u8; 32]);

/// The L2 address derived from [`TEST_ACCOUNT_KEY`].
///
/// Pre-computed so callers can reference it without constructing a signer.
// Address derived from the secp256k1 public key of [0x01; 32].
pub const TEST_ACCOUNT_ADDRESS: Address =
    alloy_primitives::address!("1a642f0E3c3aF545E7AcBD38b07251B3990914F1");

/// A test account with nonce tracking and signing capability.
///
/// Wraps a [`PrivateKeySigner`] with an auto-incrementing nonce so callers
/// can build correctly-sequenced signed transactions without manual bookkeeping.
/// Shared via [`Arc`] so the sequencer and external test code stay in sync.
///
/// [`Arc`]: std::sync::Arc
#[derive(Debug)]
pub struct TestAccount {
    signer: PrivateKeySigner,
    nonce: u64,
}

impl TestAccount {
    /// Create a new test account from a private key with nonce starting at 0.
    pub fn new(key: B256) -> Self {
        let signer = PrivateKeySigner::from_bytes(&key).expect("valid key");
        Self { signer, nonce: 0 }
    }

    /// Return the address derived from this account's private key.
    pub const fn address(&self) -> Address {
        self.signer.address()
    }

    /// Sign a pre-built EIP-1559 transaction without modifying the nonce.
    ///
    /// The caller is responsible for setting the correct nonce in the
    /// transaction fields before calling this method.
    pub fn sign_tx(
        &mut self,
        tx: alloy_consensus::TxEip1559,
    ) -> Result<BaseTxEnvelope, alloy_signer::Error> {
        let sig = self.signer.sign_hash_sync(&tx.signature_hash())?;
        Ok(BaseTxEnvelope::Eip1559(tx.into_signed(sig)))
    }

    /// Creates and signs a minimal EIP-1559 transfer, auto-incrementing the nonce.
    pub fn create_eip1559_tx(&mut self, chain_id: u64) -> BaseTxEnvelope {
        self.create_tx(chain_id, TxKind::Call(Address::ZERO), Bytes::new(), U256::from(1), 21_000)
    }

    /// Creates and signs a custom EIP-1559 transaction, auto-incrementing the nonce.
    ///
    /// The caller provides the destination, calldata, value, and gas limit.
    /// Chain-level fields (`chain_id`, `nonce`, fee caps) are filled in automatically.
    pub fn create_tx(
        &mut self,
        chain_id: u64,
        to: TxKind,
        input: Bytes,
        value: U256,
        gas_limit: u64,
    ) -> BaseTxEnvelope {
        let tx = alloy_consensus::TxEip1559 {
            chain_id,
            nonce: self.nonce,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 1_000_000,
            gas_limit,
            to,
            value,
            input,
            access_list: Default::default(),
        };
        let sig = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("test account signing must not fail");
        self.nonce += 1;
        BaseTxEnvelope::Eip1559(tx.into_signed(sig))
    }

    /// Return the current nonce.
    pub const fn nonce(&self) -> u64 {
        self.nonce
    }
}
