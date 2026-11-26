//! The RPC differential campaign: the chain advances in rounds, and between rounds every node
//! answers the same generated requests.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use alloy_primitives::keccak256;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Rng,
    nodes::{ChainFollower, NodeClient, Reply, RpcCase, RpcCompare, RpcGen, RpcGuide, RpcVerdict},
};

/// Campaign settings.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// Request generator seed.
    pub seed: u64,
    /// Blocks added per round, until the chain reaches `max_blocks`.
    pub blocks_per_round: u64,
    /// Chain length after which the chain stops growing.
    pub max_blocks: u64,
    /// Requests per round.
    pub requests_per_round: u64,
    /// Worker threads sending requests.
    pub jobs: usize,
    /// How long to run.
    pub duration: Duration,
    /// Families to generate (all when empty).
    pub families: Vec<String>,
    /// Examples saved per finding signature.
    pub examples: usize,
    /// Output directory.
    pub out: PathBuf,
    /// When set, every request is appended to `<request_log>.<job>` before it is sent, so the
    /// request in flight when a node crashes is the last line of its worker's log.
    pub request_log: Option<PathBuf>,
    /// Cases left out for some nodes, such as requests known to crash one.
    pub excludes: Vec<RpcExclude>,
    /// Nodes whose `fuzz_coverage` edges guide the requests (see [`RpcGuide`]); empty for
    /// unguided rounds without coverage.
    pub coverage_from: Vec<String>,
    /// With `coverage_from`, mutate corpus requests (`true`) or only measure coverage of fresh
    /// requests (`false`).
    pub guided: bool,
    /// Percentage of guided requests that are fresh instead of mutants.
    pub explore_percent: u64,
    /// Compare error messages and texts too (see [`RpcCompare::normalize`]).
    pub strict_errors: bool,
}

/// Leaves a node out of the cases whose method and body match, written
/// `node=method[:body substring]`; a method ending in `*` is a prefix. Used to keep a campaign
/// going past a known crash until the node is fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcExclude {
    /// Node name.
    pub node: String,
    /// Method, or method prefix ending in `*`.
    pub method: String,
    /// Substring the request body must contain (empty matches every body).
    pub body: String,
}

impl RpcExclude {
    /// Parses `node=method[:body substring]`.
    pub fn parse(spec: &str) -> eyre::Result<Self> {
        let (node, rest) = spec
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("exclude must be node=method[:text]: {spec}"))?;
        let (method, body) = rest.split_once(':').unwrap_or((rest, ""));
        Ok(Self { node: node.to_owned(), method: method.to_owned(), body: body.to_owned() })
    }

    /// Whether `node` is left out of `case`. The method is looked for in the body too, so a
    /// batch or malformed case that carries the method is left out as well.
    pub fn matches(&self, node: &str, case: &RpcCase) -> bool {
        let (prefix, exact) =
            self.method.strip_suffix('*').map_or((self.method.as_str(), true), |p| (p, false));
        let quoted = if exact { format!("\"{prefix}\"") } else { format!("\"{prefix}") };
        let method = if exact { case.method == prefix } else { case.method.starts_with(prefix) };
        node == self.node
            && (method || case.body.contains(&quoted))
            && case.body.contains(&self.body)
    }
}

/// Agreement and coverage counters for one method or family.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MethodStats {
    /// Requests sent.
    pub requests: u64,
    /// Requests every node answered identically.
    pub agree: u64,
    /// Requests where every node returned an error.
    pub all_errors: u64,
    /// Requests the reference answered with a result.
    pub success: u64,
    /// Requests the reference answered with an error, by error code (`transport` for no JSON
    /// body, `batch` for a batch reply).
    pub errors: BTreeMap<String, u64>,
    /// Requests by the number of nodes compared.
    pub nodes: BTreeMap<usize, u64>,
    /// Requests by coarse parameter shape ([`RpcCase::shape_of`]), at most
    /// [`MethodStats::MAX_SHAPES`] distinct shapes; the rest count under `other`.
    pub shapes: BTreeMap<String, u64>,
}

impl MethodStats {
    /// Most distinct shapes kept per method.
    pub const MAX_SHAPES: usize = 400;

    /// Counts one request; `reference` is the reference's normalized reply.
    pub fn count(
        &mut self,
        agree: bool,
        all_errors: bool,
        reference: &Value,
        nodes: usize,
        shape: &str,
    ) {
        self.requests += 1;
        self.agree += agree as u64;
        self.all_errors += all_errors as u64;
        *self.nodes.entry(nodes).or_default() += 1;
        if let Some(error) = reference.get("error") {
            *self
                .errors
                .entry(error.get("code").map_or_else(|| "none".into(), Value::to_string))
                .or_default() += 1;
        } else if reference.get("result").is_some() {
            self.success += 1;
        } else {
            *self.errors.entry(RpcVerdict::kind(reference)).or_default() += 1;
        }
        let key = if self.shapes.len() < Self::MAX_SHAPES || self.shapes.contains_key(shape) {
            shape
        } else {
            "other"
        };
        *self.shapes.entry(key.to_owned()).or_default() += 1;
    }
}

/// One group of disagreements with the same signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcFinding {
    /// Method, node grouping, and the shapes of the first differing paths.
    pub signature: String,
    /// Occurrences.
    pub count: u64,
    /// Directory holding saved examples.
    pub dir: String,
}

/// Campaign summary, rewritten every round to `<out>/rpc-summary.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RpcSummary {
    /// Rounds run.
    pub rounds: u64,
    /// Chain head.
    pub head: u64,
    /// Blocks the chain has grown by.
    pub blocks: u64,
    /// Why the chain stopped growing, if it did before `max_blocks`.
    pub chain_stopped: Option<String>,
    /// Requests sent.
    pub requests: u64,
    /// Requests every node answered identically.
    pub agree: u64,
    /// Per-method counters.
    pub methods: BTreeMap<String, MethodStats>,
    /// Per-family counters.
    pub families: BTreeMap<String, MethodStats>,
    /// Disagreement groups by signature.
    pub findings: BTreeMap<String, RpcFinding>,
    /// Per method, disagreements where the reference answered the same request differently
    /// twice; not findings.
    pub nondeterministic: BTreeMap<String, u64>,
    /// Per node, the slowest reply seen (milliseconds) and its method.
    pub slowest_ms: BTreeMap<String, (u64, String)>,
    /// Reference error codes per method (`method code`), to see what the generator reaches.
    pub reference_errors: BTreeMap<String, u64>,
    /// Per known cause, the requests it explained; the other nodes were compared without the
    /// node the cause affects (see [`RpcCampaign::KNOWN_BASE_GENESIS`]).
    pub known: BTreeMap<String, u64>,
    /// Per method, the nodes that serve it. A node that answers a well-formed request with
    /// "method not found" (-32601) does not serve it and is left out of that method's
    /// comparisons; methods fewer than two nodes serve are not sent.
    pub supported: BTreeMap<String, Vec<String>>,
    /// Seconds run.
    pub elapsed_secs: u64,
}

/// The campaign driver.
#[derive(Debug)]
pub struct RpcCampaign;

impl RpcCampaign {
    /// One agreeing case in this many is appended to `<out>/samples.jsonl`, to check what the
    /// generator reaches.
    pub const SAMPLE_EVERY: u64 = 500;

    /// Stack size of request workers.
    pub const WORKER_STACK: usize = 256 << 20;

    /// Runs the campaign until `config.duration` passes.
    pub fn run(follower: &mut ChainFollower, config: &RpcConfig) -> eyre::Result<RpcSummary> {
        fs::create_dir_all(config.out.join("rpc-findings"))?;
        let started = Instant::now();
        let summary = Arc::new(Mutex::new(RpcSummary {
            supported: Self::probe(follower, config.seed, &config.excludes),
            ..RpcSummary::default()
        }));
        let guide = (!config.coverage_from.is_empty())
            .then(|| {
                RpcGuide::new(
                    &mut follower.nodes,
                    config.coverage_from.clone(),
                    config.guided,
                    config.explore_percent,
                    &config.out,
                )
            })
            .transpose()?;
        let mut round = 0u64;
        while started.elapsed() < config.duration {
            round += 1;
            let filters = Self::install_filters(follower, config.seed, round);
            let stopped = summary.lock().expect("lock").chain_stopped.is_some();
            if !stopped && follower.chain.blocks() < config.max_blocks {
                for _ in 0..config.blocks_per_round {
                    if let Err(error) = follower.step() {
                        let reason = format!("{error:#}");
                        eprintln!("chain stopped at {}: {reason}", follower.index.head().number);
                        fs::write(config.out.join("chain-stopped.txt"), &reason)?;
                        summary.lock().expect("lock").chain_stopped = Some(reason);
                        break;
                    }
                }
            }
            Self::poll_filters(follower, filters, &summary, config);
            match &guide {
                Some(guide) => guide.round(follower, config, round, &summary),
                None => Self::fuzz_round(follower, config, round, &summary),
            }
            // A node that stopped answering (crashed or killed) turns every later request into
            // a transport failure; stop so the cause stays the newest finding.
            let down: Vec<&str> = follower
                .nodes
                .iter()
                .filter(|node| matches!(node.call("eth_chainId", &json!([])), Reply::Transport(_)))
                .map(|node| node.name.as_str())
                .collect();
            if !down.is_empty() {
                let reason =
                    format!("nodes stopped answering after round {round}: {}", down.join(","));
                fs::write(config.out.join("node-down.txt"), &reason)?;
                eyre::bail!(reason);
            }
            let mut s = summary.lock().expect("lock");
            s.rounds = round;
            s.head = follower.index.head().number;
            s.blocks = follower.chain.blocks();
            s.elapsed_secs = started.elapsed().as_secs();
            fs::write(config.out.join("rpc-summary.json"), serde_json::to_vec_pretty(&*s)?)?;
            eprintln!(
                "round {round}: head {} requests {} agree {:.2}% signatures {}",
                s.head,
                s.requests,
                100.0 * s.agree as f64 / s.requests.max(1) as f64,
                s.findings.len()
            );
            if config.requests_per_round == 0
                && (s.chain_stopped.is_some() || s.blocks >= config.max_blocks)
            {
                break;
            }
        }
        let s = summary.lock().expect("lock").clone();
        Ok(s)
    }

