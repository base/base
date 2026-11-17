//! Field-by-field comparison of adapter responses (see "Comparison rules" in `PROTOCOL.md`).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::{CallFailure, HeaderResults, Request, Response};

/// Name used for the canonical chain in differences.
pub const CANONICAL: &str = "canonical";

/// One field on which the clients (or a client and the canonical chain) disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    /// Flattened field path, such as `header.stateRoot` or `post.0xab...storage.0x01`.
    pub field: String,
    /// Each client's value, `None` when the field is absent from its response.
    pub values: BTreeMap<String, Option<String>>,
}

/// Outcome of running one case on every adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "camelCase")]
pub enum Verdict {
    /// Every adapter answered and all answers match (and match the canonical header in
    /// validate mode).
    Agree,
    /// Every adapter answered but some fields differ.
    Diverge {
        /// The differing fields.
        differences: Vec<Difference>,
    },
    /// At least one adapter failed to produce a usable answer.
    AdapterFailure {
        /// Client name to failure description.
        failures: BTreeMap<String, String>,
    },
}

impl Verdict {
    /// Compares the responses of every adapter for one request.
    pub fn of(request: &Request, results: &[(String, Result<Response, CallFailure>)]) -> Self {
        let mut failures = BTreeMap::new();
        let mut flats = Vec::new();
        for (name, result) in results {
            match result {
                Err(failure) => {
                    failures.insert(name.clone(), failure.to_string());
                }
                Ok(response) if !response.ok => {
                    failures.insert(
                        name.clone(),
                        format!("ok=false: {}", response.error.as_deref().unwrap_or("")),
                    );
                }
                Ok(response) => flats.push((name.clone(), flatten(response))),
            }
        }
        if !failures.is_empty() {
            return Self::AdapterFailure { failures };
        }
        if let Some(canonical) = request.header.results.as_ref() {
            flats.push((CANONICAL.to_owned(), flatten_canonical(canonical)));
        }
        let differences = diff(&flats);
        if differences.is_empty() { Self::Agree } else { Self::Diverge { differences } }
    }

    /// One-line description of a verdict for logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Agree => "agree".to_owned(),
            Self::Diverge { differences } => {
                let fields: Vec<&str> =
                    differences.iter().take(6).map(|d| d.field.as_str()).collect();
                format!("DIVERGE on {} fields: {}", differences.len(), fields.join(", "))
            }
            Self::AdapterFailure { failures } => {
                let parts: Vec<String> = failures
                    .iter()
                    .map(|(name, why)| {
                        format!("{name}: {}", why.chars().take(200).collect::<String>())
                    })
                    .collect();
                format!("ADAPTER FAILURE {}", parts.join("; "))
            }
        }
    }
}

fn diff(flats: &[(String, BTreeMap<String, String>)]) -> Vec<Difference> {
    let fields: std::collections::BTreeSet<&str> =
        flats.iter().flat_map(|(_, flat)| flat.keys().map(String::as_str)).collect();
    let mut differences = Vec::new();
    for field in &fields {
        // The canonical chain only constrains the fields it has.
        let canonical_has =
            flats.iter().any(|(name, flat)| name == CANONICAL && flat.contains_key(*field));
        let participants: Vec<&(String, BTreeMap<String, String>)> =
            flats.iter().filter(|(name, _)| name != CANONICAL || canonical_has).collect();
        let first = participants[0].1.get(*field);
        if participants.iter().all(|(_, flat)| flat.get(*field) == first) {
            continue;
        }
        differences.push(Difference {
            field: (*field).to_owned(),
            values: participants
                .iter()
                .map(|(name, flat)| (name.clone(), flat.get(*field).cloned()))
                .collect(),
        });
    }
    differences
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Flattens the compared parts of a response into `field -> value`.
fn flatten(response: &Response) -> BTreeMap<String, String> {
    let mut flat = BTreeMap::new();
    flat.insert("valid".to_owned(), response.valid.to_string());
    let rejected: Vec<String> = response.rejected.iter().map(|r| r.index.to_string()).collect();
    flat.insert("rejected".to_owned(), format!("[{}]", rejected.join(",")));
    if let Some(header) = &response.header {
        flatten_header(header, &mut flat);
    }
    if let Some(base_fee) = response.computed_base_fee_per_gas {
        flat.insert("computedBaseFeePerGas".to_owned(), base_fee.to_string());
    }
    flat.insert("receipts.len".to_owned(), response.receipts.len().to_string());
    for (i, receipt) in response.receipts.iter().enumerate() {
        let Value::Object(fields) = serde_json::to_value(receipt).expect("receipts serialize")
        else {
            unreachable!("receipts serialize to objects")
        };
        for (key, value) in fields {
            flat.insert(format!("receipts[{i}].{key}"), text(&value));
        }
    }
    for (address, account) in &response.post {
        let prefix = format!("post.{address:#x}");
        match account {
            None => {
                flat.insert(prefix, "deleted".to_owned());
            }
            Some(account) => {
                flat.insert(format!("{prefix}.nonce"), account.nonce.to_string());
                flat.insert(format!("{prefix}.balance"), account.balance.to_string());
                flat.insert(format!("{prefix}.codeHash"), format!("{:#x}", account.code_hash));
                for (slot, value) in &account.storage {
                    flat.insert(format!("{prefix}.storage.{slot:#x}"), format!("{value:#x}"));
                }
            }
        }
    }
    flat
}

fn flatten_header(header: &HeaderResults, flat: &mut BTreeMap<String, String>) {
    let Value::Object(fields) = serde_json::to_value(header).expect("headers serialize") else {
        unreachable!("headers serialize to objects")
    };
    for (key, value) in fields {
        flat.insert(format!("header.{key}"), text(&value));
    }
}

fn flatten_canonical(header: &HeaderResults) -> BTreeMap<String, String> {
    let mut flat = BTreeMap::new();
    flat.insert("valid".to_owned(), true.to_string());
    flatten_header(header, &mut flat);
    flat
}
