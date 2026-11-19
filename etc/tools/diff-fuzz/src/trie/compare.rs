//! Comparison of every implementation's per-block trie results, field by field, against the
//! majority.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Bytes, hex, keccak256};
use alloy_trie::{KECCAK_EMPTY, Nibbles, TrieAccount, proof::verify_proof};
use serde::Serialize;

use crate::{ProofResult, TrieBlockResult};

/// One field on which implementations disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrieDifference {
    /// Field path, such as `b2.proof0.slot1.proof`.
    pub field: String,
    /// Field kind, such as `stateRoot` or `storageProof`.
    pub kind: String,
    /// Block index.
    pub block: usize,
    /// The majority value.
    pub majority: String,
    /// Implementations with another value, and their values.
    pub odd: BTreeMap<String, String>,
}

/// The outcome of comparing one case.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrieVerdict {
    /// Every disagreement, in block and field order.
    pub differences: Vec<TrieDifference>,
}

impl TrieVerdict {
    /// Kinds from the most to the least telling symptom.
    pub const PRIORITY: [&str; 10] = [
        "failure",
        "error",
        "stateRoot",
        "storageRoots.keys",
        "storageRoot",
        "account",
        "storageValue",
        "proofInvalid",
        "accountProof",
        "storageProof",
    ];

    /// Whether everything agreed.
    pub const fn agrees(&self) -> bool {
        self.differences.is_empty()
    }

    /// The finding signature: the most telling kind in the first diverging block and the
    /// implementations that differ in it, such as `stateRoot:nethermind/flat`.
    pub fn signature(&self) -> Option<String> {
        let first = self.differences.first()?.block;
        let in_block: Vec<&TrieDifference> =
            self.differences.iter().filter(|d| d.block == first).collect();
        let kind = Self::PRIORITY
            .iter()
            .find(|kind| in_block.iter().any(|d| d.kind == **kind))
            .copied()
            .unwrap_or("other");
        let of_kind: Vec<&&TrieDifference> = in_block.iter().filter(|d| d.kind == kind).collect();
        let odd: BTreeSet<&String> = of_kind.iter().flat_map(|d| d.odd.keys()).collect();
        let odd: Vec<&str> = odd.into_iter().map(String::as_str).collect();
        // Errors and failures also differ by what went wrong.
        let what = match kind {
            "error" | "failure" => of_kind
                .first()
                .and_then(|d| d.odd.values().next())
                .map(|message| format!(":{}", Self::shape(message)))
                .unwrap_or_default(),
            _ => String::new(),
        };
        Some(format!("{kind}:{}{what}", odd.join(",")))
    }

    /// An error message with numbers and hex elided, shortened, for grouping.
    pub fn shape(message: &str) -> String {
        let mut out = String::new();
        let mut in_number = false;
        for c in message.chars() {
            if c.is_ascii_hexdigit() && (in_number || c.is_ascii_digit()) || (in_number && c == 'x')
            {
                if !in_number {
                    out.push('#');
                    in_number = true;
                }
            } else {
                in_number = false;
                out.push(c);
            }
            if out.len() >= 60 {
                break;
            }
        }
        out
    }
}

/// Compares implementations' results.
#[derive(Debug, Clone, Copy)]
pub struct TrieCompare;

impl TrieCompare {
    /// Proof nodes as compared: embedded nodes (shorter than 32 bytes, other than the first)
    /// dropped, and the empty-trie node (`0x80`), which clients list or omit for an empty trie.
    pub fn normalize(proof: &[Bytes]) -> Vec<Bytes> {
        proof
            .iter()
            .enumerate()
            .filter(|(i, n)| {
                (*i == 0 || n.len() >= 32) && n.as_ref() != [alloy_rlp::EMPTY_STRING_CODE]
            })
            .map(|(_, n)| n.clone())
            .collect()
    }

    fn nodes(proof: &[Bytes]) -> String {
        Self::normalize(proof).iter().map(hex::encode).collect::<Vec<_>>().join(",")
    }