    fn fuzz_round(
        follower: &ChainFollower,
        config: &RpcConfig,
        round: u64,
        summary: &Arc<Mutex<RpcSummary>>,
    ) {
        let per_job = config.requests_per_round.div_ceil(config.jobs as u64);
        let (index, state, nodes) = (&follower.index, follower.chain.state(), &follower.nodes);
        thread::scope(|scope| {
            for job in 0..config.jobs {
                let summary = Arc::clone(summary);
                // Deeply nested traces are parsed, compared, and dropped recursively.
                let worker = thread::Builder::new().stack_size(Self::WORKER_STACK);
                worker
                    .spawn_scoped(scope, move || {
                        let mut rng = Rng::new(&[config.seed, round, job as u64]);
                        let supported = summary.lock().expect("lock").supported.clone();
                        let mut sent = 0;
                        while sent < per_job {
                            let case =
                                RpcGen { rng: &mut rng, index, state }.case(&config.families);
                            let Some(serving) = supported.get(&case.method) else {
                                continue;
                            };
                            let serving: Vec<String> = serving
                                .iter()
                                .filter(|name| {
                                    !config.excludes.iter().any(|e| e.matches(name, &case))
                                })
                                .cloned()
                                .collect();
                            if serving.len() < 2 {
                                continue;
                            }
                            sent += 1;
                            if let Some(path) = &config.request_log {
                                Self::log_request(path, job, &case);
                            }
                            let replies: Vec<(String, Reply)> = nodes
                                .iter()
                                .filter(|n| serving.contains(&n.name))
                                .map(|n| {
                                    let sent = Instant::now();
                                    let reply = n.send(&case.body);
                                    let ms = sent.elapsed().as_millis() as u64;
                                    let mut s = summary.lock().expect("lock");
                                    let slowest = s.slowest_ms.entry(n.name.clone()).or_default();
                                    if ms > slowest.0 {
                                        *slowest = (ms, case.method.clone());
                                    }
                                    drop(s);
                                    (n.name.clone(), reply)
                                })
                                .collect();
                            let serving: Vec<&NodeClient> =
                                nodes.iter().filter(|n| serving.contains(&n.name)).collect();
                            Self::record(
                                &case,
                                &replies,
                                &serving,
                                index.head().number,
                                &summary,
                                config,
                            );
                        }
                    })
                    .expect("spawning a request worker");
            }
        });
    }

