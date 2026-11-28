//! `el decode`: in-process differential decoding of tiger against base-reth's crates.
//!
//! Each target generates valid inputs from the seed, mutates them, feeds the same bytes to both
//! sides, and compares what each side does: accept with a normalized value, or reject (with a
//! comparable code where both sides have one, such as a disconnect reason). Disagreements are
//! grouped by signature, minimized, and printed (and written as JSON with `--out`).

use std::{collections::BTreeMap, fmt::Write as _, time::Instant};

use alloy_primitives::hex;

use super::{Args, rlp, rng::Rng};

/// What one side did with an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Accepted. `None` means accepted without looking at the content (tiger ignores some
    /// messages), which agrees with any acceptance.
    Accept(Option<String>),
    /// Rejected. `code` is compared across sides when both have one; `detail` is informational.
    Reject { code: Option<String>, detail: String },
}

impl Verdict {
    /// An acceptance with a normalized value.
    pub(crate) fn ok(value: impl Into<String>) -> Self {
        Self::Accept(Some(value.into()))
    }

    /// An acceptance that did not decode the content.
    pub(crate) const fn ignored() -> Self {
        Self::Accept(None)
    }

    /// A rejection without a comparable code.
    pub(crate) fn err(detail: impl std::fmt::Debug) -> Self {
        Self::Reject { code: None, detail: format!("{detail:?}") }
    }

    /// A rejection with a code compared across sides.
    pub(crate) fn code(code: impl Into<String>, detail: impl std::fmt::Debug) -> Self {
        Self::Reject { code: Some(code.into()), detail: format!("{detail:?}") }
    }

    fn short(&self) -> String {
        match self {
            Self::Accept(None) => "accept(ignored)".into(),
            Self::Accept(Some(v)) => format!("accept({})", truncate(v, 160)),
            Self::Reject { code: Some(c), detail } => {
                format!("reject[{c}]({})", truncate(detail, 160))
            }
            Self::Reject { code: None, detail } => format!("reject({})", truncate(detail, 160)),
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}...", &s[..s.floor_char_boundary(n)]) }
}

/// The kind of a disagreement, or `None` when the sides agree.
fn classify(tiger: &Verdict, reth: &Verdict) -> Option<String> {
    use Verdict::{Accept, Reject};
    match (tiger, reth) {
        (Accept(Some(a)), Accept(Some(b))) if a != b => {
            Some(match (a.split_once(" | "), b.split_once(" | ")) {
                // Targets may prefix both values with a shared tag (`tag | value`) naming the kind
                // of input, so inputs of different kinds group apart.
                (Some((ta, _)), Some((tb, _))) if ta == tb => format!("values-differ ({ta})"),
                _ => "values-differ".into(),
            })
        }
        (Reject { code: Some(a), .. }, Reject { code: Some(b), .. }) if a != b => {
            Some(format!("reject-code tiger={a} reth={b}"))
        }
        (Accept(_), Accept(_)) | (Reject { .. }, Reject { .. }) => None,
        (Accept(None), Reject { .. }) => Some("tiger-ignores reth-rejects".into()),
        (Accept(_), Reject { detail, .. }) => {
            Some(format!("tiger-accepts reth: {}", reason(detail)))
        }
        (Reject { detail, .. }, Accept(_)) => {
            Some(format!("reth-accepts tiger: {}", reason(detail)))
        }
    }
}

/// An error's shape: its first three words, without numbers or hex, so equal causes group
/// together.
fn reason(detail: &str) -> String {
    let words: Vec<&str> = detail
        .split(|c: char| !(c.is_ascii_alphabetic() || c == '_'))
        .filter(|w| w.len() >= 2 && !w.chars().all(|c| c.is_ascii_hexdigit()))
        .take(3)
        .collect();
    words.join(" ")
}

/// A valid input and how to mutate it.
#[derive(Debug, Clone)]
pub(crate) struct Seed {
    /// The sub-case, passed back to `check` (for example `eth69/GetBlockHeaders`).
    pub(crate) case: String,
    /// The encoding, mutated by the generic RLP mutations when present.
    pub(crate) tree: Option<rlp::Node>,
    /// The valid bytes (the tree's encoding, or a non-RLP input).
    pub(crate) bytes: Vec<u8>,
    /// Target-private data the mutator needs (keys to re-sign or re-seal with).
    pub(crate) aux: Vec<u8>,
}

impl Seed {
    /// A seed whose bytes are its tree's encoding.
    pub(crate) fn tree(case: impl Into<String>, tree: rlp::Node) -> Self {
        let bytes = tree.encode();
        Self { case: case.into(), tree: Some(tree), bytes, aux: Vec::new() }
    }

