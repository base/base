//! Canonical non-broadcastable envelope ownership and conservative L1 fee evidence.

use std::collections::HashSet;

use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{B256, Bytes, Signature, U256, b256, keccak256};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use base_common_flz::flz_compress_len;

const DUMMY_R: B256 = b256!("5fdab2bc3e0846351de15a51b4f354bf4a4ce227302de002ac790bacef8ba802");
const DUMMY_S: B256 = b256!("adccfdc48b0427d6d60ddfacca470a52f6924a603539118d356c152d1f0b5986");
const DUMMY_Y_PARITY: bool = true;
/// Fixed structurally invalid high-s signature shared by T4b and phase-b encoding.
pub(crate) const fn dummy_signature() -> Signature {
    Signature::new(U256::from_be_bytes(DUMMY_R.0), U256::from_be_bytes(DUMMY_S.0), DUMMY_Y_PARITY)
}

/// Maximum encoded candidate envelope accepted by the bounded T4b shape authority.
pub const MAX_CANONICAL_ENVELOPE_LEN: usize = 4_096;

/// Owns canonical bytes until one consuming upstream L1-fee calculation.
#[derive(Debug)]
pub struct CanonicalEnvelopeOwner {
    encoded: Bytes,
}

/// Raw-free measurements for one canonical envelope fee calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalEnvelopeFeeEvidence {
    encoded_length: usize,
    zero_bytes: usize,
    non_zero_bytes: usize,
    fast_lz_size: usize,
    digest: B256,
    fee: U256,
}

impl CanonicalEnvelopeFeeEvidence {
    /// Returns the EIP-2718 encoded length.
    pub const fn encoded_length(&self) -> usize {
        self.encoded_length
    }
    /// Returns the zero-byte count.
    pub const fn zero_bytes(&self) -> usize {
        self.zero_bytes
    }
    /// Returns the non-zero-byte count.
    pub const fn non_zero_bytes(&self) -> usize {
        self.non_zero_bytes
    }
    /// Returns the upstream FastLZ encoded size.
    pub const fn fast_lz_size(&self) -> usize {
        self.fast_lz_size
    }
    /// Returns the envelope digest.
    pub const fn digest(&self) -> B256 {
        self.digest
    }
    /// Returns the upstream L1 fee.
    pub const fn fee(&self) -> U256 {
        self.fee
    }
}

/// Raw-free dual-envelope proof and the conservative fee selected from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalL1EnvelopeEvidence {
    dummy: CanonicalEnvelopeFeeEvidence,
    surrogate: CanonicalEnvelopeFeeEvidence,
    fee: U256,
}

impl CanonicalL1EnvelopeEvidence {
    /// Returns the fixed invalid-signature diagnostic tuple.
    pub const fn dummy(&self) -> CanonicalEnvelopeFeeEvidence {
        self.dummy
    }
    /// Returns the deterministic unique-u24 upper-bound tuple.
    pub const fn surrogate(&self) -> CanonicalEnvelopeFeeEvidence {
        self.surrogate
    }
    /// Returns the larger of the two upstream L1 fees.
    pub const fn fee(&self) -> U256 {
        self.fee
    }
    /// Returns the tuple whose fee defines the conservative selected authority.
    pub fn selected(&self) -> CanonicalEnvelopeFeeEvidence {
        if self.dummy.fee >= self.surrogate.fee { self.dummy } else { self.surrogate }
    }
}

/// Fail-closed canonical envelope construction or proof failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalEnvelopeError {
    /// The encoded shape exceeds the reviewed bounded domain.
    EnvelopeTooLong,
    /// The deterministic surrogate repeated a three-byte word.
    RepeatedU24,
    /// FastLZ did not encode the unique-u24 surrogate as one literal run.
    FastLzProofFailed,
    /// Upstream returned a zero fee for a positive-length envelope.
    ZeroL1Fee,
}

impl core::fmt::Display for CanonicalEnvelopeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "canonical envelope authority unavailable: {self:?}")
    }
}

impl core::error::Error for CanonicalEnvelopeError {}

impl CanonicalEnvelopeOwner {
    /// Consumes the only raw owner and calls the upstream L1 fee authority.
    pub fn calculate_l1_evidence(
        self,
        l1: &mut L1BlockInfo,
        spec: BaseSpecId,
    ) -> Result<CanonicalEnvelopeFeeEvidence, CanonicalEnvelopeError> {
        let encoded_length = self.encoded.len();
        let zero_bytes = self.encoded.iter().filter(|byte| **byte == 0).count();
        let non_zero_bytes = encoded_length - zero_bytes;
        let fast_lz_size = flz_compress_len(&self.encoded) as usize;
        let digest = keccak256(&self.encoded);
        let fee = l1.calculate_tx_l1_cost(&self.encoded, spec);
        if encoded_length != 0 && fee.is_zero() {
            return Err(CanonicalEnvelopeError::ZeroL1Fee);
        }
        Ok(CanonicalEnvelopeFeeEvidence {
            encoded_length,
            zero_bytes,
            non_zero_bytes,
            fast_lz_size,
            digest,
            fee,
        })
    }
}