    fn flatten(blocks: &[TrieBlockResult]) -> BTreeMap<(usize, String), (String, String)> {
        let mut out = BTreeMap::new();
        for (b, result) in blocks.iter().enumerate() {
            let mut put = |field: String, kind: &str, value: String| {
                out.insert((b, field), (kind.to_owned(), value));
            };
            if let Some(error) = &result.error {
                put(format!("b{b}.error"), "error", error.clone());
            }
            if let Some(root) = result.state_root {
                put(format!("b{b}.stateRoot"), "stateRoot", root.to_string());
            }
            if let Some(roots) = &result.storage_roots {
                let keys: Vec<String> = roots.keys().map(ToString::to_string).collect();
                put(format!("b{b}.storageRoots.keys"), "storageRoots.keys", keys.join(","));
                for (key, root) in roots {
                    put(format!("b{b}.storageRoot.{key}"), "storageRoot", root.to_string());
                }
            }
            for (p, proof) in result.proofs.iter().flatten().enumerate() {
                Self::flatten_proof(b, p, proof, &mut put);
            }
        }
        out
    }

    fn flatten_proof(
        b: usize,
        p: usize,
        proof: &ProofResult,
        put: &mut impl FnMut(String, &str, String),
    ) {
        let account = proof.account.as_ref().map_or_else(
            || "none".to_owned(),
            |a| format!("{}/{}/{}/{}", a.nonce, a.balance, a.code_hash, a.storage_root),
        );
        put(format!("b{b}.proof{p}.account"), "account", account);
        put(
            format!("b{b}.proof{p}.accountProof"),
            "accountProof",
            Self::nodes(&proof.account_proof),
        );
        for (s, slot) in proof.storage.iter().enumerate() {
            put(format!("b{b}.proof{p}.slot{s}.value"), "storageValue", slot.value.to_string());
            put(format!("b{b}.proof{p}.slot{s}.proof"), "storageProof", Self::nodes(&slot.proof));
        }
    }

    /// Compares `results` (implementation name and its blocks, or an adapter failure) over a
    /// case of `blocks` blocks. A field is compared among the implementations that report it;
    /// the most common value is the majority, ties going to the earliest implementation.
    pub fn compare(
        blocks: usize,
        results: &[(String, Result<Vec<TrieBlockResult>, String>)],
    ) -> TrieVerdict {
        let mut differences = Vec::new();
        let mut fields: BTreeMap<(usize, String), Vec<(&str, String)>> = BTreeMap::new();
        let mut kinds: BTreeMap<(usize, String), String> = BTreeMap::new();
        for (name, result) in results {
            match result {
                Err(error) => differences.push(TrieDifference {
                    field: "failure".to_owned(),
                    kind: "failure".to_owned(),
                    block: 0,
                    majority: String::new(),
                    odd: BTreeMap::from([(name.clone(), error.clone())]),
                }),
                Ok(list) => {
                    for ((block, field), (kind, value)) in Self::flatten(list) {
                        kinds.insert((block, field.clone()), kind);
                        fields.entry((block, field)).or_default().push((name, value));
                    }
                }
            }
        }
        for ((block, field), values) in fields {
            let kind = kinds.remove(&(block, field.clone())).unwrap_or_default();
            if kind == "error" {
                let odd = values.iter().map(|(n, v)| ((*n).to_owned(), v.clone())).collect();
                differences.push(TrieDifference {
                    field,
                    kind,
                    block,
                    majority: String::new(),
                    odd,
                });
                continue;
            }
            let mut counts: Vec<(&String, usize)> = Vec::new();
            for (_, value) in &values {
                match counts.iter_mut().find(|(v, _)| *v == value) {
                    Some(entry) => entry.1 += 1,
                    None => counts.push((value, 1)),
                }
            }
            if counts.len() == 1 {
                continue;
            }
            let best = counts.iter().map(|(_, c)| *c).max().unwrap_or(0);
            let majority = counts.iter().find(|(_, c)| *c == best).expect("nonempty").0.clone();
            let odd = values
                .iter()
                .filter(|(_, v)| *v != majority)
                .map(|(n, v)| ((*n).to_owned(), v.clone()))
                .collect();
            differences.push(TrieDifference { field, kind, block, majority, odd });
        }
        for (name, result) in results {
            let Ok(list) = result else { continue };
            for (block, result) in list.iter().enumerate().take(blocks) {
                if let Some(field) = Self::wrong_code(name, result) {
                    differences.push(TrieDifference {
                        field: format!("b{block}.{field}"),
                        kind: "code".to_owned(),
                        block,
                        majority: String::new(),
                        odd: BTreeMap::from([(
                            name.clone(),
                            "code does not match codeHash".to_owned(),
                        )]),
                    });
                }
                if let Some(field) = Self::invalid_proof(result) {
                    differences.push(TrieDifference {
                        field: format!("b{block}.{field}"),
                        kind: "proofInvalid".to_owned(),
                        block,
                        majority: String::new(),
                        odd: BTreeMap::from([(name.clone(), "does not verify".to_owned())]),
                    });
                }
            }
        }
        differences.sort_by(|a, b| a.block.cmp(&b.block).then_with(|| a.field.cmp(&b.field)));
        TrieVerdict { differences }
    }

