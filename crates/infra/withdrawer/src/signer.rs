use std::fmt;

use alloy_consensus::SignableTransaction;
#[cfg(feature = "ledger")]
use alloy_consensus::TxType;
use alloy_network::TxSigner;
use alloy_primitives::{Address, Signature};
#[cfg(feature = "ledger")]
use alloy_primitives::{B256, normalize_v};
#[cfg(feature = "ledger")]
use alloy_signer_ledger::{HDPath, LedgerError, LedgerSigner};
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use thiserror::Error;

use crate::SignerSource;

/// Software or opt-in Ledger transaction signer.
pub enum WithdrawalSigner {
    /// Local private key, supplied directly or derived from a mnemonic.
    Local(PrivateKeySigner),
    /// Ledger Ethereum app.
    #[cfg(feature = "ledger")]
    Ledger(LedgerSigner),
}

impl fmt::Debug for WithdrawalSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WithdrawalSigner")
            .field("address", &self.address())
            .field("ledger", &self.is_ledger())
            .finish_non_exhaustive()
    }
}

/// Redacted signer construction and validation errors.
#[derive(Debug, Error)]
pub enum WithdrawalSignerError {
    /// The private key is not a nonzero secp256k1 scalar.
    #[error("invalid private key; expected a nonzero secp256k1 scalar")]
    InvalidPrivateKey,
    /// Mnemonic key derivation failed.
    #[error("could not derive mnemonic signing key")]
    Mnemonic,
    /// The derivation path exceeds the BIP-32 depth limit.
    #[error("derivation path must have at most {} components", SignerSource::MAX_DERIVATION_DEPTH)]
    DerivationPathTooDeep,
    /// The Ledger could not be opened or queried for its address.
    #[cfg(feature = "ledger")]
    #[error("could not connect to Ledger; connect and unlock it, then open the Ethereum app")]
    LedgerConnection(#[source] LedgerError),
    /// The Ledger could not sign the transaction.
    #[cfg(feature = "ledger")]
    #[error("Ledger transaction signing failed")]
    LedgerSigning(#[source] alloy_signer::Error),
    /// Signature recovery did not yield the expected signing address.
    #[cfg(feature = "ledger")]
    #[error("Ledger signature does not recover to the signing address")]
    SignatureMismatch,
    /// Legacy signing cannot represent both parity values on this chain.
    #[cfg(feature = "ledger")]
    #[error(
        "Ledger legacy signing is unsupported for chain ID {chain_id}; use EIP-1559 fees instead"
    )]
    UnsupportedLedgerLegacyChain {
        /// Transaction chain ID.
        chain_id: u64,
    },
}

impl WithdrawalSigner {
    /// Creates a signer, deriving a local key or connecting to a Ledger without RPC.
    pub async fn new(source: &SignerSource) -> Result<Self, WithdrawalSignerError> {
        match source {
            SignerSource::PrivateKey(key) => PrivateKeySigner::from_bytes(&key.0)
                .map(Self::Local)
                .map_err(|_| WithdrawalSignerError::InvalidPrivateKey),
            SignerSource::Mnemonic { phrase, path } => {
                if path.len() > SignerSource::MAX_DERIVATION_DEPTH {
                    return Err(WithdrawalSignerError::DerivationPathTooDeep);
                }
                let key =
                    phrase.0.derive_key(path, None).map_err(|_| WithdrawalSignerError::Mnemonic)?;
                Ok(Self::Local(PrivateKeySigner::from_signing_key(Clone::clone(key.as_ref()))))
            }
            #[cfg(feature = "ledger")]
            SignerSource::Ledger { path } => {
                if path.len() > SignerSource::MAX_DERIVATION_DEPTH {
                    return Err(WithdrawalSignerError::DerivationPathTooDeep);
                }
                LedgerSigner::new(HDPath::Other(path.derivation_string()), None)
                    .await
                    .map(Self::Ledger)
                    .map_err(WithdrawalSignerError::LedgerConnection)
            }
        }
    }

    /// Returns whether signing requires a Ledger device.
    pub const fn is_ledger(&self) -> bool {
        match self {
            Self::Local(_) => false,
            #[cfg(feature = "ledger")]
            Self::Ledger(_) => true,
        }
    }

