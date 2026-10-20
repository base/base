//! Signed candidate envelope: its EIP-712 encoding and its JSON-RPC form.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolStruct, eip712_domain, sol};
use serde::{Deserialize, Deserializer, Serialize, de};

sol! {
    /// EIP-712 selection rule. Field order and names are part of the signed type string.
    #[derive(Debug, PartialEq, Eq)]
    struct SelectionRule {
        /// Rule kind; `1` is `MAX_RECIPIENT_BALANCE_DELTA`, the only kind in v0.
        uint8 kind;
        /// Score-read context version; `1` is the only version in v0.
        uint8 scoreVersion;
        /// ERC-20 token whose balance is measured, or the zero address for the native asset.
        address token;
        /// Account whose balance change is measured.
        address recipient;
        /// A trial is eligible only if its delta is at least this value.
        uint256 minDelta;
    }

    /// EIP-712 candidate envelope. Its signing hash is the group's identity (`groupHash`).
    ///
    /// It carries no expiry: the node bounds each admitted group with its configured lifetime.
    #[derive(Debug, PartialEq, Eq)]
    struct CandidateSet {
        /// Sender of every sibling and signer of the envelope.
        address sender;
        /// Nonce shared by every sibling.
        uint64 nonce;
        /// Ordered sibling hashes; the index is the candidate id.
        bytes32[] siblingHashes;
        /// Rule that chooses the winner.
        SelectionRule rule;
        /// `0` = `DROP`, `1` = `DEFAULT_SIBLING`.
        uint8 fallbackMode;
        /// Sibling used for `DEFAULT_SIBLING` fallback.
        uint8 defaultIndex;
        /// Lifetime cap on trial gas plus scoring-probe gas.
        uint64 maxWorkGas;
        /// Lifetime cap on evaluation rounds.
        uint16 maxEvaluations;
        /// Upper bound, in wei, on any access charge.
        uint256 maxAccessCharge;
        /// `0` = attestation only, `1` = delayed full replay allowed.
        uint8 disclosure;
    }
}

/// Wire-format ceiling on the number of siblings or sibling hashes a request may carry.
///
/// Deserialization stops before allocating past this bound; the configured
/// `max_siblings` limit is enforced afterwards and must not exceed it.
pub const MAX_SIBLINGS_CEILING: usize = 16;

/// The only selection rule kind in v0: maximize the recipient's balance change.
pub const RULE_MAX_RECIPIENT_BALANCE_DELTA: u8 = 1;

/// The only score-read context version in v0.
pub const SCORE_VERSION_V1: u8 = 1;

/// What happens when selection cannot produce a winner (design §5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallbackMode {
    /// Never execute a sibling the rule did not select.
    Drop,
    /// Execute `defaultIndex` through the ordinary transaction path.
    DefaultSibling,
}

impl FallbackMode {
    /// Decodes the signed `fallbackMode` byte.
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Drop),
            1 => Some(Self::DefaultSibling),
            _ => None,
        }
    }

    /// Returns a stable metric and log label.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Drop => "drop",
            Self::DefaultSibling => "default_sibling",
        }
    }
}

/// JSON form of [`SelectionRule`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateSelectionRule {
    /// See [`SelectionRule::kind`].
    #[serde(with = "alloy_serde::quantity")]
    pub kind: u8,
    /// See [`SelectionRule::scoreVersion`].
    #[serde(with = "alloy_serde::quantity")]
    pub score_version: u8,
    /// See [`SelectionRule::token`].
    pub token: Address,
    /// See [`SelectionRule::recipient`].
    pub recipient: Address,
    /// See [`SelectionRule::minDelta`].
    pub min_delta: U256,
}

impl CandidateSelectionRule {
    /// Returns `true` when the rule measures the native asset rather than an ERC-20 token.
    pub fn is_native(&self) -> bool {
        self.token.is_zero()
    }
}