    /// The first proof of `result` whose reported code does not hash to its account's code
    /// hash. needlefish's `disk-deep` stores no codes and reports every code as empty.
    fn wrong_code(name: &str, result: &TrieBlockResult) -> Option<String> {
        result.proofs.iter().flatten().enumerate().find_map(|(p, proof)| {
            let code = proof.code.as_ref()?;
            let hash = proof.account.as_ref().map_or(KECCAK_EMPTY, |a| a.code_hash);
            let ok = keccak256(code) == hash || (code.is_empty() && name.ends_with("/disk-deep"));
            (!ok).then(|| format!("proof{p}.code"))
        })
    }

    /// The first account or storage proof of `result` that does not verify against its own
    /// state root and storage root.
    fn invalid_proof(result: &TrieBlockResult) -> Option<String> {
        let root = result.state_root?;
        for (p, proof) in result.proofs.iter().flatten().enumerate() {
            let leaf = proof.account.as_ref().map(|a| {
                alloy_rlp::encode(TrieAccount {
                    nonce: a.nonce,
                    balance: a.balance,
                    storage_root: a.storage_root,
                    code_hash: a.code_hash,
                })
            });
            let nodes = Self::normalize(&proof.account_proof);
            if verify_proof(root, Nibbles::unpack(proof.key), leaf, &nodes).is_err() {
                return Some(format!("proof{p}.accountProof"));
            }
            let Some(storage_root) = proof.account.as_ref().map(|a| a.storage_root) else {
                continue;
            };
            for (s, slot) in proof.storage.iter().enumerate() {
                let value = alloy_primitives::U256::from_be_bytes(slot.value.0);
                let expected = (!value.is_zero()).then(|| alloy_rlp::encode(value));
                let nodes = Self::normalize(&slot.proof);
                if verify_proof(storage_root, Nibbles::unpack(slot.slot), expected, &nodes).is_err()
                {
                    return Some(format!("proof{p}.slot{s}.proof"));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    fn root(byte: u8) -> TrieBlockResult {
        TrieBlockResult { state_root: Some(B256::repeat_byte(byte)), ..Default::default() }
    }

    #[test]
    fn the_minority_is_odd_and_signatures_name_the_first_block() {
        let results = vec![
            ("model".to_owned(), Ok(vec![root(1), root(2)])),
            ("base".to_owned(), Ok(vec![root(1), root(2)])),
            ("nf".to_owned(), Ok(vec![root(1), root(3)])),
            ("nm/flat".to_owned(), Ok(vec![root(1)])),
        ];
        let verdict = TrieCompare::compare(2, &results);
        assert_eq!(verdict.differences.len(), 1);
        assert_eq!(verdict.differences[0].block, 1);
        assert_eq!(verdict.signature().as_deref(), Some("stateRoot:nf"));
    }

    #[test]
    fn reported_code_must_hash_to_the_code_hash() {
        let code = Bytes::from_static(&[0x60, 0x00]);
        let with_code = |code: Bytes| TrieBlockResult {
            proofs: Some(vec![ProofResult {
                key: B256::ZERO,
                account: Some(crate::ProofAccount {
                    nonce: 1,
                    balance: Default::default(),
                    code_hash: keccak256([0x60, 0x00]),
                    storage_root: alloy_trie::EMPTY_ROOT_HASH,
                }),
                account_proof: Vec::new(),
                storage: Vec::new(),
                code: Some(code),
            }]),
            ..Default::default()
        };
        let results = vec![
            ("nf".to_owned(), Ok(vec![with_code(code)])),
            ("nf/disk-deep".to_owned(), Ok(vec![with_code(Bytes::new())])),
            ("nf/disk-small".to_owned(), Ok(vec![with_code(Bytes::new())])),
            ("nf/memory-history".to_owned(), Ok(vec![with_code(Bytes::from_static(&[0x60]))])),
        ];
        let odd: Vec<_> = TrieCompare::compare(1, &results)
            .differences
            .iter()
            .filter(|d| d.kind == "code")
            .flat_map(|d| d.odd.keys().cloned())
            .collect();
        assert_eq!(odd, ["nf/disk-small", "nf/memory-history"]);
    }
}
