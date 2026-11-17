//! Finding signatures and reduction of diverging cases to small reproducers.

use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
};

use serde::Serialize;

use crate::{
    Adapter, CANONICAL, CallFailure, FuzzCase, GeneratorOptions, Request, Response, SlotKind,
    Verdict,
};

/// Every adapter's response (or failure) to one request, in adapter order.
pub type AdapterResults = Vec<(String, Result<Response, CallFailure>)>;

/// What makes two findings the same bug for deduplication and minimization: the clients that
/// disagree with the reference and the most specific kind of field that differs (one root cause
/// usually changes many fields, and the earliest effect in [`Self::PRIORITY`] names it best).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct FindingSignature {
    /// Whether the finding is an adapter failure rather than a divergence.
    pub failure: bool,
    /// Clients whose answer differs from the reference's (or that failed).
    pub odd: BTreeSet<String>,
    /// The most specific differing field kind, or the failure kind.
    pub primary: String,
    /// Every differing field kind, such as `receipts.status` or `post.balance`, or failure kinds.
    pub kinds: BTreeSet<String>,
}

impl FindingSignature {
    /// Field kinds from the most to the least specific symptom.
    pub const PRIORITY: [&str; 16] = [
        "valid",
        "rejected",
        "receipts.len",
        "receipts.status",
        "receipts.cumulativeGasUsed",
        "receipts.logs",
        "receipts.depositNonce",
        "receipts.depositReceiptVersion",
        "post.codeHash",
        "post.nonce",
        "post.deleted",
        "post.storage",
        "post.balance",
        "computedBaseFeePerGas",
        "header.blobGasUsed",
        "header.withdrawalsRoot",
    ];

    fn primary(kinds: &BTreeSet<String>) -> String {
        Self::PRIORITY
            .iter()
            .find(|kind| kinds.contains(**kind))
            .map(|kind| (*kind).to_owned())
            .or_else(|| kinds.iter().next().cloned())
            .unwrap_or_default()
    }

    /// The signature of a non-agreeing verdict, with `reference` as the client others are
    /// compared to. `None` for agreement.
    pub fn of(verdict: &Verdict, reference: &str) -> Option<Self> {
        match verdict {
            Verdict::Agree => None,
            Verdict::Diverge { differences } => {
                let mut odd = BTreeSet::new();
                let mut kinds = BTreeSet::new();
                for difference in differences {
                    let expected = difference.values.get(reference);
                    for (name, value) in &difference.values {
                        if name != CANONICAL && Some(value) != expected {
                            odd.insert(name.clone());
                        }
                    }
                    kinds.insert(Self::kind(&difference.field));
                }
                Some(Self { failure: false, odd, primary: Self::primary(&kinds), kinds })
            }
            Verdict::AdapterFailure { failures } => {
                let kinds: BTreeSet<String> = failures
                    .values()
                    .map(|why| {
                        let words: Vec<&str> = why
                            .split_whitespace()
                            .map(|w| if w.chars().any(|c| c.is_ascii_digit()) { "#" } else { w })
                            .take(8)
                            .collect();
                        words.join(" ")
                    })
                    .collect();
                Some(Self {
                    failure: true,
                    odd: failures.keys().cloned().collect(),
                    primary: kinds.iter().next().cloned().unwrap_or_default(),
                    kinds,
                })
            }
        }
    }

    /// The kind of a flattened field path: indices and addresses removed.
    pub fn kind(field: &str) -> String {
        if let Some(rest) = field.strip_prefix("receipts[") {
            return format!("receipts.{}", rest.split_once("].").map_or(rest, |(_, f)| f));
        }
        if let Some(rest) = field.strip_prefix("post.") {
            let mut parts = rest.split('.');
            parts.next();
            return format!("post.{}", parts.next().unwrap_or("deleted"));
        }
        field.to_owned()
    }

    /// Whether `other` is still the same finding: the same odd clients and, for divergences,
    /// this signature's primary field kind among its differences.
    pub fn reproduced_by(&self, other: &Self) -> bool {
        self.failure == other.failure
            && self.odd == other.odd
            && (self.failure || other.kinds.contains(&self.primary))
    }

    /// The grouping key: failure flag, odd clients, and primary kind.
    pub fn key(&self) -> (bool, BTreeSet<String>, String) {
        (self.failure, self.odd.clone(), self.primary.clone())
    }

    /// A one-line form for reports.
    pub fn describe(&self) -> String {
        let odd: Vec<&str> = self.odd.iter().map(String::as_str).collect();
        let what = if self.failure { "failure" } else { "differs" };
        format!("{} {what}: {}", odd.join("+"), self.primary)
    }
}

/// A reduced reproducer.
#[derive(Debug)]
pub struct Minimized {
    /// The reduced descriptor, when the finding came from a fuzz case.
    pub case: Option<FuzzCase>,
    /// The reduced request.
    pub request: Request,
    /// Its verdict.
    pub verdict: Verdict,
    /// Its responses.
    pub results: AdapterResults,
    /// Requests tried.
    pub attempts: usize,
}