/// JSON form of [`CandidateSet`], as carried in `base_sendSwapTransaction`'s `selection`.
///
/// Integer fields keep their raw signed values so the signing hash is computed over exactly
/// what the sender signed; [`crate::CandidateValidator`] checks their ranges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CandidateEnvelope {
    /// See [`CandidateSet::sender`].
    pub sender: Address,
    /// See [`CandidateSet::nonce`].
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// See [`CandidateSet::siblingHashes`].
    #[serde(deserialize_with = "CandidateEnvelope::deserialize_bounded")]
    pub sibling_hashes: Vec<B256>,
    /// See [`CandidateSet::rule`].
    pub rule: CandidateSelectionRule,
    /// See [`CandidateSet::fallbackMode`].
    #[serde(with = "alloy_serde::quantity")]
    pub fallback_mode: u8,
    /// See [`CandidateSet::defaultIndex`].
    #[serde(with = "alloy_serde::quantity")]
    pub default_index: u8,
    /// See [`CandidateSet::maxWorkGas`].
    #[serde(with = "alloy_serde::quantity")]
    pub max_work_gas: u64,
    /// See [`CandidateSet::maxEvaluations`].
    #[serde(with = "alloy_serde::quantity")]
    pub max_evaluations: u16,
    /// See [`CandidateSet::maxAccessCharge`].
    pub max_access_charge: U256,
    /// See [`CandidateSet::disclosure`].
    #[serde(with = "alloy_serde::quantity")]
    pub disclosure: u8,
}

impl CandidateEnvelope {
    /// EIP-712 domain name.
    pub const DOMAIN_NAME: &'static str = "Base Candidate Selection";

    /// EIP-712 domain version.
    pub const DOMAIN_VERSION: &'static str = "1";

    /// Returns the EIP-712 message for this envelope.
    pub fn to_eip712(&self) -> CandidateSet {
        CandidateSet {
            sender: self.sender,
            nonce: self.nonce,
            siblingHashes: self.sibling_hashes.clone(),
            rule: SelectionRule {
                kind: self.rule.kind,
                scoreVersion: self.rule.score_version,
                token: self.rule.token,
                recipient: self.rule.recipient,
                minDelta: self.rule.min_delta,
            },
            fallbackMode: self.fallback_mode,
            defaultIndex: self.default_index,
            maxWorkGas: self.max_work_gas,
            maxEvaluations: self.max_evaluations,
            maxAccessCharge: self.max_access_charge,
            disclosure: self.disclosure,
        }
    }

    /// Returns the EIP-712 signing hash, which is the group's identity (`groupHash`).
    ///
    /// The domain is `Base Candidate Selection`, version `1`, with the siblings' chain ID.
    pub fn signing_hash(&self, chain_id: u64) -> B256 {
        self.to_eip712().eip712_signing_hash(&eip712_domain! {
            name: Self::DOMAIN_NAME,
            version: Self::DOMAIN_VERSION,
            chain_id: chain_id,
        })
    }

    /// Returns the decoded fallback mode, or `None` for an unknown signed value.
    pub const fn fallback(&self) -> Option<FallbackMode> {
        FallbackMode::from_wire(self.fallback_mode)
    }

    /// Returns the index of the pool and iterator lifecycle identity.
    ///
    /// `defaultIndex` for `DEFAULT_SIBLING`, else `0`. The representative is never the winner's
    /// identity; resource checks and accounting always use the winning sibling.
    pub fn representative_index(&self) -> usize {
        match self.fallback() {
            Some(FallbackMode::DefaultSibling) => usize::from(self.default_index),
            _ => 0,
        }
    }

