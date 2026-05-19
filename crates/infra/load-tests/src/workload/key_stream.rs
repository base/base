//! Lazy key stream for deterministic `secp256k1` signer derivation.

use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use rand::{Rng, SeedableRng, rngs::StdRng};

use crate::utils::{BaselineError, Result};

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
    /// `StdRng`-driven derivation: each `next_signer` consumes 32 bytes.
    /// Boxed because `StdRng` is ~256 bytes and dwarfs the other variant.
    Seed { rng: Box<StdRng>, seed: u64, offset: usize, generated: u64 },
    /// BIP39 derivation: each `next_signer` advances `next_index`.
    Mnemonic {
        /// BIP39 phrase used to derive each key.
        phrase: String,
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
    /// `AccountPool::with_offset`.
    pub fn from_seed(seed: u64, offset: usize) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        for _ in 0..offset {
            let mut skip = [0u8; 32];
            rng.fill(&mut skip);
        }
        Self(KeyStreamInner::Seed { rng: Box::new(rng), seed, offset, generated: 0 })
    }

    /// Builds a mnemonic-driven stream positioned at BIP39 index `offset`.
    pub fn from_mnemonic(phrase: impl Into<String>, offset: usize) -> Result<Self> {
        let next_index = u32::try_from(offset).map_err(|_| {
            BaselineError::Config(format!("mnemonic index {offset} exceeds u32::MAX"))
        })?;
        Ok(Self(KeyStreamInner::Mnemonic {
            phrase: phrase.into(),
            offset,
            next_index,
            generated: 0,
        }))
    }

    /// Returns instructions for recovering keys produced by this stream.
    pub fn recovery_message(&self) -> String {
        match &self.0 {
            KeyStreamInner::Seed { seed, offset, .. } => {
                format!(
                    "Fresh-recipient mode: seed={seed} recipient_offset={offset} \
                     (recover with AccountPool::with_offset(seed, n, recipient_offset))"
                )
            }
            KeyStreamInner::Mnemonic { offset, .. } => {
                format!(
                    "Fresh-recipient mode: recipient_offset={offset} \
                     (recover with AccountPool::from_mnemonic(mnemonic, n, recipient_offset))"
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
    /// is handled by drawing again. For `Mnemonic`, returns an error if the
    /// next index would overflow `u32::MAX` or if BIP39 derivation fails.
    pub fn next_signer(&mut self) -> Result<PrivateKeySigner> {
        match &mut self.0 {
            KeyStreamInner::Seed { rng, generated, .. } => loop {
                let mut bytes = [0u8; 32];
                rng.fill(&mut bytes);
                if let Ok(signer) = PrivateKeySigner::from_bytes(&bytes.into()) {
                    *generated = generated.saturating_add(1);
                    return Ok(signer);
                }
            },
            KeyStreamInner::Mnemonic { phrase, next_index, generated, .. } => {
                let index = *next_index;
                let signer = MnemonicBuilder::<English>::default()
                    .phrase(phrase.as_str())
                    .index(index)
                    .map_err(|e| {
                        BaselineError::Config(format!("invalid mnemonic index {index}: {e}"))
                    })?
                    .build()
                    .map_err(|e| BaselineError::Config(format!("failed to derive key: {e}")))?;
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