/// Reduces a finding while it reproduces, by running candidates on the adapters.
#[derive(Debug)]
pub struct Minimizer<'a> {
    /// One process of every adapter.
    pub adapters: &'a mut [Adapter],
    /// Reference client name.
    pub reference: &'a str,
    /// Generator options the case was built with.
    pub options: &'a GeneratorOptions,
    /// Most candidate requests to run.
    pub budget: usize,
    /// Candidates run so far.
    pub attempts: usize,
}

impl Minimizer<'_> {
    /// Runs `request` on every adapter.
    pub fn run(adapters: &mut [Adapter], request: &Request) -> (AdapterResults, Verdict) {
        let line = serde_json::to_string(request).expect("requests serialize");
        let results: Vec<_> = adapters
            .iter_mut()
            .map(|adapter| (adapter.spec.name.clone(), adapter.call_line(&request.id, &line)))
            .collect();
        let verdict = Verdict::of(request, &results);
        (results, verdict)
    }

    fn test(&mut self, request: &Request, target: &FindingSignature) -> bool {
        if self.attempts >= self.budget {
            return false;
        }
        self.attempts += 1;
        let (_, verdict) = Self::run(self.adapters, request);
        FindingSignature::of(&verdict, self.reference).is_some_and(|s| target.reproduced_by(&s))
    }

    fn build(&self, case: &FuzzCase, id: &str) -> Option<Request> {
        let mut request =
            catch_unwind(AssertUnwindSafe(|| case.build(self.options).request)).ok()?;
        request.id = id.to_owned();
        Some(request)
    }

    fn test_case(&mut self, case: &FuzzCase, id: &str, target: &FindingSignature) -> bool {
        self.build(case, id).is_some_and(|request| self.test(&request, target))
    }

    /// Reduces a fuzz case: drops L1 edits, then chunks of slots, then tweaks and bytecode
    /// edits, then individual transactions of the resulting request.
    pub fn minimize_case(&mut self, case: &FuzzCase, target: &FindingSignature) -> Minimized {
        let id = format!("{}-min", case.id());
        let mut best = case.clone();
        for i in (0..best.l1.len()).rev() {
            let mut candidate = best.clone();
            candidate.l1.remove(i);
            if self.test_case(&candidate, &id, target) {
                best = candidate;
            }
        }
        let mut chunk = (best.slots.len() / 2).max(1);
        loop {
            let mut i = 0;
            while i < best.slots.len() && best.slots.len() > 1 {
                let mut candidate = best.clone();
                let end = (i + chunk).min(candidate.slots.len());
                candidate.slots.drain(i..end);
                if !candidate.slots.is_empty() && self.test_case(&candidate, &id, target) {
                    best = candidate;
                } else {
                    i += chunk;
                }
            }
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
        for slot in 0..best.slots.len() {
            for tweak in (0..best.slots[slot].tweaks.len()).rev() {
                let mut candidate = best.clone();
                candidate.slots[slot].tweaks.remove(tweak);
                if self.test_case(&candidate, &id, target) {
                    best = candidate;
                }
            }
            let edits = match &best.slots[slot].kind {
                SlotKind::Mutant { edits, .. } => edits.len(),
                _ => 0,
            };
            for edit in (0..edits).rev() {
                let mut candidate = best.clone();
                if let SlotKind::Mutant { edits, .. } = &mut candidate.slots[slot].kind {
                    edits.remove(edit);
                }
                if self.test_case(&candidate, &id, target) {
                    best = candidate;
                }
            }
        }
        let request = self.build(&best, &id).unwrap_or_else(|| case.build(self.options).request);
        let mut minimized = self.minimize_request(&request, target);
        minimized.case = Some(best);
        minimized
    }

    /// Reduces a request by dropping transactions one at a time (never the first, the L1
    /// attributes deposit).
    pub fn minimize_request(&mut self, request: &Request, target: &FindingSignature) -> Minimized {
        let mut best = request.clone();
        let mut i = best.transactions.len();
        while i > 1 {
            i -= 1;
            let mut candidate = best.clone();
            candidate.transactions.remove(i);
            if self.test(&candidate, target) {
                best = candidate;
            }
        }
        let (results, verdict) = Self::run(self.adapters, &best);
        Minimized { case: None, request: best, verdict, results, attempts: self.attempts }
    }
}

#[cfg(test)]
mod tests {
    use super::FindingSignature;

    #[test]
    fn field_kinds_drop_indices_and_addresses() {
        assert_eq!(FindingSignature::kind("receipts[3].status"), "receipts.status");
        assert_eq!(FindingSignature::kind("post.0xabc.storage.0x01"), "post.storage");
        assert_eq!(FindingSignature::kind("post.0xabc"), "post.deleted");
        assert_eq!(FindingSignature::kind("header.stateRoot"), "header.stateRoot");
    }
}