/// Constructs the fixed dummy and deterministic proof envelopes without exposing bytes.
#[derive(Debug)]
pub struct CanonicalEnvelopeFactory;

impl CanonicalEnvelopeFactory {
    /// Calculates both upstream fees with independent cache clears and returns their maximum.
    pub fn calculate_l1_evidence(
        tx: &TxEip1559,
        l1: &mut L1BlockInfo,
        spec: BaseSpecId,
    ) -> Result<CanonicalL1EnvelopeEvidence, CanonicalEnvelopeError> {
        let dummy = Self::dummy(tx)?;
        let length = dummy.encoded.len();
        let surrogate = Self::unique_u24_surrogate(length)?;

        l1.clear_tx_l1_cost();
        let dummy = dummy.calculate_l1_evidence(l1, spec)?;
        l1.clear_tx_l1_cost();
        let surrogate = surrogate.calculate_l1_evidence(l1, spec)?;
        let fee = dummy.fee.max(surrogate.fee);
        Ok(CanonicalL1EnvelopeEvidence { dummy, surrogate, fee })
    }

    fn dummy(tx: &TxEip1559) -> Result<CanonicalEnvelopeOwner, CanonicalEnvelopeError> {
        let signature = dummy_signature();
        let encoded = Bytes::from(tx.clone().into_signed(signature).encoded_2718());
        if encoded.len() > MAX_CANONICAL_ENVELOPE_LEN {
            return Err(CanonicalEnvelopeError::EnvelopeTooLong);
        }
        Ok(CanonicalEnvelopeOwner { encoded })
    }

    fn unique_u24_surrogate(
        length: usize,
    ) -> Result<CanonicalEnvelopeOwner, CanonicalEnvelopeError> {
        if length > MAX_CANONICAL_ENVELOPE_LEN {
            return Err(CanonicalEnvelopeError::EnvelopeTooLong);
        }
        fn emit_de_bruijn(
            t: usize,
            period: usize,
            state: &mut [u16; 4],
            encoded: &mut Vec<u8>,
            target: usize,
            started: &mut bool,
        ) -> bool {
            if t > 3 {
                if 3 % period == 0 {
                    for index in 1..=period {
                        let byte = state[index] as u8;
                        if *started || byte == 0x02 {
                            *started = true;
                            encoded.push(byte);
                            if encoded.len() == target {
                                return true;
                            }
                        }
                    }
                }
                return false;
            }
            state[t] = state[t - period];
            if emit_de_bruijn(t + 1, period, state, encoded, target, started) {
                return true;
            }
            for value in state[t - period] + 1..256 {
                state[t] = value;
                if emit_de_bruijn(t + 1, t, state, encoded, target, started) {
                    return true;
                }
            }
            false
        }

        let mut encoded = Vec::with_capacity(length);
        if length != 0 {
            let mut state = [0u16; 4];
            let mut started = false;
            if !emit_de_bruijn(1, 1, &mut state, &mut encoded, length, &mut started) {
                return Err(CanonicalEnvelopeError::RepeatedU24);
            }
        }
        if encoded
            .windows(3)
            .map(|word| (u32::from(word[0]) << 16) | (u32::from(word[1]) << 8) | u32::from(word[2]))
            .collect::<HashSet<_>>()
            .len()
            != length.saturating_sub(2)
        {
            return Err(CanonicalEnvelopeError::RepeatedU24);
        }
        if flz_compress_len(&encoded) as usize != Self::literal_encoded_len(length) {
            return Err(CanonicalEnvelopeError::FastLzProofFailed);
        }
        Ok(CanonicalEnvelopeOwner { encoded: Bytes::from(encoded) })
    }

    /// Returns the exact FastLZ cost of a total match length `m >= 3`.
    pub const fn exact_match_encoded_len(match_length: usize) -> usize {
        let adjusted = match_length - 3;
        3 * (adjusted / 262) + if adjusted % 262 >= 6 { 3 } else { 2 }
    }

    /// Returns the exact cost of one literal run.
    pub const fn literal_encoded_len(length: usize) -> usize {
        length + length.div_ceil(32)
    }

    /// Maximizes exact encoded cost over relaxed legal literal/match partitions.
    pub fn relaxed_legal_partition_upper_bound(length: usize) -> usize {
        if length == 0 {
            return 0;
        }
        let mut after_match = vec![0usize; length + 1];
        let mut after_literal = vec![0usize; length + 1];
        for consumed in 1..=length {
            after_literal[consumed] = Self::literal_encoded_len(consumed);
            for run in 1..=consumed {
                after_literal[consumed] = after_literal[consumed]
                    .max(after_match[consumed - run] + Self::literal_encoded_len(run));
            }
            if consumed >= 3 {
                for matched in 3..=consumed {
                    let prior = consumed - matched;
                    let best_prior = after_match[prior].max(after_literal[prior]);
                    after_match[consumed] = after_match[consumed]
                        .max(best_prior + Self::exact_match_encoded_len(matched));
                }
            }
        }
        after_match[length].max(after_literal[length])
    }
}
