//! Lazy key stream for deterministic `secp256k1` signer derivation.

use alloy_signer_local::{MnemonicBuilder, MnemonicKey, PrivateKeySigner, coins_bip39::English};
use rand::{Rng, SeedableRng, rngs::StdRng};

use crate::utils::{BaselineError, Result};

const SEED_SIGNER_MAX_ATTEMPTS: usize = 16;
const MAX_SEED_KEY_STREAM_OFFSET: usize = 10_000_000;

/// Lazy stream of `secp256k1` signing keys used for sender pool generation
/// and on-demand recipient generation in fresh-recipient mode.
///
/// Both constructors advance one key per [`KeyStream::next_signer`] call, so a caller that
/// pre-skips `offset` keys and then takes `n` keys produces the same sequence as
/// `AccountPool::with_offset(_, n, offset)` /
/// `AccountPool::from_mnemonic(_, n, offset)`. This is the contract that lets
/// users recover recipient addresses out-of-band.
#[derive(Debug)]
pub struct KeyStream(KeyStreamInner);

#[derive(Debug)]
enum KeyStreamInner {
    /// `StdRng`-driven derivation: each `next_signer` consumes 32 bytes per attempt.
    /// Boxed because `StdRng` is ~256 bytes and dwarfs the other variant.
    Seed { rng: Box<StdRng>, seed: u64, offset: usize, generated: u64 },
    /// BIP39 derivation: each `next_signer` advances `next_index`.
    Mnemonic {
        /// Parent key at `m/44'/60'/0'/0`, cached to avoid PBKDF2 per signer.
        key: MnemonicKey,
        /// Initial BIP39 child index used to position this stream.
        offset: usize,
        /// BIP39 child index for the next [`KeyStream::next_signer`] call.
        next_index: u32,
        /// Number of signers produced by this stream.
        generated: u64,
    },
}

impl KeyStream {
    /// Builds a seed-driven stream positioned `offset` keys in. Each skipped
    /// position consumes 32 bytes from the underlying RNG, matching
    /// `AccountPool::with_offset`. Offsets are capped because positioning this
    /// stream is O(offset).
    pub fn from_seed(seed: u64, offset: usize) -> Result<Self> {
        if offset > MAX_SEED_KEY_STREAM_OFFSET {
            return Err(BaselineError::Config(format!(
                "seed key stream offset {offset} exceeds maximum supported offset \
                 {MAX_SEED_KEY_STREAM_OFFSET}"
            )));
        }

        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..offset {
            let mut skip = [0u8; 32];
            rng.fill(&mut skip);
        }
        Ok(Self(KeyStreamInner::Seed { rng: Box::new(rng), seed, offset, generated: 0 }))
    }

    /// Builds a mnemonic-driven stream positioned at BIP39 index `offset`.
    pub fn from_mnemonic(phrase: impl Into<String>, offset: usize) -> Result<Self> {
        let next_index = u32::try_from(offset).map_err(|_| {
            BaselineError::Config(format!("mnemonic index {offset} exceeds u32::MAX"))
        })?;
        let key = MnemonicBuilder::<English>::default().phrase(phrase).build_parent_key().map_err(
            |e| BaselineError::Config(format!("failed to derive mnemonic parent key: {e}")),
        )?;
        Ok(Self(KeyStreamInner::Mnemonic { key, offset, next_index, generated: 0 }))
    }

    /// Returns instructions for recovering keys produced by this stream.
    pub fn recovery_message(&self) -> String {
        match &self.0 {
            KeyStreamInner::Seed { seed, offset, .. } => {
                format!(
                    "Fresh-recipient mode: seed={seed} recipient_offset={offset} \
                     (recover with AccountPool::with_offset(seed, fresh_recipient_count, \
                     recipient_offset))"
                )
            }
            KeyStreamInner::Mnemonic { offset, .. } => {
                format!(
                    "Fresh-recipient mode: recipient_offset={offset} \
                     (recover with AccountPool::from_mnemonic(mnemonic, fresh_recipient_count, \
                     recipient_offset))"
                )
            }
        }
    }

    /// Returns the number of signers produced by this stream.
    pub const fn generated_count(&self) -> u64 {
        match &self.0 {
            KeyStreamInner::Seed { generated, .. } | KeyStreamInner::Mnemonic { generated, .. } => {
                *generated
            }
        }
    }

    /// Yields the next signer in the stream.
    ///
    /// For `Seed`, the (vanishingly rare) case of an invalid secp256k1 scalar
    /// is handled by drawing again up to a bounded retry count. For `Mnemonic`,
    /// returns an error if the next index would overflow `u32::MAX` or if BIP39
    /// derivation fails.
    pub fn next_signer(&mut self) -> Result<PrivateKeySigner> {
        match &mut self.0 {
            KeyStreamInner::Seed { rng, generated, .. } => {
                for _ in 0..SEED_SIGNER_MAX_ATTEMPTS {
                    let mut bytes = [0u8; 32];
                    rng.fill(&mut bytes);
                    if let Ok(signer) = PrivateKeySigner::from_bytes(&bytes.into()) {
                        *generated = generated.saturating_add(1);
                        return Ok(signer);
                    }
                }
                Err(BaselineError::Account {
                    address: alloy_primitives::Address::ZERO,
                    message: format!(
                        "failed to derive valid seed signer after {SEED_SIGNER_MAX_ATTEMPTS} attempts"
                    ),
                })
            }
            KeyStreamInner::Mnemonic { key, next_index, generated, .. } => {
                let index = *next_index;
                let signer = key
                    .child(index)
                    .map_err(|e| {
                        BaselineError::Config(format!(
                            "failed to derive mnemonic child {index}: {e}"
                        ))
                    })?
                    .signer();
                *next_index = next_index.checked_add(1).ok_or_else(|| {
                    BaselineError::Config(
                        "mnemonic index would overflow u32::MAX after derivation".into(),
                    )
                })?;
                *generated = generated.saturating_add(1);
                Ok(signer)
            }
        }
    }
}