    /// Rejects legacy chain IDs whose truncated EIP-155 parity cannot be decoded reliably.
    #[cfg(feature = "ledger")]
    pub fn check_ledger_legacy_chain(chain_id: u64) -> Result<(), WithdrawalSignerError> {
        for parity in [false, true] {
            // The device returns only the low byte of EIP-155 v, without the chain ID.
            let v = chain_id.wrapping_mul(2).wrapping_add(35 + u64::from(parity)) & 0xff;
            if normalize_v(v) != Some(parity) {
                return Err(WithdrawalSignerError::UnsupportedLedgerLegacyChain { chain_id });
            }
        }
        Ok(())
    }

    /// Checks that a device signature recovers to its expected address for the prehash.
    #[cfg(feature = "ledger")]
    pub fn verify_signature(
        signature: &Signature,
        prehash: &B256,
        address: Address,
    ) -> Result<(), WithdrawalSignerError> {
        if signature.recover_address_from_prehash(prehash).ok() != Some(address) {
            return Err(WithdrawalSignerError::SignatureMismatch);
        }
        Ok(())
    }
}

#[async_trait]
impl TxSigner<Signature> for WithdrawalSigner {
    fn address(&self) -> Address {
        match self {
            Self::Local(signer) => signer.address(),
            #[cfg(feature = "ledger")]
            Self::Ledger(signer) => TxSigner::address(signer),
        }
    }

