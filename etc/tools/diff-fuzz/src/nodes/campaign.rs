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

    /// Compares the replies to one case and records the outcome.
    ///
    /// `serving` are the nodes that answered, in `replies` order; with them, a disagreement is
    /// checked and shrunk. The request is sent to the reference again, and if its two answers
    /// differ the answer is not deterministic (such as a `pending` block's random `prevRandao`)
    /// and the case is counted in `nondeterministic` instead of becoming a finding. The first
    /// case of a new signature is minimized (see [`Self::minimize`]).
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
            .map(|(name, reply)| (name.clone(), RpcCompare::normalize(&case.method, reply)))
            .collect();
        let mut replies = replies.to_vec();
        let mut serving = serving.to_vec();
        let known = Self::known_base_cause(case, &replies, &normalized)
            .map(|cause| (cause, 0))
            .or_else(|| Self::known_nethermind_cause(case, &replies, &normalized));
        if let Some((cause, index)) = known {
            *summary.lock().expect("lock").known.entry(cause.into()).or_default() += 1;
            normalized.remove(index);
            replies.remove(index);
            if index < serving.len() {
                serving.remove(index);
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
            && RpcCompare::normalize(&case.method, &reference.send(&case.body)) != normalized[0].1
        {
            let mut s = summary.lock().expect("lock");
            s.requests += 1;
            *s.nondeterministic.entry(case.method.clone()).or_default() += 1;
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
                let (minimized, replies) =
                    Self::minimize(case, serving, &signature, Self::MINIMIZE_BUDGET);
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

    /// The known base/base cause, if any, that explains how base answered `case`
    /// (`replies` and `normalized` reference first). The cause's cases are compared on the other
    /// nodes only.
    pub fn known_base_cause(
        case: &RpcCase,
        replies: &[(String, Reply)],
        normalized: &[(String, Value)],
    ) -> Option<&'static str> {
        let ((name, reference), others) = normalized.split_first()?;
        if name != "base" || others.is_empty() {
            return None;
        }
        if Self::base_genesis_bug(case, normalized) {
            return Some(Self::KNOWN_BASE_GENESIS);
        }
        // Nethermind names the cause in its message: "too many arguments, want at most N".
        let too_many = replies.iter().any(|(name, reply)| {
            name == "nethermind"
                && matches!(reply, Reply::Body(body)
                    if body["error"]["message"].as_str().is_some_and(|m| m.starts_with("too many arguments")))
        });
        if too_many && others.iter().all(|(_, v)| v != reference) {
            return Some(Self::KNOWN_BASE_EXTRA_PARAMS);
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
            return Some(Self::KNOWN_BASE_ID);
        }
        None
    }

    /// Nethermind accepts EIP-1898-style block parameters where base wants a number or tag (a
    /// block hash, `""` or `null`) and answers `null` for an index of 2^64 or more.
    pub const KNOWN_NM_BLOCK_PARAM: &str = "nethermind: lenient block parameter or index";
    /// Nethermind refuses `0X`-prefixed hashes.
    pub const KNOWN_NM_UPPER_PREFIX: &str = "nethermind: 0X prefix refused";
    /// Nethermind accepts odd-length hex data, access-list entries without `storageKeys`, and
    /// authorizations without `r` or `s`.
    pub const KNOWN_NM_LENIENT_FIELDS: &str = "nethermind: lenient request fields";
    /// Nethermind checks `eth_simulateV1` calls in another order, so the first error differs.
    pub const KNOWN_NM_SIMULATE_ORDER: &str = "nethermind: simulate check order";
    /// Nethermind (with geth) answers a priced estimate from an unfunded sender with
    /// "gas required exceeds allowance (0)".
    pub const KNOWN_NM_ESTIMATE_ALLOWANCE: &str = "nethermind: estimate allowance (0)";
    /// Nethermind's prestate tracer keeps `codeHash` and lists authorities of invalid EIP-7702
    /// authorizations, as geth does.
    pub const KNOWN_NM_PRESTATE: &str = "nethermind: prestate codeHash and invalid authorities";

    /// Nethermind's 4byteTracer skips calls to precompiles, as geth's does; revm-inspectors'
    /// counts them.
    pub const KNOWN_NM_4BYTE_PRECOMPILES: &str = "nethermind: 4byteTracer skips precompile calls";

    /// The known Nethermind difference, if any, that explains Nethermind's answer to `case`,
    /// with Nethermind's index: base and needlefish answer alike and Nethermind otherwise.
    /// Nethermind's case is then compared on the other nodes only.
    pub fn known_nethermind_cause(
        case: &RpcCase,
        replies: &[(String, Reply)],
        normalized: &[(String, Value)],
    ) -> Option<(&'static str, usize)> {
        let get = |name: &str| normalized.iter().position(|(n, _)| n == name);
        let (base, needlefish, nm) = (get("base")?, get("needlefish")?, get("nethermind")?);
        let (reference, theirs) = (&normalized[base].1, &normalized[nm].1);
        if reference != &normalized[needlefish].1 || reference == theirs {
            return None;
        }
        let code = |v: &Value| v["error"]["code"].as_i64();
        let ok = |v: &Value| v.get("result").is_some();
        let message = match &replies[nm].1 {
            Reply::Body(body) => body["error"]["message"].as_str().unwrap_or_default().to_owned(),
            Reply::Transport(_) => return None,
        };
        let body: Value = serde_json::from_str(&case.body).ok()?;
        let params = body.get("params").and_then(Value::as_array);
        let strings = Self::strings(&body["params"]);
        let cause = if ok(theirs) && code(reference) == Some(-32602) {
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
                Self::KNOWN_NM_LENIENT_FIELDS
            } else if block_like || big_index {
                Self::KNOWN_NM_BLOCK_PARAM
            } else {
                return None;
            }
        } else if code(theirs) == Some(-32602) && ok(reference) && case.body.contains("\"0X") {
            Self::KNOWN_NM_UPPER_PREFIX
        } else if case.method == "eth_simulateV1"
            && code(theirs).is_some()
            && code(reference).is_some()
        {
            Self::KNOWN_NM_SIMULATE_ORDER
        } else if case.method == "eth_estimateGas"
            && message.starts_with("gas required exceeds allowance (0)")
        {
            Self::KNOWN_NM_ESTIMATE_ALLOWANCE
        } else if case.body.contains("4byteTracer") && Self::counts_within(theirs, reference) {
            Self::KNOWN_NM_4BYTE_PRECOMPILES
        } else if case.body.contains("prestateTracer")
            && Self::without_extra_accounts(theirs, reference) == *reference
        {
            Self::KNOWN_NM_PRESTATE
        } else {
            return None;
        };
        Some((cause, nm))
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
    fn any_object(value: &Value, test: &dyn Fn(&serde_json::Map<String, Value>) -> bool) -> bool {
        match value {
            Value::Object(object) => {
                test(object) || object.values().any(|v| Self::any_object(v, test))
            }
            Value::Array(items) => items.iter().any(|v| Self::any_object(v, test)),
            _ => false,
        }
    }

    /// `value` without the `codeHash` fields and the account (address-keyed) entries that
    /// `reference` lacks at the same place.
    fn without_extra_accounts(value: &Value, reference: &Value) -> Value {
        match (value, reference) {
            (Value::Object(object), Value::Object(theirs)) => Value::Object(
                object
                    .iter()
                    .filter(|(key, _)| {
                        theirs.contains_key(*key)
                            || !(key.as_str() == "codeHash"
                                || (key.len() == 42 && RpcCompare::is_hex(key)))
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
        let Some(((name, reference), others)) = normalized.split_first() else { return false };
        let failed = |v: &Value| v.get("error").is_some() || v.get("result") == Some(&Value::Null);
        name == "base"
            && !others.is_empty()
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
                    .map(|(name, reply)| (name.clone(), RpcCompare::normalize(&case.method, reply)))
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
        let replies = |base: &Value, other: &Value| {
            vec![
                ("base".to_owned(), base.clone()),
                ("needlefish".to_owned(), other.clone()),
                ("nethermind".to_owned(), other.clone()),
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
                ("base".to_owned(), Reply::Body(base.clone())),
                ("needlefish".to_owned(), Reply::Body(invalid.clone())),
                (
                    "nethermind".to_owned(),
                    Reply::Body(json!({"id":1,"error":{"code":-32602,"message":message}})),
                ),
            ]
        };
        let known = |case: &RpcCase, base: &Value, message: &str| {
            RpcCampaign::known_base_cause(case, &raw(base, message), &replies(base, &invalid))
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
    }

    #[test]
    fn known_nethermind_causes_need_base_and_needlefish_agreeing() {
        let classify = |case: &RpcCase, base: Value, needlefish: Value, nm: Value| {
            let replies = vec![
                ("base".to_owned(), Reply::Body(base)),
                ("needlefish".to_owned(), Reply::Body(needlefish)),
                ("nethermind".to_owned(), Reply::Body(nm)),
            ];
            let normalized: Vec<(String, Value)> = replies
                .iter()
                .map(|(n, r)| (n.clone(), RpcCompare::normalize(&case.method, r)))
                .collect();
            RpcCampaign::known_nethermind_cause(case, &replies, &normalized).map(|(c, _)| c)
        };
        let invalid = json!({"id":1,"error":{"code":-32602,"message":"invalid params"}});
        let ok = json!({"id":1,"result":"0x1"});
        let hash = format!("0x{}", "ab".repeat(32));
        let by_hash = RpcCase::new("state", "eth_getBalance", json!(["0x01", hash]));
        assert_eq!(
            classify(&by_hash, invalid.clone(), invalid.clone(), ok.clone()),
            Some(RpcCampaign::KNOWN_NM_BLOCK_PARAM)
        );
        // Not when needlefish sides with Nethermind, nor for a block number.
        assert_eq!(classify(&by_hash, invalid.clone(), ok.clone(), ok.clone()), None);
        let by_number = RpcCase::new("state", "eth_getBalance", json!(["0x01", "0x10"]));
        assert_eq!(classify(&by_number, invalid.clone(), invalid, ok), None);
        // Prestate: extra accounts and codeHash only on Nethermind are known; another value is not.
        let trace = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "prestateTracer"}]),
        );
        let a = "0x00000000000000000000000000000000000000aa";
        let b = "0x00000000000000000000000000000000000000bb";
        let base = json!({"id":1,"result":{a:{"balance":"0x1"}}});
        let extra =
            json!({"id":1,"result":{a:{"balance":"0x1","codeHash":"0x00"},b:{"balance":"0x2"}}});
        let other = json!({"id":1,"result":{a:{"balance":"0x2"}}});
        assert_eq!(
            classify(&trace, base.clone(), base.clone(), extra),
            Some(RpcCampaign::KNOWN_NM_PRESTATE)
        );
        assert_eq!(classify(&trace, base.clone(), base, other), None);
        // 4byteTracer: fewer or missing counts on Nethermind are known; a higher count is not.
        let four = RpcCase::new(
            "trace",
            "debug_traceTransaction",
            json!(["0x01", {"tracer": "4byteTracer"}]),
        );
        let counts = |c: Value| json!({"id":1,"result":c});
        let base = counts(json!({"0x00000000-33":10,"0xa9059cbb-68":1}));
        assert_eq!(
            classify(&four, base.clone(), base.clone(), counts(json!({"0xa9059cbb-68":1}))),
            Some(RpcCampaign::KNOWN_NM_4BYTE_PRECOMPILES)
        );
        assert_eq!(classify(&four, base.clone(), base, counts(json!({"0xa9059cbb-68":2}))), None);
    }
}