    /// A seed of raw bytes.
    pub(crate) fn raw(case: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self { case: case.into(), tree: None, bytes, aux: Vec::new() }
    }
}

/// One decode target.
pub(crate) struct Target {
    /// Name used by `--target`.
    pub(crate) name: &'static str,
    /// A valid input.
    pub(crate) generate: fn(&mut Rng) -> Seed,
    /// A mutation of a valid input: bytes and the mutation's name. Targets without their own
    /// use [`default_mutate`].
    pub(crate) mutate: fn(&mut Rng, &Seed) -> (Vec<u8>, String),
    /// Runs both sides: (tiger, reth).
    pub(crate) check: fn(&str, &[u8]) -> (Verdict, Verdict),
    /// Whether replaying an input gives the same answer, so disagreements can be shrunk.
    pub(crate) minimize: bool,
}

/// The generic mutation: RLP-structured when the seed has a tree, byte-level otherwise.
pub(crate) fn default_mutate(rng: &mut Rng, seed: &Seed) -> (Vec<u8>, String) {
    match &seed.tree {
        Some(tree) => {
            let (bytes, name) = rlp::mutate(rng, tree);
            (bytes, name.to_string())
        }
        None => byte_mutate(rng, &seed.bytes),
    }
}

/// Byte-level mutations for inputs without RLP structure.
pub(crate) fn byte_mutate(rng: &mut Rng, input: &[u8]) -> (Vec<u8>, String) {
    let mut out = input.to_vec();
    let name = match rng.below(6) {
        0 if !out.is_empty() => {
            let cut = rng.below(out.len());
            out.truncate(cut);
            "truncate"
        }
        1 => {
            let n = 1 + rng.below(8);
            out.extend(rng.bytes(n));
            "trailing-bytes"
        }
        2 if !out.is_empty() => {
            let at = rng.below(out.len());
            out.remove(at);
            "byte-delete"
        }
        3 => {
            let at = rng.below(out.len() + 1);
            out.insert(at, rng.next_u64() as u8);
            "byte-insert"
        }
        _ if !out.is_empty() => {
            let at = rng.below(out.len());
            out[at] ^= 1 << rng.below(8);
            "byte-flip"
        }
        _ => {
            out.push(rng.next_u64() as u8);
            "byte-insert"
        }
    };
    (out, name.to_string())
}

/// A disagreement group.
#[derive(Debug, Clone, serde::Serialize)]
struct Group {
    target: String,
    case: String,
    class: String,
    count: u64,
    mutations: BTreeMap<String, u64>,
    first_iteration: u64,
    input_hex: String,
    minimized_hex: String,
    tiger: String,
    reth: String,
}

/// Shrinks `input` while the disagreement class stays the same.
fn minimize(
    check: fn(&str, &[u8]) -> (Verdict, Verdict),
    case: &str,
    input: &[u8],
    class: &str,
) -> Vec<u8> {
    let same = |candidate: &[u8]| {
        let (t, r) = check(case, candidate);
        classify(&t, &r).as_deref() == Some(class)
    };
    let mut best = input.to_vec();
    let mut budget = 3000usize;
    let mut chunk = best.len().div_ceil(2).max(1);
    while chunk >= 1 && budget > 0 {
        let mut i = 0;
        let mut progressed = false;
        while i < best.len() && budget > 0 {
            let end = (i + chunk).min(best.len());
            let mut candidate = best[..i].to_vec();
            candidate.extend_from_slice(&best[end..]);
            budget -= 1;
            if same(&candidate) {
                best = candidate;
                progressed = true;
            } else {
                i += chunk;
            }
        }
        if !progressed {
            if chunk == 1 {
                break;
            }
            chunk = chunk.div_ceil(2);
        }
    }
    best
}

/// Runs `el decode`.
pub(crate) fn run(args: &Args, targets: &[Target]) -> Result<(), String> {
    let selected: Vec<&Target> = targets
        .iter()
        .filter(|t| args.target.as_deref().is_none_or(|name| name == t.name || name == "all"))
        .collect();
    if selected.is_empty() {
        let names: Vec<&str> = targets.iter().map(|t| t.name).collect();
        return Err(format!("unknown target; choose from {}", names.join(", ")));
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut report = String::new();
    for target in selected {
        let started = Instant::now();
        let mut rng = Rng::new(args.seed ^ fxhash(target.name));
        let (mut valid_agree, mut mutated, mut agree, mut disagree) = (0u64, 0u64, 0u64, 0u64);
        let (mut tiger_accepts, mut reth_accepts) = (0u64, 0u64);
        for iteration in 0..args.iters {
            let seed = (target.generate)(&mut rng);
            // Every fourth input is the valid one unchanged; the rest are mutated.
            let (input, mutation) = if iteration % 4 == 0 {
                (seed.bytes.clone(), "none".to_string())
            } else {
                mutated += 1;
                (target.mutate)(&mut rng, &seed)
            };
            let (tiger, reth) = (target.check)(&seed.case, &input);
            tiger_accepts += u64::from(matches!(tiger, Verdict::Accept(_)));
            reth_accepts += u64::from(matches!(reth, Verdict::Accept(_)));
            let Some(class) = classify(&tiger, &reth) else {
                agree += 1;
                if mutation == "none" {
                    valid_agree += 1;
                }
                continue;
            };
            disagree += 1;
            let key = format!("{}|{}|{}", target.name, seed.case, class);
            let group = groups.entry(key).or_insert_with(|| {
                let minimized = if target.minimize {
                    minimize(target.check, &seed.case, &input, &class)
                } else {
                    input.clone()
                };
                let (t, r) = if target.minimize {
                    (target.check)(&seed.case, &minimized)
                } else {
                    (tiger.clone(), reth.clone())
                };
                Group {
                    target: target.name.to_string(),
                    case: seed.case.clone(),
                    class: class.clone(),
                    count: 0,
                    mutations: BTreeMap::new(),
                    first_iteration: iteration,
                    input_hex: hex::encode(&input),
                    minimized_hex: hex::encode(&minimized),
                    tiger: t.short(),
                    reth: r.short(),
                }
            });
            group.count += 1;
            *group.mutations.entry(mutation).or_default() += 1;
        }
        let _ = writeln!(
            report,
            "{:<10} iters={} mutated={} agree={} disagree={} (valid inputs agreeing: {}) tiger-accepts={} reth-accepts={} {:.1}s",
            target.name,
            args.iters,
            mutated,
            agree,
            disagree,
            valid_agree,
            tiger_accepts,
            reth_accepts,
            started.elapsed().as_secs_f64()
        );
    }
    println!("== el decode, seed {} ==", args.seed);
    print!("{report}");
    println!("== {} disagreement signatures ==", groups.len());
    for group in groups.values() {
        println!(
            "[{}] {} | {} | count={} first@{} mutations={:?}\n    minimized: {}\n    tiger: {}\n    reth:  {}",
            group.target,
            group.case,
            group.class,
            group.count,
            group.first_iteration,
            group.mutations,
            truncate(&group.minimized_hex, 400),
            group.tiger,
            group.reth
        );
    }
    if let Some(path) = &args.out {
        let groups: Vec<&Group> = groups.values().collect();
        let json = serde_json::json!({ "seed": args.seed, "iters": args.iters, "summary": report, "groups": groups });
        std::fs::write(path, serde_json::to_string_pretty(&json).unwrap())
            .map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

/// A stable hash of a target name, so each target draws its own stream from one seed.
fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}