    async fn sign_transaction(
        &self,
        transaction: &mut dyn SignableTransaction<Signature>,
    ) -> alloy_signer::Result<Signature> {
        match self {
            Self::Local(signer) => signer.sign_transaction(transaction).await,
            #[cfg(feature = "ledger")]
            Self::Ledger(signer) => {
                if transaction.ty() == TxType::Legacy as u8
                    && let Some(chain_id) = transaction.chain_id()
                {
                    Self::check_ledger_legacy_chain(chain_id)
                        .map_err(alloy_signer::Error::other)?;
                }
                let signature = signer.sign_transaction(transaction).await.map_err(|error| {
                    alloy_signer::Error::other(WithdrawalSignerError::LedgerSigning(error))
                })?;
                Self::verify_signature(&signature, &transaction.signature_hash(), self.address())
                    .map_err(alloy_signer::Error::other)?;
                Ok(signature)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, TxLegacy, TypedTransaction};
    use alloy_network::TxSigner;
    #[cfg(feature = "ledger")]
    use alloy_primitives::{Address, Signature, keccak256};
    use alloy_primitives::{B256, U256, address};
    #[cfg(feature = "ledger")]
    use alloy_signer::SignerSync;
    #[cfg(feature = "ledger")]
    use alloy_signer_local::PrivateKeySigner;

    use crate::{Secret, SignerSource, WithdrawalSigner, WithdrawalSignerError, WithdrawerArgs};

    const PHRASE: &str = "test test test test test test test test test test test junk";

    fn mnemonic(path: &str) -> SignerSource {
        SignerSource::Mnemonic {
            phrase: Secret(PHRASE.parse().unwrap()),
            path: path.parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn mnemonic_derives_standard_ethereum_accounts() {
        for (path, expected) in [
            ("m/44'/60'/0'/0/0", address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")),
            ("m/44'/60'/0'/0/1", address!("70997970C51812dc3A010C7d01b50e0d17dc79C8")),
        ] {
            let signer = WithdrawalSigner::new(&mnemonic(path))
                .await
                .expect("standard mnemonic derivation must succeed");
            assert_eq!(signer.address(), expected);
        }
    }

    #[tokio::test]
    async fn mnemonic_accepts_short_hardened_path() {
        WithdrawalSigner::new(&mnemonic("m/321'/0'"))
            .await
            .expect("a short hardened BIP-32 path must derive successfully");
    }

    #[tokio::test]
    async fn derivation_depth_accepts_255_and_rejects_256() {
        let path = format!("m{}", "/0".repeat(255));
        WithdrawerArgs::parse_hd_path(&path).expect("255 components must parse");
        WithdrawalSigner::new(&mnemonic(&path)).await.expect("255 levels must derive");

        let path = format!("{path}/0");
        assert_eq!(
            WithdrawerArgs::parse_hd_path(&path).unwrap_err(),
            "derivation path must have at most 255 components"
        );
        assert!(matches!(
            WithdrawalSigner::new(&mnemonic(&path)).await,
            Err(WithdrawalSignerError::DerivationPathTooDeep)
        ));
        #[cfg(feature = "ledger")]
        assert!(matches!(
            WithdrawalSigner::new(&SignerSource::Ledger { path: path.parse().unwrap() }).await,
            Err(WithdrawalSignerError::DerivationPathTooDeep)
        ));
    }

    #[tokio::test]
    async fn private_key_rejects_zero_without_disclosing_it() {
        let key = B256::ZERO;
        let error = WithdrawalSigner::new(&SignerSource::PrivateKey(Secret(key)))
            .await
            .expect_err("zero scalar was accepted");
        assert!(!error.to_string().contains(&format!("{key:x}")));
        assert!(!format!("{error:?}").contains(&format!("{key:x}")));
    }

    #[tokio::test]
    async fn private_key_rejects_curve_order_and_larger_without_disclosing_them() {
        for key in [
            "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
                .parse::<B256>()
                .unwrap(),
            B256::repeat_byte(0xff),
        ] {
            let error = WithdrawalSigner::new(&SignerSource::PrivateKey(Secret(key)))
                .await
                .expect_err("out-of-range scalar was accepted");
            assert!(!error.to_string().contains(&format!("{key:x}")));
            assert!(!format!("{error:?}").contains(&format!("{key:x}")));
        }
    }

    #[tokio::test]
    async fn local_sources_sign_legacy_and_eip1559_transactions() {
        for (source, expected) in [
            (
                SignerSource::PrivateKey(Secret(B256::with_last_byte(1))),
                address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf"),
            ),
            (mnemonic("m/44'/60'/0'/0/0"), address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")),
        ] {
            let signer = WithdrawalSigner::new(&source).await.unwrap();
            for mut transaction in [
                TypedTransaction::Legacy(TxLegacy {
                    chain_id: Some(111),
                    gas_price: 1,
                    gas_limit: 21_000,
                    value: U256::from(17),
                    ..Default::default()
                }),
                TypedTransaction::Eip1559(TxEip1559 {
                    chain_id: 8453,
                    nonce: 5,
                    max_fee_per_gas: 2,
                    max_priority_fee_per_gas: 1,
                    gas_limit: 21_000,
                    ..Default::default()
                }),
            ] {
                let signature = signer.sign_transaction(&mut transaction).await.unwrap();
                assert_eq!(
                    signature.recover_address_from_prehash(&transaction.signature_hash()).unwrap(),
                    expected,
                );
            }
        }
    }

    #[tokio::test]
    async fn signer_debug_omits_private_key_and_mnemonic() {
        let key = B256::repeat_byte(0x42);
        for source in [SignerSource::PrivateKey(Secret(key)), mnemonic("m/44'/60'/0'/0/0")] {
            let signer = WithdrawalSigner::new(&source).await.unwrap();
            let debug = format!("{signer:?}");
            assert!(!debug.contains(&format!("{key:x}")));
            assert!(!debug.contains(PHRASE));
        }
    }

    #[cfg(feature = "ledger")]
    #[test]
    fn ledger_legacy_chain_check_rejects_truncated_parity() {
        for chain_id in [1, 11155111, 8453] {
            WithdrawalSigner::check_ledger_legacy_chain(chain_id).unwrap();
        }
        for chain_id in [111, u64::MAX] {
            let error = WithdrawalSigner::check_ledger_legacy_chain(chain_id)
                .expect_err("chain has an ambiguous device parity byte");
            assert!(error.to_string().contains("EIP-1559"));
        }
    }

    #[cfg(feature = "ledger")]
    #[test]
    fn ledger_signature_must_recover_expected_address_and_prehash() {
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(1)).unwrap();
        let prehash = keccak256(b"withdrawal transaction");
        let signature = signer.sign_hash_sync(&prehash).unwrap();
        WithdrawalSigner::verify_signature(&signature, &prehash, signer.address()).unwrap();
        assert!(
            WithdrawalSigner::verify_signature(
                &Signature::new(U256::ZERO, U256::ZERO, false),
                &prehash,
                Address::ZERO,
            )
            .is_err()
        );
        assert!(
            WithdrawalSigner::verify_signature(
                &signature.with_parity(!signature.v()),
                &prehash,
                signer.address(),
            )
            .is_err(),
            "incorrect recovery parity was accepted"
        );
        assert!(
            WithdrawalSigner::verify_signature(
                &signature,
                &prehash,
                address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
            )
            .is_err()
        );
        assert!(
            WithdrawalSigner::verify_signature(
                &signature,
                &keccak256(b"different transaction"),
                signer.address(),
            )
            .is_err()
        );
    }
}