    /// Appends one request body to a worker's request log.
    pub fn log_request(path: &std::path::Path, job: usize, case: &RpcCase) {
        let path = path.with_extension(job.to_string());
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", case.body);
        }
    }

    /// Finds which nodes serve each method the generator emits.
    /// Nodes an exclude matches for a method's sample count as serving it, unasked.
    pub fn probe(
        follower: &ChainFollower,
        seed: u64,
        excludes: &[RpcExclude],
    ) -> BTreeMap<String, Vec<String>> {
        let mut rng = Rng::new(&[seed, u64::MAX - 1]);
        let mut samples: BTreeMap<String, RpcCase> = BTreeMap::new();
        for family in crate::nodes::FAMILIES.iter().map(|(f, _)| *f).filter(|f| *f != "malformed") {
            for _ in 0..400 {
                let case =
                    RpcGen { rng: &mut rng, index: &follower.index, state: follower.chain.state() }
                        .family(family);
                samples.entry(case.method.clone()).or_insert(case);
            }
        }
        for method in [
            "eth_newBlockFilter",
            "eth_newFilter",
            "eth_getFilterChanges",
            "eth_getFilterLogs",
            "eth_uninstallFilter",
        ] {
            samples.insert(method.to_owned(), RpcCase::new("filters", method, json!([])));
        }
        samples
            .into_iter()
            .map(|(method, case)| {
                let serving = follower
                    .nodes
                    .iter()
                    .filter(|node| {
                        excludes.iter().any(|e| e.matches(&node.name, &case))
                            || match node.send(&case.body) {
                                Reply::Body(body) => body["error"]["code"] != json!(-32601),
                                Reply::Transport(_) => true,
                            }
                    })
                    .map(|node| node.name.clone())
                    .collect();
                (method, serving)
            })
            .collect()
    }

    /// Times the reference is asked again before a disagreement counts as deterministic. Some
    /// answers vary only now and then, such as which of several invalid overrides base-reth
    /// names (it iterates a randomly seeded map).
    pub const REFERENCE_RETRIES: usize = 3;

    /// Compares the replies to one case and records the outcome.
    ///
    /// `serving` are the nodes that answered, in `replies` order; with them, a disagreement is
    /// checked and shrunk. The request is sent to the reference again (up to
    /// [`Self::REFERENCE_RETRIES`] times), and if any answer differs the answer is not
    /// deterministic (such as a `pending` block's random `prevRandao`) and the case is counted
    /// in `nondeterministic` instead of becoming a finding. The first case of a new signature
    /// is minimized (see [`Self::minimize`]).
    pub fn record(
        case: &RpcCase,
        replies: &[(String, Reply)],
        serving: &[&NodeClient],
        head: u64,
        summary: &Mutex<RpcSummary>,
        config: &RpcConfig,
    ) -> RpcVerdict {
        let mut normalized: Vec<(String, Value)> = replies
            .iter()
            .map(|(name, reply)| {
                (name.clone(), RpcCompare::normalize(&case.method, reply, config.strict_errors))
            })
            .collect();
        let mut replies = replies.to_vec();
        let mut serving = serving.to_vec();
        // The base/base causes were found against Nethermind as the reference. Without Nethermind,
        // base is the reference and every node must match it, bugs included.
        let nethermind = normalized.iter().any(|(name, _)| name == "nethermind");
        let known = nethermind
            .then(|| {
                Self::known_base_cause(case, &replies, &normalized)
                    .or_else(|| Self::known_base_difference(case, &replies, &normalized))
                    .or_else(|| Self::settled_unfunded_fee(case, &replies, &normalized))
            })
            .flatten()
            // Before the BLOCKHASH cause: a request every node refused ran no code.
            .or_else(|| Self::override_order(case, &replies))
            .or_else(|| Self::persisted_blockhash(case, head, &normalized))
            .or_else(|| Self::precompile_delegation(case, &normalized));
        if Self::simulate_encoding_only(case, &normalized) {
            *summary
                .lock()
                .expect("lock")
                .known
                .entry(Self::KNOWN_SIMULATE_HEADER.into())
                .or_default() += 1;
            normalized.iter_mut().for_each(|(_, v)| *v = Self::without_block_encoding(v));
        } else if let Some((cause, index)) = known {
            *summary.lock().expect("lock").known.entry(cause.into()).or_default() += 1;
            if cause == Self::KNOWN_BASE_OVERRIDE_ORDER {
                // Every node named an account with both fields, and any node may pick a
                // different one (tiger iterates the overrides in no fixed order either), so the
                // cause explains them all: only base's reply is kept.
                let base = normalized[index].0.clone();
                normalized.retain(|(name, _)| *name == base);
                replies.retain(|(name, _)| *name == base);
                serving.retain(|node| node.name == base);
            } else {
                normalized.remove(index);
                replies.remove(index);
                if index < serving.len() {
                    serving.remove(index);
                }
            }
        }
        let (replies, serving) = (replies.as_slice(), serving.as_slice());
        let verdict = RpcVerdict::compare(&normalized);
        // A node that sent no JSON body (crashed, hung, or killed) is a finding even when every
        // node failed the same way.
        let failed: Vec<&str> = normalized
            .iter()
            .filter(|(_, value)| value.get("transport").is_some())
            .map(|(name, _)| name.as_str())
            .collect();
        if !verdict.agree
            && failed.is_empty()
            && let Some(reference) = serving.first()
            && let Some(again) =
                (0..Self::REFERENCE_RETRIES).map(|_| reference.send(&case.body)).find(|again| {
                    RpcCompare::normalize(&case.method, again, config.strict_errors)
                        != normalized[0].1
                })
        {
            let mut s = summary.lock().expect("lock");
            s.requests += 1;
            let count = s.nondeterministic.entry(case.method.clone()).or_default();
            *count += 1;
            let count = *count;
            drop(s);
            // Kept so each nondeterministic class can be explained, not only counted.
            if count as usize <= config.examples
                && let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(config.out.join("nondeterministic.jsonl"))
            {
                let line = json!({
                    "method": case.method,
                    "head": head,
                    "request": serde_json::from_str::<Value>(&case.body).unwrap_or_else(|_| json!(case.body)),
                    "responses": Self::raw(replies),
                    "referenceAgain": Self::raw(&[(serving[0].name.clone(), again)]),
                });
                let _ = writeln!(file, "{line}");
            }
            return verdict;
        }
        let all_errors = normalized.iter().all(|(_, v)| v.get("error").is_some());
        let mut s = summary.lock().expect("lock");
        s.requests += 1;
        if let Some(code) =
            normalized[0].1.get("error").map(|e| e.get("code").cloned().unwrap_or_default())
        {
            *s.reference_errors.entry(format!("{} {code}", case.method)).or_default() += 1;
        }
        let reference = &normalized[0].1;
        s.methods.entry(case.method.clone()).or_default().count(
            verdict.agree,
            all_errors,
            reference,
            replies.len(),
            &case.shape,
        );
        s.families.entry(case.family.clone()).or_default().count(
            verdict.agree,
            all_errors,
            reference,
            replies.len(),
            &case.method,
        );
        if verdict.agree {
            s.agree += 1;
        }
        if verdict.agree && failed.is_empty() {
            let sample = s.agree.is_multiple_of(Self::SAMPLE_EVERY);
            drop(s);
            if sample {
                let line = json!({ "family": case.family, "request": case.body, "response": normalized[0].1 });
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(config.out.join("samples.jsonl"))
                {
                    let _ = writeln!(file, "{line}");
                }
            }
            return verdict;
        }
        let signature = if failed.is_empty() {
            Self::signature(case, &verdict)
        } else {
            format!("{} | transport failure: {}", case.method, failed.join(","))
        };
        let id = keccak256(signature.as_bytes()).to_string()[2..14].to_owned();
        let dir = config.out.join("rpc-findings").join(&id);
        let finding = s.findings.entry(signature.clone()).or_insert_with(|| RpcFinding {
            signature: signature.clone(),
            count: 0,
            dir: dir.display().to_string(),
        });
        finding.count += 1;
        let count = finding.count;
        drop(s);
        if count as usize <= config.examples {
            let _ = fs::create_dir_all(&dir);
            let raw = Self::raw(replies);
            let example = json!({
                "signature": signature,
                "family": case.family,
                "method": case.method,
                "head": head,
                "request": serde_json::from_str::<Value>(&case.body).unwrap_or_else(|_| json!(case.body)),
                "verdict": verdict,
                "responses": raw,
            });
            let _ = fs::write(
                dir.join(format!("{count}.json")),
                serde_json::to_vec(&example).unwrap_or_default(),
            );
            if count == 1 && !serving.is_empty() && failed.is_empty() {
                let (minimized, replies) = Self::minimize(
                    case,
                    serving,
                    &signature,
                    Self::MINIMIZE_BUDGET,
                    config.strict_errors,
                );
                let example = json!({
                    "signature": signature,
                    "method": case.method,
                    "head": head,
                    "request": serde_json::from_str::<Value>(&minimized.body).unwrap_or_else(|_| json!(minimized.body)),
                    "responses": Self::raw(&replies),
                });
                let _ = fs::write(
                    dir.join("minimized.json"),
                    serde_json::to_vec(&example).unwrap_or_default(),
                );
            }
        }
        verdict
    }

    /// base/base treats a nonzero genesis as block 0: "earliest" resolves to block 0,
    /// `eth_feeHistory` refuses ranges reaching below genesis, and
    /// `eth_getTransactionBySenderAndNonce` searches history from block 0. This is known and
    /// set aside for now, so these cases are compared on the other nodes only.
    pub const KNOWN_BASE_GENESIS: &str = "base: nonzero genesis treated as block 0";

    /// base/base ignores parameters beyond a method's last one, where the other clients refuse
    /// them as invalid params. Reported; compared on the other nodes only, because the mutator
    /// adds parameters often and base then answers with whatever the method returns (sometimes a
    /// trace too large to send).
    pub const KNOWN_BASE_EXTRA_PARAMS: &str = "base: parameters beyond the last are ignored";

    /// base/base answers a request whose id it cannot read (a negative or fractional number, a
    /// boolean, array or object) with no reply or a parse error. Reported; compared on the other
    /// nodes only.
    pub const KNOWN_BASE_ID: &str = "base: unreadable id gets no reply";

    /// base/base's first poll of a new block filter includes the head at install time (B2).
    pub const KNOWN_BASE_BLOCK_FILTER: &str = "base: block filter's first poll includes the head";
    /// base/base answers -32001 or -32003 (EIP-1474) where the others give -32000, and refuses fee
    /// fields above 2^128 as invalid params (B4).
    pub const KNOWN_BASE_ERROR_CODES: &str = "base: EIP-1474 codes and 128-bit fee fields";
    /// base/base answers eth\_getProof at "pending" with -32001, and its eth\_config lists Base
    /// precompiles (B5).
    pub const KNOWN_BASE_PROOF_CONFIG: &str = "base: pending proof and eth_config precompiles";
    /// base/base's callTracer frame for a system deposit after Regolith, which fails before any
    /// frame starts, has `to` 0x0 and no input; Nethermind builds it from the tx (B11).
    pub const KNOWN_BASE_FAILED_DEPOSIT_FRAME: &str = "base: failed system deposit frame";
    /// base/base's BLOCKHASH under a `number` block override above the state block answers the
    /// canonical hash for blocks it has persisted and zero for blocks still in its in-memory
    /// canonical state, so the answer depends on how far persistence lags the head: reth persists
    /// once 50 blocks are in memory, down to head−5, but while blocks arrive quickly the backlog
    /// grows past 51 (seen at more than 60). Tiger answers the hash up to head−51, zero above.
    pub const KNOWN_BASE_PERSISTED_BLOCKHASH: &str =
        "base: blockhash above the state block depends on persistence";
    /// When several state-override accounts set both `state` and `stateDiff`, base/base names a
    /// random one of them: it checks them in the order of a randomly seeded map.
    pub const KNOWN_BASE_OVERRIDE_ORDER: &str = "base: names a random invalid override account";
    /// A state override giving a precompile address 7702 delegation code: base/base (revm) runs
    /// the precompile, tiger (evm2) follows the delegation. Only overrides reach it (no one can
    /// sign an authorization for a precompile address); accepted as won't-fix (finding R7).
    pub const ACCEPTED_PRECOMPILE_DELEGATION: &str =
        "accepted: 7702 delegation on a precompile (R7, won't fix)";
    /// base/base accepts a JSON number as a call's `type`.
    pub const KNOWN_BASE_INTEGER_TYPE: &str = "base: integer type accepted";

    /// The known base/base cause, if any, that explains how base answered `case`, with base's
    /// index. The cause's cases are compared on the other nodes only.
    pub fn known_base_cause(
        case: &RpcCase,
        replies: &[(String, Reply)],
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let base = normalized.iter().position(|(n, _)| n == "base")?;
        let reference = &normalized[base].1;
        let others: Vec<&(String, Value)> =
            normalized.iter().enumerate().filter(|(i, _)| *i != base).map(|(_, o)| o).collect();
        if others.is_empty() {
            return None;
        }
        if Self::base_genesis_bug(case, normalized) {
            return Some((Self::KNOWN_BASE_GENESIS, base));
        }
        // Nethermind names the cause in its message: "too many arguments, want at most N".
        let too_many = replies.iter().any(|(name, reply)| {
            name == "nethermind"
                && matches!(reply, Reply::Body(body)
                    if body["error"]["message"].as_str().is_some_and(|m| m.starts_with("too many arguments")))
        });
        if too_many && others.iter().all(|(_, v)| v != reference) {
            return Some((Self::KNOWN_BASE_EXTRA_PARAMS, base));
        }
        let id =
            serde_json::from_str::<Value>(&case.body).ok().and_then(|body| body.get("id").cloned());
        // Ids jsonrpsee cannot read: a negative or fractional number, a boolean, array or object.
        let odd_id = id.as_ref().is_some_and(|id| match id {
            Value::Number(n) => n.as_f64().is_some_and(|id| id < 0.0 || id.fract() != 0.0),
            Value::Bool(_) | Value::Array(_) | Value::Object(_) => true,
            Value::Null | Value::String(_) => false,
        });
        if odd_id
            && (reference == &json!({ "empty": true }) || reference["error"]["code"] == -32700)
        {
            return Some((Self::KNOWN_BASE_ID, base));
        }
        // The rest need the other nodes agreeing against base.
        let theirs = &others[0].1;
        if theirs == reference || others.iter().any(|(_, v)| v != theirs) {
            return None;
        }
        let code = |v: &Value| v["error"]["code"].as_i64();
        let ok = |v: &Value| v.get("result").is_some();
        let body: Value = serde_json::from_str(&case.body).unwrap_or_default();
        let params = &body["params"];
        let nm_message = replies.iter().find_map(|(name, reply)| match reply {
            Reply::Body(body) if name == "nethermind" => body["error"]["message"].as_str(),
            _ => None,
        });
        // "invalid argument N" for the last parameter, beyond what the method takes.
        let last_argument = params.as_array().is_some_and(|p| {
            p.len() > 1 && nm_message == Some(format!("invalid argument {}", p.len() - 1).as_str())
        });
        let wide_fee = Self::any_object(params, &|o| {
            o.iter().any(|(key, v)| {
                (key.contains("Fee") || key == "gasPrice")
                    && v.as_str().is_some_and(|s| s.len() > 34)
            })
        });
        let raw = |name: &str| {
            replies.iter().find(|(n, _)| n == name).and_then(|(_, reply)| match reply {
                Reply::Body(body) => Some(body.to_string()),
                Reply::Transport(_) => None,
            })
        };
        let system_deposit = raw("nethermind")
            .is_some_and(|b| b.contains("system deposit after Regolith"))
            && raw("base").is_some_and(|b| {
                b.contains("\"to\":\"0x0000000000000000000000000000000000000000\"")
            });
        let cause = if system_deposit {
            Self::KNOWN_BASE_FAILED_DEPOSIT_FRAME
        } else if case.method == "eth_getFilterChanges"
            && case.body.contains("eth_newBlockFilter")
            && ok(reference)
        {
            Self::KNOWN_BASE_BLOCK_FILTER
        } else if matches!(code(reference), Some(-32001 | -32003)) && code(theirs) == Some(-32000)
            || code(reference) == Some(-32602) && wide_fee
        {
            Self::KNOWN_BASE_ERROR_CODES
        } else if case.method == "eth_config"
            || case.method == "eth_getProof"
                && Self::strings(params).contains(&"pending")
                && code(reference) == Some(-32001)
        {
            Self::KNOWN_BASE_PROOF_CONFIG
        } else if code(theirs) == Some(-32602)
            && Self::any_object(params, &|o| o.get("type").is_some_and(Value::is_number))
        {
            Self::KNOWN_BASE_INTEGER_TYPE
        } else if last_argument && code(reference) != Some(-32602) {
            Self::KNOWN_BASE_EXTRA_PARAMS
        } else {
            return None;
        };
        Some((cause, base))
    }

    /// base/base refuses EIP-1898-style block parameters that Nethermind accepts (a block hash,
    /// `""` or `null`) and refuses an index of 2^64 or more that Nethermind answers with `null`.
    pub const KNOWN_BASE_BLOCK_PARAM: &str = "base: strict block parameter or index";
    /// base/base accepts `0X`-prefixed hashes, which Nethermind refuses.
    pub const KNOWN_BASE_UPPER_PREFIX: &str = "base: 0X prefix accepted";
    /// base/base refuses odd-length hex data, access-list entries without `storageKeys`, and
    /// authorizations without `r` or `s`, which Nethermind accepts.
    pub const KNOWN_BASE_STRICT_FIELDS: &str = "base: strict request fields";
    /// base/base checks `eth_simulateV1` calls in another order than Nethermind, so the first
    /// error differs.
    pub const KNOWN_BASE_SIMULATE_ORDER: &str = "base: simulate check order";
    /// base/base answers a priced estimate from an unfunded sender otherwise than Nethermind's
    /// (and geth's) "gas required exceeds allowance (0)".
    pub const KNOWN_BASE_ESTIMATE_ALLOWANCE: &str = "base: estimate without allowance (0)";
    /// base/base's prestate tracer lists another set of accounts than Nethermind's (which follows
    /// geth): it drops `codeHash` and the authorities of invalid EIP-7702 authorizations, lists
    /// the zero-credit fee vaults instead of the coinbase for a fee-free call, and keeps an
    /// empty sender in a diffMode `pre`, and lists a slot whose SLOAD or SSTORE ran out of gas.
    /// Only the account and slot sets differ; shared entries agree.
    pub const KNOWN_BASE_PRESTATE: &str = "base: prestate account set";
    /// base/base's 4byteTracer (revm-inspectors) counts calls to precompiles, which Nethermind's
    /// (with geth's) skips.
    pub const KNOWN_BASE_4BYTE_PRECOMPILES: &str = "base: 4byteTracer counts precompile calls";

    /// base/base accepts call requests of type 0x7e (deposit) and `debug_traceBlockByNumber` at
    /// "pending", which Nethermind refuses.
    pub const KNOWN_BASE_DEPOSIT_CALL_PENDING_TRACE: &str =
        "base: deposit-typed calls and pending traces accepted";
    /// base/base prices a call with a tip and no fee cap at base fee + tip (alloy's `CallFees`),
    /// where geth and Nethermind leave the cap at 0 and fail with "max priority fee per gas higher
    /// than max fee per gas".
    pub const KNOWN_BASE_TIP_WITHOUT_CAP: &str = "base: tip without fee cap priced";
    /// base/base's eth\_createAccessList reports a single run without the access list (its list,
    /// gas and error); geth and Nethermind rerun with the list until it stops changing and report
    /// the final run.
    pub const KNOWN_BASE_ACCESS_LIST_GAS: &str = "base: access list from a single run";
    /// base/base keeps running a precompile whose address has a state override; geth and
    /// Nethermind turn it into an ordinary account (geth's `override.Apply`).
    pub const KNOWN_BASE_PRECOMPILE_OVERRIDE: &str = "base: overridden precompile still runs";
    /// base/base keeps the block's fork rules under a block override of `time`, so an earlier
    /// time still applies the Prague calldata floor ("intrinsic gas too low"); geth and Nethermind
    /// take the rules from the overridden time (B12).
    pub const KNOWN_BASE_OVERRIDE_TIME_RULES: &str = "base: fork rules ignore a time override";
    /// Settled base/base differences from Nethermind: base accepts `eth_feeHistory` without
    /// rewardPercentiles, refuses a `null` boolean argument, and refuses tracer names it has no
    /// native tracer for, which Nethermind runs as JS tracers.
    pub const KNOWN_BASE_SETTLED: &str = "base: settled (feeHistory, null boolean, JS tracer)";

    /// RPC fee handling for unfunded senders is non-geth in all three clients (coordinator's
    /// call, settled): a priced debug\_traceCall from a sender that can't pay gives "intrinsic gas
    /// too low" in base/base and needlefish (gas capped by the allowance) and runs in Nethermind
    /// (no gas charge), where geth refuses it with insufficient funds.
    pub const SETTLED_UNFUNDED_FEE: &str = "settled: RPC fees for unfunded senders";

    /// Simulated block encoding: Nethermind computes `stateRoot` and `withdrawalsRoot` and uses a
    /// zero `parentBeaconBlockRoot`, as geth does, where base/base (and needlefish, following it)
    /// leave both roots zero and copy the parent's beacon root (settled); and base/base gives a
    /// simulated legacy tx the EIP-155 `v` where geth and Nethermind give 0 (a base difference,
    /// counted once needlefish matches Nethermind's transactions). Block and tx hashes, roots and
    /// sizes follow from these. Such cases are compared without those derived fields.
    pub const KNOWN_SIMULATE_HEADER: &str = "settled: simulated header roots and tx encoding";

    /// Tracer names base/base implements natively.
    const NATIVE_TRACERS: [&str; 6] = [
        "callTracer",
        "prestateTracer",
        "4byteTracer",
        "noopTracer",
        "flatCallTracer",
        "muxTracer",
    ];

    /// The known base/base difference from Nethermind, the RPC reference, if any, that explains
    /// base's answer to `case`, with base's index: needlefish answers like Nethermind and base
    /// otherwise. base's case is then compared on the other nodes only.
    pub fn known_base_difference(
        case: &RpcCase,
        replies: &[(String, Reply)],
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let get = |name: &str| normalized.iter().position(|(n, _)| n == name);
        let (base, needlefish, nm) = (get("base")?, get("needlefish")?, get("nethermind")?);
        let (ours, reference) = (&normalized[base].1, &normalized[nm].1);
        if reference != &normalized[needlefish].1 || reference == ours {
            return None;
        }
        let code = |v: &Value| v["error"]["code"].as_i64();
        let ok = |v: &Value| v.get("result").is_some();
        let message = match &replies[nm].1 {
            Reply::Body(body) => body["error"]["message"].as_str().unwrap_or_default().to_owned(),
            Reply::Transport(_) => return None,
        };
        let base_message = match &replies[base].1 {
            Reply::Body(body) => body["error"]["message"].as_str().unwrap_or_default().to_owned(),
            Reply::Transport(_) => return None,
        };
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let params = body.get("params").and_then(Value::as_array);
        let strings = Self::strings(&body["params"]);
        let null_bool = case.method.starts_with("eth_getBlockBy")
            && params.is_some_and(|p| p.get(1).is_some_and(Value::is_null));
        let named_tracer = |o: &serde_json::Map<String, Value>| {
            o.get("tracer")
                .and_then(Value::as_str)
                .is_some_and(|t| !Self::NATIVE_TRACERS.contains(&t))
        };
        let cause = if message.contains("max priority fee per gas higher than max fee per gas") {
            Self::KNOWN_BASE_TIP_WITHOUT_CAP
        } else if case.method == "eth_createAccessList" && ok(ours) && ok(reference) {
            Self::KNOWN_BASE_ACCESS_LIST_GAS
        } else if base_message == "intrinsic gas too low"
            && Self::any_object(&body["params"], &|o| o.contains_key("time"))
        {
            Self::KNOWN_BASE_OVERRIDE_TIME_RULES
        } else if Self::any_object(&body["params"], &|o| {
            o.iter().any(|(key, value)| value.is_object() && Self::is_precompile(key))
        }) {
            Self::KNOWN_BASE_PRECOMPILE_OVERRIDE
        } else if ok(ours)
            && code(reference).is_some()
            && (body["params"].to_string().contains("\"type\":\"0x7e\"")
                || case.method == "debug_traceBlockByNumber" && strings.contains(&"pending"))
        {
            Self::KNOWN_BASE_DEPOSIT_CALL_PENDING_TRACE
        } else if (case.method == "eth_feeHistory"
            && params.is_some_and(|p| p.len() == 2)
            && ok(ours)
            && code(reference).is_some())
            || (null_bool && ok(reference) && code(ours) == Some(-32602))
            || (code(ours) == Some(-32603) && Self::any_object(&body["params"], &named_tracer))
        {
            Self::KNOWN_BASE_SETTLED
        } else if ok(reference) && code(ours) == Some(-32602) {
            let last = params.and_then(|p| p.last());
            let block_like = last.is_some_and(|v| {
                v.is_null() || v == "" || v.as_str().is_some_and(|s| s.len() == 66)
            });
            let big_index = params.is_some_and(|p| {
                p.iter().any(|v| {
                    v.as_str().is_some_and(|s| s.len() > 18 && s.starts_with("0x") && s.len() < 66)
                })
            });
            let odd_hex = strings
                .iter()
                .any(|s| s.len() > 3 && s.len() % 2 == 1 && RpcCompare::is_hex(s) && s.len() != 66);
            let lenient = body["params"].to_string().contains("\"accessList\"")
                && Self::any_object(&body["params"], &|o| {
                    o.contains_key("address") && !o.contains_key("storageKeys")
                })
                || Self::any_object(&body["params"], &|o| {
                    o.contains_key("chainId")
                        && o.contains_key("nonce")
                        && o.contains_key("address")
                        && !(o.contains_key("r") && o.contains_key("s"))
                });
            if odd_hex || lenient {
                Self::KNOWN_BASE_STRICT_FIELDS
            } else if block_like || big_index {
                Self::KNOWN_BASE_BLOCK_PARAM
            } else {
                return None;
            }
        } else if code(reference) == Some(-32602) && ok(ours) && case.body.contains("\"0X") {
            Self::KNOWN_BASE_UPPER_PREFIX
        } else if case.method == "eth_simulateV1"
            && code(reference).is_some()
            && code(ours).is_some()
        {
            Self::KNOWN_BASE_SIMULATE_ORDER
        } else if case.method == "eth_estimateGas"
            && message.starts_with("gas required exceeds allowance (0)")
        {
            Self::KNOWN_BASE_ESTIMATE_ALLOWANCE
        } else if case.body.contains("4byteTracer") && Self::counts_within(reference, ours) {
            Self::KNOWN_BASE_4BYTE_PRECOMPILES
        } else if case.body.contains("prestateTracer")
            && Self::without_extra_accounts(reference, ours)
                == Self::without_extra_accounts(ours, reference)
        {
            Self::KNOWN_BASE_PRESTATE
        } else {
            return None;
        };
        Some((cause, base))
    }

    /// [`Self::KNOWN_BASE_PERSISTED_BLOCKHASH`] with base's index, when `case` can reach it: an
    /// eth\_call or eth\_estimateGas whose `number` override N is above its state block B, so a
    /// BLOCKHASH(n) with B < n < N, n ≥ N − 256, and n ≤ head − 5 is possible, and
    /// the other node disagrees with base. A state block given as a tag other than latest or
    /// pending counts as block 0.
    pub fn persisted_blockhash(
        case: &RpcCase,
        head: u64,
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let base = normalized.iter().position(|(n, _)| n == "base")?;
        if normalized.iter().all(|(_, v)| *v == normalized[base].1)
            || !matches!(case.method.as_str(), "eth_call" | "eth_estimateGas")
        {
            return None;
        }
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let number = |v: &Value| u64::from_str_radix(v.as_str()?.strip_prefix("0x")?, 16).ok();
        let over = number(&body["params"][3]["number"])?;
        let tag = &body["params"][1];
        let state = match tag.as_str() {
            None if tag.is_null() => head,
            Some("latest" | "pending") => head,
            Some(_) => number(tag).unwrap_or(0),
            None => number(&tag["blockNumber"]).unwrap_or(0),
        };
        let low = state + 1;
        let high = head.saturating_sub(5).min(over.saturating_sub(1));
        (low <= high && high + 256 >= over).then_some((Self::KNOWN_BASE_PERSISTED_BLOCKHASH, base))
    }

    /// [`Self::KNOWN_BASE_OVERRIDE_ORDER`] with base's index, when every node refused `case` by
    /// naming an account that sets both `state` and `stateDiff` in its overrides, and the names
    /// differ. In a batch, each reply that differs must be explained this way (matched by `id`),
    /// and every other reply must be identical.
    pub fn override_order(
        case: &RpcCase,
        replies: &[(String, Reply)],
    ) -> Option<(&'static str, usize)> {
        let base = replies.iter().position(|(n, _)| n == "base")?;
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let bodies: Vec<&Value> = replies
            .iter()
            .map(|(_, reply)| match reply {
                Reply::Body(body) => Some(body),
                Reply::Transport(_) => None,
            })
            .collect::<Option<_>>()?;
        let Value::Array(requests) = &body else {
            return Self::names_differ(&body, &bodies, base)
                .filter(|differ| *differ)
                .map(|_| (Self::KNOWN_BASE_OVERRIDE_ORDER, base));
        };
        let by_id = |reply: &Value| -> Option<BTreeMap<String, Value>> {
            reply.as_array().map(|items| {
                items.iter().map(|item| (item["id"].to_string(), item.clone())).collect()
            })
        };
        let maps: Vec<BTreeMap<String, Value>> =
            bodies.iter().map(|b| by_id(b)).collect::<Option<_>>()?;
        if maps.iter().any(|m| m.keys().ne(maps[base].keys())) {
            return None;
        }
        let request_ids: Vec<String> = requests.iter().map(|r| r["id"].to_string()).collect();
        let mut explained = false;
        for (id, reference) in &maps[base] {
            let items: Vec<&Value> = maps.iter().map(|m| &m[id]).collect();
            if items.iter().all(|item| *item == reference) {
                continue;
            }
            let index = request_ids.iter().position(|r| r == id)?;
            let request = &requests[index];
            if Self::names_differ(request, &items, base) != Some(true) {
                return None;
            }
            explained = true;
        }
        explained.then_some((Self::KNOWN_BASE_OVERRIDE_ORDER, base))
    }

    /// For one request and each node's reply to it: `Some(whether the named accounts differ)`
    /// when every reply names an account that sets both `state` and `stateDiff` in the
    /// request's overrides, else `None`.
    fn names_differ(request: &Value, replies: &[&Value], base: usize) -> Option<bool> {
        let overrides = request["params"][2].as_object()?;
        let named = |reply: &Value| -> Option<String> {
            let message = reply["error"]["message"].as_str()?;
            let account = message
                .strip_prefix("account ")?
                .strip_suffix(" has both 'state' and 'stateDiff'")?;
            let invalid = overrides.iter().any(|(address, o)| {
                address.eq_ignore_ascii_case(account)
                    && o.get("state").is_some()
                    && o.get("stateDiff").is_some()
            });
            invalid.then(|| account.to_lowercase())
        };
        let names: Vec<String> = replies.iter().map(|reply| named(reply)).collect::<Option<_>>()?;
        Some(names.iter().any(|n| *n != names[base]))
    }

    /// [`Self::ACCEPTED_PRECOMPILE_DELEGATION`] with base's index, when the nodes disagree on
    /// an eth\_call, eth\_estimateGas or eth\_createAccessList whose state overrides give a
    /// precompile address (0x01 to 0x11, or 0x100) code `0xef0100` followed by an address.
    pub fn precompile_delegation(
        case: &RpcCase,
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let base = normalized.iter().position(|(n, _)| n == "base")?;
        if normalized.iter().all(|(_, v)| *v == normalized[base].1)
            || !matches!(
                case.method.as_str(),
                "eth_call" | "eth_estimateGas" | "eth_createAccessList"
            )
        {
            return None;
        }
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let delegated = body["params"][2].as_object()?.iter().any(|(address, o)| {
            let precompile = address
                .strip_prefix("0x")
                .filter(|hex| hex.len() == 40)
                .and_then(|hex| {
                    u64::from_str_radix(&hex[24..], 16)
                        .ok()
                        .filter(|_| hex[..24].bytes().all(|b| b == b'0'))
                })
                .is_some_and(|n| (0x01..=0x11).contains(&n) || n == 0x100);
            let code = o["code"].as_str().unwrap_or_default().to_lowercase();
            precompile && code.len() == 2 + 46 && code.starts_with("0xef0100")
        });
        delegated.then_some((Self::ACCEPTED_PRECOMPILE_DELEGATION, base))
    }

    /// [`Self::SETTLED_UNFUNDED_FEE`] with Nethermind's index, when it explains `case`: a priced
    /// debug\_traceCall that base/base and needlefish both refuse with "intrinsic gas too low"
    /// and Nethermind runs.
    pub fn settled_unfunded_fee(
        case: &RpcCase,
        replies: &[(String, Reply)],
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let get = |name: &str| normalized.iter().position(|(n, _)| n == name);
        let (base, needlefish, nm) = (get("base")?, get("needlefish")?, get("nethermind")?);
        let low = |index: usize| match &replies[index].1 {
            Reply::Body(body) => body["error"]["message"] == "intrinsic gas too low",
            Reply::Transport(_) => false,
        };
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let call = &body["params"][0];
        (case.method == "debug_traceCall"
            && (call.get("gasPrice").is_some() || call.get("maxFeePerGas").is_some())
            && normalized[base].1 == normalized[needlefish].1
            && low(base)
            && low(needlefish)
            && normalized[nm].1.get("result").is_some())
        .then_some((Self::SETTLED_UNFUNDED_FEE, nm))
    }

    /// Whether `case`'s replies, eth\_simulateV1 blocks, differ only as
    /// [`Self::KNOWN_SIMULATE_HEADER`] describes: they agree without the fields
    /// [`Self::without_block_encoding`] drops, and needlefish's transactions match Nethermind's.
    pub fn simulate_encoding_only(case: &RpcCase, normalized: &[(String, Value)]) -> bool {
        let get = |name: &str| normalized.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        let (Some(base), Some(needlefish), Some(nm)) =
            (get("base"), get("needlefish"), get("nethermind"))
        else {
            return false;
        };
        let transactions = |v: &Value| {
            v["result"]
                .as_array()
                .map(|blocks| blocks.iter().map(|b| b["transactions"].clone()).collect::<Vec<_>>())
        };
        let stripped = Self::without_block_encoding(nm);
        case.method == "eth_simulateV1"
            && !(base == nm && needlefish == nm)
            && transactions(needlefish) == transactions(nm)
            && Self::without_block_encoding(base) == stripped
            && Self::without_block_encoding(needlefish) == stripped
    }

    /// `value`, an eth\_simulateV1 reply, without each block's hash, parent hash, roots, size
    /// and transactions, and without its logs' block and transaction hashes.
    pub fn without_block_encoding(value: &Value) -> Value {
        const DERIVED: [&str; 9] = [
            "hash",
            "parentHash",
            "stateRoot",
            "withdrawalsRoot",
            "parentBeaconBlockRoot",
            "transactionsRoot",
            "receiptsRoot",
            "size",
            "transactions",
        ];
        let mut value = value.clone();
        for block in value.get_mut("result").and_then(Value::as_array_mut).into_iter().flatten() {
            if let Some(object) = block.as_object_mut() {
                DERIVED.iter().for_each(|key| {
                    object.remove(*key);
                });
            }
            for call in block.get_mut("calls").and_then(Value::as_array_mut).into_iter().flatten() {
                for log in call.get_mut("logs").and_then(Value::as_array_mut).into_iter().flatten()
                {
                    if let Some(log) = log.as_object_mut() {
                        log.remove("blockHash");
                        log.remove("transactionHash");
                    }
                }
            }
        }
        value
    }

    /// Whether `value` matches `reference` except that its numeric counts may be lower or
    /// missing (a 4byteTracer result that leaves some calls out).
    fn counts_within(value: &Value, reference: &Value) -> bool {
        match (value, reference) {
            (Value::Object(object), Value::Object(theirs)) => object.iter().all(|(key, v)| {
                theirs.get(key).is_some_and(|r| match (v.as_u64(), r.as_u64()) {
                    (Some(count), Some(limit)) => count <= limit,
                    _ => Self::counts_within(v, r),
                })
            }),
            (Value::Array(items), Value::Array(theirs)) => {
                items.len() == theirs.len()
                    && items.iter().zip(theirs).all(|(v, r)| Self::counts_within(v, r))
            }
            _ => value == reference,
        }
    }

    /// Every string inside `value`.
    fn strings(value: &Value) -> Vec<&str> {
        match value {
            Value::String(s) => vec![s.as_str()],
            Value::Array(items) => items.iter().flat_map(Self::strings).collect(),
            Value::Object(object) => object.values().flat_map(Self::strings).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether any object inside `value` satisfies `test`.
    /// Whether `address` is a precompile on Base: 0x01 to 0x11, or P256VERIFY at 0x0100.
    fn is_precompile(address: &str) -> bool {
        let Some(hex) = address.strip_prefix("0x").filter(|h| h.len() == 40) else {
            return false;
        };
        u64::from_str_radix(&hex[24..], 16).is_ok_and(|n| {
            hex[..24].bytes().all(|b| b == b'0') && ((1..=0x11).contains(&n) || n == 0x100)
        })
    }

    fn any_object(value: &Value, test: &dyn Fn(&serde_json::Map<String, Value>) -> bool) -> bool {
        match value {
            Value::Object(object) => {
                test(object) || object.values().any(|v| Self::any_object(v, test))
            }
            Value::Array(items) => items.iter().any(|v| Self::any_object(v, test)),
            _ => false,
        }
    }

    /// `value` without the `codeHash` fields, the account (address-keyed) entries and the storage
    /// (slot-keyed) entries that `reference` lacks at the same place.
    fn without_extra_accounts(value: &Value, reference: &Value) -> Value {
        match (value, reference) {
            (Value::Object(object), Value::Object(theirs)) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| {
                        theirs.contains_key(*key)
                            || !(key.as_str() == "codeHash"
                                || ([42, 66].contains(&key.len()) && RpcCompare::is_hex(key)))
                    })
                    .map(|(key, v)| {
                        let reference = theirs.get(key).unwrap_or(&Value::Null);
                        (key.clone(), Self::without_extra_accounts(v, reference))
                    })
                    .collect(),
            ),
            (Value::Array(items), Value::Array(theirs)) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        Self::without_extra_accounts(v, theirs.get(i).unwrap_or(&Value::Null))
                    })
                    .collect(),
            ),
            _ => value.clone(),
        }
    }

    /// Whether `normalized` (reference first) shows [`Self::KNOWN_BASE_GENESIS`]: base failed
    /// (an error or a `null` result) a request that names `earliest` or is a fee-history or
    /// sender-and-nonce lookup, while every other node answered it with a result (`null` included:
    /// a sender-and-nonce lookup of a nonce never used is `null`).
    pub fn base_genesis_bug(case: &RpcCase, normalized: &[(String, Value)]) -> bool {
        let Some(base) = normalized.iter().position(|(n, _)| n == "base") else { return false };
        let reference = &normalized[base].1;
        let others: Vec<&(String, Value)> =
            normalized.iter().enumerate().filter(|(i, _)| *i != base).map(|(_, o)| o).collect();
        let failed = |v: &Value| v.get("error").is_some() || v.get("result") == Some(&Value::Null);
        !others.is_empty()
            && failed(reference)
            && (others.iter().all(|(_, v)| v.get("result").is_some())
                // Base cannot find the block ("block not found: earliest", -32001 or -32000) where the others
                // find it and answer alike, even with an execution error.
                || ([-32001, -32000].contains(&reference["error"]["code"].as_i64().unwrap_or(0))
                    && others.iter().all(|(_, v)| v == &others[0].1 && v != reference)))
            && (case.body.contains("\"earliest\"")
                || case.method == "eth_feeHistory"
                || case.method == "eth_getTransactionBySenderAndNonce")
    }

    /// Candidate requests one minimization may send.
    pub const MINIMIZE_BUDGET: usize = 80;

    /// Largest reply saved whole in an example; a larger one is saved as its size and first
    /// bytes.
    pub const MAX_SAVED_REPLY: usize = 1 << 20;

    /// Replies by node name, transport failures as `{"transport": text}`.
    pub fn raw(replies: &[(String, Reply)]) -> BTreeMap<String, Value> {
        replies
            .iter()
            .map(|(name, reply)| {
                (
                    name.clone(),
                    match reply {
                        Reply::Body(body) => {
                            let text = body.to_string();
                            if text.len() > Self::MAX_SAVED_REPLY {
                                let cut = text.floor_char_boundary(4096);
                                json!({ "truncatedBytes": text.len(), "head": &text[..cut] })
                            } else {
                                body.clone()
                            }
                        }
                        Reply::Transport(text) => json!({ "transport": text }),
                    },
                )
            })
            .collect()
    }

    /// Shrinks a disagreeing case while its signature stays the same: drops object members and
    /// array elements of the parameters, largest first, and empties long hex strings. Returns the
    /// smallest case found and its replies.
    pub fn minimize(
        case: &RpcCase,
        serving: &[&NodeClient],
        signature: &str,
        budget: usize,
        strict_errors: bool,
    ) -> (RpcCase, Vec<(String, Reply)>) {
        let send = |case: &RpcCase| -> Vec<(String, Reply)> {
            serving.iter().map(|n| (n.name.clone(), n.send(&case.body))).collect()
        };
        let mut best = case.clone();
        let mut best_replies = send(&best);
        let Ok(mut envelope) = serde_json::from_str::<Value>(&case.body) else {
            return (best, best_replies);
        };
        let mut tries = 0;
        'outer: while tries < budget {
            for candidate in Self::shrinks(&envelope["params"]) {
                if tries >= budget {
                    break 'outer;
                }
                // An opcode trace (no `tracer`, or an empty or non-string one) without `limit` can be
                // gigabytes.
                if case.method.starts_with("debug_trace")
                    && !candidate.as_array().and_then(|p| p.last()).is_some_and(|config| {
                        config.get("tracer").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                            || config.get("limit").is_some()
                    })
                {
                    continue;
                }
                tries += 1;
                let mut trial = envelope.clone();
                trial["params"] = candidate;
                let trial_case = RpcCase { body: trial.to_string(), ..case.clone() };
                let replies = send(&trial_case);
                let normalized: Vec<(String, Value)> = replies
                    .iter()
                    .map(|(name, reply)| {
                        (name.clone(), RpcCompare::normalize(&case.method, reply, strict_errors))
                    })
                    .collect();
                let verdict = RpcVerdict::compare(&normalized);
                if !verdict.agree && Self::signature(case, &verdict) == signature {
                    envelope = trial;
                    best = trial_case;
                    best_replies = replies;
                    continue 'outer;
                }
            }
            break;
        }
        (best, best_replies)
    }

    /// Every one-step shrink of `value`, larger removals first.
    fn shrinks(value: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        match value {
            Value::Object(object) => {
                for key in object.keys() {
                    let mut smaller = object.clone();
                    smaller.remove(key);
                    out.push(Value::Object(smaller));
                }
                for (key, child) in object {
                    for shrunk in Self::shrinks(child) {
                        let mut smaller = object.clone();
                        smaller.insert(key.clone(), shrunk);
                        out.push(Value::Object(smaller));
                    }
                }
            }
            Value::Array(items) => {
                for i in (0..items.len()).rev() {
                    let mut smaller = items.clone();
                    smaller.remove(i);
                    out.push(Value::Array(smaller));
                }
                for (i, child) in items.iter().enumerate() {
                    for shrunk in Self::shrinks(child) {
                        let mut smaller = items.clone();
                        smaller[i] = shrunk;
                        out.push(Value::Array(smaller));
                    }
                }
            }
            Value::String(text) if text.len() > 66 && RpcCompare::is_hex(text) => {
                out.push(json!("0x"))
            }
            _ => {}
        }
        out
    }

    /// Method, node grouping, and the shapes of the first two differing paths per node.
    pub fn signature(case: &RpcCase, verdict: &RpcVerdict) -> String {
        let groups: Vec<String> = verdict
            .groups
            .iter()
            .zip(&verdict.kinds)
            .map(|(g, kind)| format!("{}:{kind}", g.join(",")))
            .collect();
        let mut shapes: Vec<String> = verdict
            .differences
            .iter()
            .flat_map(|(_, diffs)| diffs.iter().take(2).map(|d| RpcCompare::shape(&d.path)))
            .collect();
        shapes.sort();
        shapes.dedup();
        format!("{} | {} | {}", case.method, groups.join(" / "), shapes.join(" "))
    }

    /// Installs a block filter and a log filter on every node before the chain advances.
    fn install_filters(
        follower: &ChainFollower,
        seed: u64,
        round: u64,
    ) -> Vec<(RpcCase, Vec<Option<Value>>)> {
        let mut rng = Rng::new(&[seed, round, u64::MAX]);
        let mut g = RpcGen { rng: &mut rng, index: &follower.index, state: follower.chain.state() };
        let mut log_filter = g.family("logs").params().unwrap_or_default()[0].clone();
        if log_filter.get("blockHash").is_none() && g.rng.chance(50) {
            log_filter["fromBlock"] = json!("latest");
            log_filter.as_object_mut().map(|o| o.remove("toBlock"));
        }
        let cases = [
            RpcCase::new("filters", "eth_newBlockFilter", json!([])),
            RpcCase::new("filters", "eth_newFilter", json!([log_filter])),
        ];
        cases
            .into_iter()
            .map(|case| {
                let ids = follower
                    .nodes
                    .iter()
                    .map(|node| match node.send(&case.body) {
                        Reply::Body(body) => Some(body),
                        Reply::Transport(_) => None,
                    })
                    .collect();
                (case, ids)
            })
            .collect()
    }

    /// Polls the filters installed before the chain advanced, then removes them.
    fn poll_filters(
        follower: &ChainFollower,
        filters: Vec<(RpcCase, Vec<Option<Value>>)>,
        summary: &Mutex<RpcSummary>,
        config: &RpcConfig,
    ) {
        let head = follower.index.head().number;
        let supported = summary.lock().expect("lock").supported.clone();
        for (install, ids) in filters {
            let serving = supported.get(&install.method).cloned().unwrap_or_default();
            if serving.len() < 2 {
                continue;
            }
            let mut methods = vec!["eth_getFilterChanges"];
            if install.method == "eth_newFilter" {
                methods.push("eth_getFilterLogs");
            }
            methods.push("eth_uninstallFilter");
            for method in methods {
                let replies: Vec<(String, Reply)> = follower
                    .nodes
                    .iter()
                    .zip(&ids)
                    .filter(|(node, _)| serving.contains(&node.name))
                    .map(|(node, id)| {
                        // A node that refused the install answers every step with that refusal.
                        let reply = id.as_ref().map_or_else(
                            || Reply::Transport("filter was not installed".into()),
                            |body| {
                                body.get("result").map_or_else(
                                    || Reply::Body(body.clone()),
                                    |id| node.call(method, &json!([id])),
                                )
                            },
                        );
                        (node.name.clone(), reply)
                    })
                    .collect();
                let mut case = RpcCase::new("filters", method, json!(["<filter id>"]));
                case.shape = format!("{}:{}", install.method, install.shape);
                case.body = json!({ "install": serde_json::from_str::<Value>(&install.body).unwrap_or_default(), "poll": method }).to_string();
                Self::record(&case, &replies, &[], head, summary, config);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn base_genesis_bug_needs_base_failing_where_the_others_answer() {
        let earliest = RpcCase::new("state", "eth_getBalance", json!(["0x01", "earliest"]));
        let latest = RpcCase::new("state", "eth_getBalance", json!(["0x01", "latest"]));
        let err = json!({"id":1,"error":{"code":-32001}});
        let ok = json!({"id":1,"result":"0x1"});
        // Nethermind, the RPC reference, comes first.
        let replies = |base: &Value, other: &Value| {
            vec![
                ("nethermind".to_owned(), other.clone()),
                ("base".to_owned(), base.clone()),
                ("needlefish".to_owned(), other.clone()),
            ]
        };
        assert!(RpcCampaign::base_genesis_bug(&earliest, &replies(&err, &ok)));
        assert!(RpcCampaign::base_genesis_bug(
            &earliest,
            &replies(&json!({"id":1,"result":null}), &ok)
        ));
        // Not the known cause: another tag, everyone failing, or base answering.
        assert!(!RpcCampaign::base_genesis_bug(&latest, &replies(&err, &ok)));
        assert!(!RpcCampaign::base_genesis_bug(&earliest, &replies(&err, &err)));
        assert!(!RpcCampaign::base_genesis_bug(&earliest, &replies(&ok, &ok)));
        // Base not finding the block where the others execute it alike (here a revert).
        let reverted = json!({"id":1,"error":{"code":3}});
        assert!(RpcCampaign::base_genesis_bug(&earliest, &replies(&err, &reverted)));
        let invalid = json!({"id":1,"error":{"code":-32602}});
        assert!(!RpcCampaign::base_genesis_bug(&earliest, &replies(&invalid, &reverted)));

        // Extra parameters: known only when Nethermind names them and base answers otherwise.
        let raw = |base: &Value, message: &str| {
            vec![
                (
                    "nethermind".to_owned(),
                    Reply::Body(json!({"id":1,"error":{"code":-32602,"message":message}})),
                ),
                ("base".to_owned(), Reply::Body(base.clone())),
                ("needlefish".to_owned(), Reply::Body(invalid.clone())),
            ]
        };
        let known = |case: &RpcCase, base: &Value, message: &str| {
            RpcCampaign::known_base_cause(case, &raw(base, message), &replies(base, &invalid)).map(
                |(cause, index)| {
                    assert_eq!(index, 1, "base's index");
                    cause
                },
            )
        };
        assert_eq!(
            known(&latest, &ok, "too many arguments, want at most 2"),
            Some(RpcCampaign::KNOWN_BASE_EXTRA_PARAMS)
        );
        assert_eq!(known(&latest, &ok, "invalid argument 0"), None);
        assert_eq!(known(&latest, &invalid, "too many arguments, want at most 2"), None);
        // A negative or fractional id with no reply from base; an ordinary id is not explained.
        let empty = json!({"empty": true});
        let with_id = |id: Value| {
            let mut case = latest.clone();
            let mut body: Value = serde_json::from_str(&case.body).unwrap();
            body["id"] = id;
            case.body = body.to_string();
            case
        };
        assert_eq!(known(&with_id(json!(-1)), &empty, ""), Some(RpcCampaign::KNOWN_BASE_ID));
        assert_eq!(known(&with_id(json!(1.5)), &empty, ""), Some(RpcCampaign::KNOWN_BASE_ID));
        assert_eq!(known(&with_id(json!([1])), &empty, ""), Some(RpcCampaign::KNOWN_BASE_ID));
        let parse = json!({"id":null,"error":{"code":-32700}});
        assert_eq!(known(&with_id(json!(1.5)), &parse, ""), Some(RpcCampaign::KNOWN_BASE_ID));
        assert_eq!(known(&with_id(json!(1)), &empty, ""), None);

        // Tracked base differences: needlefish and Nethermind agree against base.
        let classify = |case: &RpcCase, base: Value, other: Value, message: &str| {
            let mut nm = other.clone();
            if let Some(error) = nm.get_mut("error") {
                error["message"] = json!(message);
            }
            let raw = vec![
                ("nethermind".to_owned(), Reply::Body(nm)),
                ("base".to_owned(), Reply::Body(base.clone())),
                ("needlefish".to_owned(), Reply::Body(other.clone())),
            ];
            RpcCampaign::known_base_cause(case, &raw, &replies(&base, &other)).map(|(c, _)| c)
        };
        let server = json!({"id":1,"error":{"code":-32000}});
        let not_found = json!({"id":1,"error":{"code":-32001}});
        assert_eq!(
            classify(&latest, not_found.clone(), server, ""),
            Some(RpcCampaign::KNOWN_BASE_ERROR_CODES)
        );
        assert_eq!(classify(&latest, not_found, invalid.clone(), ""), None);
        let typed = RpcCase::new("call", "eth_call", json!([{"type": 0}, "latest"]));
        assert_eq!(
            classify(&typed, ok.clone(), invalid.clone(), "invalid argument 0"),
            Some(RpcCampaign::KNOWN_BASE_INTEGER_TYPE)
        );
        let extra = RpcCase::new("call", "eth_createAccessList", json!([{}, "safe", {}, {}]));
        assert_eq!(
            classify(&extra, ok.clone(), invalid.clone(), "invalid argument 3"),
            Some(RpcCampaign::KNOWN_BASE_EXTRA_PARAMS)
        );
        assert_eq!(classify(&extra, ok, invalid, "invalid argument 0"), None);
        let trace = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "callTracer"}]),
        );
        let zero = json!({"id":1,"result":{"type":"CALL","to":"0x0000000000000000000000000000000000000000","input":"0x"}});
        let real = json!({"id":1,"result":{"type":"CALL","to":"0x00000000000000000000000000000000000000aa","input":"0x","error":"failed deposit: system deposit after Regolith"}});
        let raw = vec![
            ("nethermind".to_owned(), Reply::Body(real.clone())),
            ("base".to_owned(), Reply::Body(zero.clone())),
            ("needlefish".to_owned(), Reply::Body(real.clone())),
        ];
        assert_eq!(
            RpcCampaign::known_base_cause(&trace, &raw, &replies(&zero, &real)).map(|(c, _)| c),
            Some(RpcCampaign::KNOWN_BASE_FAILED_DEPOSIT_FRAME)
        );
    }

    #[test]
    fn known_base_differences_need_needlefish_agreeing_with_nethermind() {
        // Nethermind, the RPC reference, comes first. Returns the cause and the dropped node.
        let classify = |case: &RpcCase, base: Value, needlefish: Value, nm: Value| {
            let replies = vec![
                ("nethermind".to_owned(), Reply::Body(nm)),
                ("base".to_owned(), Reply::Body(base)),
                ("needlefish".to_owned(), Reply::Body(needlefish)),
            ];
            let normalized: Vec<(String, Value)> = replies
                .iter()
                .map(|(n, r)| (n.clone(), RpcCompare::normalize(&case.method, r, false)))
                .collect();
            if RpcCampaign::simulate_encoding_only(case, &normalized) {
                return Some((RpcCampaign::KNOWN_SIMULATE_HEADER, "all".to_owned()));
            }
            RpcCampaign::known_base_difference(case, &replies, &normalized)
                .or_else(|| RpcCampaign::settled_unfunded_fee(case, &replies, &normalized))
                .map(|(cause, index)| (cause, replies[index].0.clone()))
        };
        let base_cause = |cause| Some((cause, "base".to_owned()));
        let invalid = json!({"id":1,"error":{"code":-32602,"message":"invalid params"}});
        let ok = json!({"id":1,"result":"0x1"});
        let hash = format!("0x{}", "ab".repeat(32));
        let by_hash = RpcCase::new("state", "eth_getBalance", json!(["0x01", hash]));
        assert_eq!(
            classify(&by_hash, invalid.clone(), ok.clone(), ok.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_BLOCK_PARAM)
        );
        // Not while needlefish sides with base (a needlefish finding), nor for a block number.
        assert_eq!(classify(&by_hash, invalid.clone(), invalid.clone(), ok.clone()), None);
        let by_number = RpcCase::new("state", "eth_getBalance", json!(["0x01", "0x10"]));
        assert_eq!(classify(&by_number, invalid.clone(), ok.clone(), ok.clone()), None);
        // Deposit-typed calls and settled items: known when needlefish answers like Nethermind.
        let deposit = RpcCase::new("call", "eth_call", json!([{"type": "0x7e"}, "latest"]));
        assert_eq!(
            classify(&deposit, ok.clone(), invalid.clone(), invalid.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_DEPOSIT_CALL_PENDING_TRACE)
        );
        let fee = RpcCase::new("fees", "eth_feeHistory", json!(["0x1", "latest"]));
        assert_eq!(
            classify(&fee, ok.clone(), invalid.clone(), invalid),
            base_cause(RpcCampaign::KNOWN_BASE_SETTLED)
        );
        let js = RpcCase::new("trace", "debug_traceTransaction", json!(["0x01", {"tracer": "x"}]));
        let internal = json!({"id":1,"error":{"code":-32603,"message":"JS Tracer is not enabled"}});
        assert_eq!(
            classify(&js, internal.clone(), ok.clone(), ok.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_SETTLED)
        );
        let named = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "callTracer"}]),
        );
        assert_eq!(classify(&named, internal, ok.clone(), ok), None);
        // A tip without a fee cap and access-list gas: known when needlefish matches Nethermind.
        let tip = RpcCase::new("call", "eth_estimateGas", json!([{"maxPriorityFeePerGas": "0x1"}]));
        let funds = json!({"id":1,"error":{"code":-32003,"message":"insufficient funds"}});
        let above = json!({"id":1,"error":{"code":-32000,"message":"failed with 50000000 gas: max priority fee per gas higher than max fee per gas"}});
        assert_eq!(
            classify(&tip, funds.clone(), above.clone(), above.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_TIP_WITHOUT_CAP)
        );
        assert_eq!(classify(&tip, funds.clone(), funds, above), None);
        let list = RpcCase::new("call", "eth_createAccessList", json!([{}]));
        let gas = |g: &str| json!({"id":1,"result":{"accessList":[],"gasUsed":g}});
        assert_eq!(
            classify(&list, gas("0x655c"), gas("0x6df4"), gas("0x6df4")),
            base_cause(RpcCampaign::KNOWN_BASE_ACCESS_LIST_GAS)
        );
        // A time override that moves the call before Prague (B12).
        let early = RpcCase::new(
            "call",
            "eth_call",
            json!([{"gas": "0x5e00"}, "latest", {}, {"time": "0x0"}]),
        );
        let floor = json!({"id":1,"error":{"code":-32000,"message":"intrinsic gas too low"}});
        let runs = json!({"id":1,"result":"0x"});
        assert_eq!(
            classify(&early, floor.clone(), runs.clone(), runs.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_OVERRIDE_TIME_RULES)
        );
        let late = RpcCase::new("call", "eth_call", json!([{"gas": "0x5e00"}, "latest"]));
        assert_eq!(classify(&late, floor, runs.clone(), runs), None);
        // A state override on a precompile's address.
        let precompile = RpcCase::new(
            "trace",
            "debug_traceCall",
            json!([{"to": "0x0000000000000000000000000000000000000011"}, "latest",
                {"stateOverrides": {"0x0000000000000000000000000000000000000011": {"code": "0x00"}}}]),
        );
        let failed = json!({"id":1,"result":{"gasUsed":"0x2faf080","error":"precompile failed"}});
        let ran = json!({"id":1,"result":{"gasUsed":"0xa962"}});
        assert_eq!(
            classify(&precompile, failed.clone(), ran.clone(), ran.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_PRECOMPILE_OVERRIDE)
        );
        let ordinary = RpcCase::new(
            "trace",
            "debug_traceCall",
            json!([{"to": "0x0000000000000000000000000000000000000012"}, "latest",
                {"stateOverrides": {"0x0000000000000000000000000000000000000012": {"code": "0x00"}}}]),
        );
        assert_eq!(classify(&ordinary, failed, ran.clone(), ran), None);
        // A priced traceCall from an unfunded sender: settled, Nethermind's side set aside.
        let unfunded = RpcCase::new(
            "trace",
            "debug_traceCall",
            json!([{"to": "0x00000000000000000000000000000000000000aa", "gasPrice": "0x1"}, "latest"]),
        );
        let low = json!({"id":1,"error":{"code":-32000,"message":"intrinsic gas too low"}});
        let trace = json!({"id":1,"result":{"gas":21000,"failed":false}});
        assert_eq!(
            classify(&unfunded, low.clone(), low.clone(), trace.clone()),
            Some((RpcCampaign::SETTLED_UNFUNDED_FEE, "nethermind".to_owned()))
        );
        let free = RpcCase::new(
            "trace",
            "debug_traceCall",
            json!([{"to": "0x00000000000000000000000000000000000000aa"}, "latest"]),
        );
        assert_eq!(classify(&free, low.clone(), low, trace), None);
        // The list itself differs too when a later run touches more (or the runs run out of gas).
        let listed = json!({"id":1,"result":{"accessList":[{"address":"0x01","storageKeys":[]}],"gasUsed":"0x7e34"}});
        assert_eq!(
            classify(&list, gas("0x7e34"), listed.clone(), listed.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_ACCESS_LIST_GAS)
        );
        let converge =
            json!({"id":1,"error":{"code":-32000,"message":"access list did not converge"}});
        assert_eq!(classify(&list, converge, listed.clone(), listed), None);
        // Batch replies are JSON arrays: classified like any other difference, without panicking.
        let batch = |g: &str| json!([{"id":1,"result":{"gasUsed":g}}]);
        assert_eq!(classify(&list, batch("0x1"), batch("0x2"), batch("0x2")), None);
        // Simulated header roots, and base's legacy `v` once needlefish's transactions match
        // Nethermind's, are compared without the derived fields; another field is not.
        let simulate = RpcCase::new("call", "eth_simulateV1", json!([{"blockStateCalls": []}]));
        let block = |root: &str, hash: &str, tx: &str, gas: &str| {
            json!({"id":1,"result":[{"hash": hash, "stateRoot": root, "gasUsed": gas,
                "transactions": [tx], "calls":[{"logs":[{"blockHash": hash, "transactionHash": tx}]}]}]})
        };
        let header = |c, b, f, n| classify(&simulate, b, f, n).map(|(cause, _)| cause) == c;
        let known = Some(RpcCampaign::KNOWN_SIMULATE_HEADER);
        assert!(header(None, batch("0x1"), batch("0x2"), batch("0x2")));
        // Header roots only.
        let base_block = block("0x00", "0xb1", "0x01", "0x5208");
        assert!(header(
            known,
            base_block.clone(),
            base_block.clone(),
            block("0x83", "0xc2", "0x01", "0x5208")
        ));
        // base's legacy v after needlefish aligned: known; before it aligned: a finding.
        let aligned = block("0x00", "0xf1", "0x02", "0x5208");
        let nm = block("0x83", "0xc2", "0x02", "0x5208");
        assert!(header(known, base_block.clone(), aligned, nm.clone()));
        assert!(header(None, base_block.clone(), base_block.clone(), nm));
        // Another field differs.
        assert!(header(
            None,
            base_block.clone(),
            base_block,
            block("0x83", "0xc2", "0x01", "0x5209")
        ));
        // Prestate: Nethermind's extra accounts and codeHash missing on base are known; another
        // value is not.
        let trace = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "prestateTracer"}]),
        );
        let a = "0x00000000000000000000000000000000000000aa";
        let b = "0x00000000000000000000000000000000000000bb";
        let base = json!({"id":1,"result":{a:{"balance":"0x1"}}});
        let full =
            json!({"id":1,"result":{a:{"balance":"0x1","codeHash":"0x00"},b:{"balance":"0x2"}}});
        let other = json!({"id":1,"result":{a:{"balance":"0x2"}}});
        assert_eq!(
            classify(&trace, base, full.clone(), full.clone()),
            base_cause(RpcCampaign::KNOWN_BASE_PRESTATE)
        );
        assert_eq!(classify(&trace, other, full.clone(), full.clone()), None);
        // A slot only base lists (its SSTORE ran out of gas) is known too.
        let slot = format!("0x{}", "00".repeat(31) + "bb");
        let with_slot = json!({"id":1,"result":{a:{"balance":"0x1","storage":{&slot:"0x00"}}}});
        let without = json!({"id":1,"result":{a:{"balance":"0x1","storage":{}}}});
        assert_eq!(
            classify(&trace, with_slot, without.clone(), without),
            base_cause(RpcCampaign::KNOWN_BASE_PRESTATE)
        );
        // Accounts only base lists (zero-credit fee vaults, an empty diffMode sender) are known too.
        let c = "0x00000000000000000000000000000000000000cc";
        let base_more = json!({"id":1,"result":{a:{"balance":"0x1"},c:{"balance":"0x0"}}});
        assert_eq!(
            classify(&trace, base_more, full.clone(), full),
            base_cause(RpcCampaign::KNOWN_BASE_PRESTATE)
        );
        // 4byteTracer: extra or higher counts on base are known; a lower count is not.
        let four = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "4byteTracer"}]),
        );
        let counts = |c: Value| json!({"id":1,"result":c});
        let nm = counts(json!({"0xa9059cbb-68":1}));
        assert_eq!(
            classify(&four, counts(json!({"0x00000000-33":10,"0xa9059cbb-68":1})), nm.clone(), nm),
            base_cause(RpcCampaign::KNOWN_BASE_4BYTE_PRECOMPILES)
        );
        let fewer = counts(json!({"0xa9059cbb-68":2}));
        assert_eq!(classify(&four, counts(json!({})), fewer.clone(), fewer), None);
    }

    #[test]
    fn override_order_needs_every_named_account_to_be_invalid() {
        let (a, b, c) = (
            "0x00000000000000000000000000000000000000aa",
            "0x00000000000000000000000000000000000000bb",
            "0x00000000000000000000000000000000000000cc",
        );
        let case = RpcCase::new(
            "call",
            "eth_call",
            json!([{}, "latest", {
                a: {"state": {}, "stateDiff": {}},
                b: {"state": {}, "stateDiff": {}},
                c: {"stateDiff": {}},
            }]),
        );
        let named = |account: &str| {
            Reply::Body(json!({"id":1,"error":{"code":-32602,
                "message": format!("account {account} has both 'state' and 'stateDiff'")}}))
        };
        let replies = |base: &str, tiger: &str| {
            vec![("base".to_owned(), named(base)), ("tiger".to_owned(), named(tiger))]
        };
        assert_eq!(
            RpcCampaign::override_order(&case, &replies(a, b)),
            Some((RpcCampaign::KNOWN_BASE_OVERRIDE_ORDER, 0))
        );
        // Tiger naming an account that sets only `stateDiff` is a real difference.
        assert_eq!(RpcCampaign::override_order(&case, &replies(a, c)), None);
        // The same account named by both is no disagreement to explain.
        assert_eq!(RpcCampaign::override_order(&case, &replies(a, a)), None);
        // In a batch, the differing reply must be explained and the others identical.
        let item = |id: u64, reply: &Reply| {
            let Reply::Body(body) = reply else { unreachable!() };
            let mut body = body.clone();
            body["id"] = json!(id);
            body
        };
        let request: Value = serde_json::from_str(&case.body).unwrap();
        let mut second = request.clone();
        second["id"] = json!(2);
        let batch = RpcCase { body: json!([request, second]).to_string(), ..case };
        let ok = json!({"id":2,"result":"0x"});
        let batch_replies = |base: &str, tiger: &str, tiger_second: Value| {
            vec![
                ("base".to_owned(), Reply::Body(json!([item(1, &named(base)), ok]))),
                ("tiger".to_owned(), Reply::Body(json!([item(1, &named(tiger)), tiger_second]))),
            ]
        };
        assert!(RpcCampaign::override_order(&batch, &batch_replies(a, b, ok.clone())).is_some());
        let other = json!({"id":2,"result":"0x01"});
        assert_eq!(RpcCampaign::override_order(&batch, &batch_replies(a, b, other)), None);
    }

    #[test]
    fn precompile_delegation_needs_delegation_code_on_a_precompile() {
        let ok = vec![
            ("base".to_owned(), json!({"id":1,"result":"0x1234"})),
            ("tiger".to_owned(), json!({"id":1,"result":"0x2a"})),
        ];
        let same = vec![ok[0].clone(), ("tiger".to_owned(), ok[0].1.clone())];
        let delegation = format!("0xef0100{}", "aa".repeat(20));
        let case = |address: &str, code: &str| {
            RpcCase::new(
                "call",
                "eth_call",
                json!([{"to": address}, "latest", {address: {"code": code}}]),
            )
        };
        let identity = "0x0000000000000000000000000000000000000004";
        assert_eq!(
            RpcCampaign::precompile_delegation(&case(identity, &delegation), &ok),
            Some((RpcCampaign::ACCEPTED_PRECOMPILE_DELEGATION, 0))
        );
        let p256 = "0x0000000000000000000000000000000000000100";
        assert!(RpcCampaign::precompile_delegation(&case(p256, &delegation), &ok).is_some());
        // Agreeing nodes need no explanation.
        assert_eq!(RpcCampaign::precompile_delegation(&case(identity, &delegation), &same), None);
        // Not accepted: delegation on an ordinary account, other code on a precompile, or an
        // address past the precompiles.
        let plain = "0x00000000000000000000000000000000000000aa";
        assert_eq!(RpcCampaign::precompile_delegation(&case(plain, &delegation), &ok), None);
        assert_eq!(RpcCampaign::precompile_delegation(&case(identity, "0x602a"), &ok), None);
        let past = "0x0000000000000000000000000000000000000012";
        assert_eq!(RpcCampaign::precompile_delegation(&case(past, &delegation), &ok), None);
        let high = "0x1000000000000000000000000000000000000004";
        assert_eq!(RpcCampaign::precompile_delegation(&case(high, &delegation), &ok), None);
    }
}