    /// Deserializes a sequence, stopping before it grows past [`MAX_SIBLINGS_CEILING`].
    pub fn deserialize_bounded<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        struct BoundedVisitor<T>(core::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for BoundedVisitor<T> {
            type Value = Vec<T>;

            fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(formatter, "a sequence of at most {MAX_SIBLINGS_CEILING} elements")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut values =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_SIBLINGS_CEILING));
                while let Some(value) = seq.next_element::<T>()? {
                    if values.len() == MAX_SIBLINGS_CEILING {
                        return Err(de::Error::custom(format!(
                            "too many siblings: maximum {MAX_SIBLINGS_CEILING}"
                        )));
                    }
                    values.push(value);
                }
                Ok(values)
            }
        }

        deserializer.deserialize_seq(BoundedVisitor(core::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `envelope()`'s signing hash on chain 8453 for the `CandidateSet` type without `expiryBlock`.
    const VECTOR_HASH: B256 = alloy_primitives::b256!(
        "0x9e23362520ef2253cd5a9050ca90436e4c4c7c18f2c5f411d94fa69c41517773"
    );

    fn envelope() -> CandidateEnvelope {
        CandidateEnvelope {
            sender: Address::repeat_byte(0x11),
            nonce: 7,
            sibling_hashes: vec![B256::repeat_byte(1), B256::repeat_byte(2)],
            rule: CandidateSelectionRule {
                kind: RULE_MAX_RECIPIENT_BALANCE_DELTA,
                score_version: SCORE_VERSION_V1,
                token: Address::ZERO,
                recipient: Address::repeat_byte(0x11),
                min_delta: U256::from(5),
            },
            fallback_mode: 1,
            default_index: 1,
            max_work_gas: 1_000_000,
            max_evaluations: 3,
            max_access_charge: U256::ZERO,
            disclosure: 0,
        }
    }

    #[test]
    fn json_round_trip_uses_camel_case_quantities() {
        let json = serde_json::to_value(envelope()).unwrap();
        assert_eq!(json["nonce"], "0x7");
        assert_eq!(json["rule"]["scoreVersion"], "0x1");
        assert_eq!(json["maxWorkGas"], "0xf4240");
        let decoded: CandidateEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, envelope());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut json = serde_json::to_value(envelope()).unwrap();
        json["extra"] = serde_json::json!(1);
        assert!(serde_json::from_value::<CandidateEnvelope>(json).is_err());
    }

    #[test]
    fn sibling_hashes_are_bounded_during_deserialization() {
        let mut json = serde_json::to_value(envelope()).unwrap();
        json["siblingHashes"] =
            serde_json::to_value(vec![B256::ZERO; MAX_SIBLINGS_CEILING + 1]).unwrap();
        let error = serde_json::from_value::<CandidateEnvelope>(json).unwrap_err();
        assert!(error.to_string().contains("too many siblings"), "{error}");
    }

    #[test]
    fn signing_hash_binds_chain_and_every_field() {
        let base = envelope();
        let hash = base.signing_hash(8453);
        assert_ne!(hash, base.signing_hash(84532));

        let mut changed = base.clone();
        changed.rule.min_delta = U256::from(6);
        assert_ne!(hash, changed.signing_hash(8453));

        let mut reordered = base;
        reordered.sibling_hashes.reverse();
        assert_ne!(hash, reordered.signing_hash(8453));
    }

    #[test]
    fn representative_is_default_index_only_for_default_sibling() {
        let mut env = envelope();
        assert_eq!(env.representative_index(), 1);
        env.fallback_mode = 0;
        assert_eq!(env.representative_index(), 0);
    }

    #[test]
    fn signed_type_carries_no_expiry() {
        // Groups get a server-side lifetime; the sender signs no expiry block.
        let type_string = CandidateSet::eip712_encode_type();
        assert!(!type_string.contains("expiryBlock"), "{type_string}");
        let mut json = serde_json::to_value(envelope()).unwrap();
        json["expiryBlock"] = serde_json::json!("0x64");
        assert!(serde_json::from_value::<CandidateEnvelope>(json).is_err());
    }

    #[test]
    fn signed_type_and_hash_vector() {
        assert_eq!(
            CandidateSet::eip712_encode_type(),
            "CandidateSet(address sender,uint64 nonce,bytes32[] siblingHashes,SelectionRule rule,\
             uint8 fallbackMode,uint8 defaultIndex,uint64 maxWorkGas,uint16 maxEvaluations,\
             uint256 maxAccessCharge,uint8 disclosure)SelectionRule(uint8 kind,uint8 scoreVersion,\
             address token,address recipient,uint256 minDelta)"
        );
        assert_eq!(envelope().signing_hash(8453), VECTOR_HASH);
    }
}
