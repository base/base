//! The coverage-guided state campaign: inputs of chain operations run on real nodes, kept when
//! they reach new trie, state, database, or persistence edges in any node.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    Corpus, MutationStats, Rng,
    nodes::{Divergence, GuideSample, StateInput, StateMutator, StateOp, StateSession},
};

/// Settings of a state campaign.
#[derive(Debug, Clone)]
pub struct StateConfig {
    /// Seed of the inputs (the chain seed is the session's).
    pub seed: u64,
    /// How long to run.
    pub duration: Duration,
    /// Output directory.
    pub out: PathBuf,
    /// Nodes whose coverage guides the run.
    pub coverage_from: Vec<String>,
    /// Mutate corpus inputs (`true`) or only run fresh ones while still measuring coverage.
    pub guided: bool,
    /// Percentage of guided inputs that are fresh.
    pub explore_percent: u64,
    /// Canonical height after which the nodes are wiped and the chain restarts from genesis.
    pub max_blocks: u64,
    /// Replays one minimization may run (0 disables minimization).
    pub minimize_budget: usize,
}

/// One divergence signature and its first example.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateFinding {
    /// The signature.
    pub signature: String,
    /// Times it was seen.
    pub count: u64,
    /// The first divergence.
    pub first: Divergence,
    /// Operations since the wipe that led to it (minimized when minimization ran).
    pub ops: usize,
    /// Reproducer directory.
    pub reproducer: PathBuf,
}

/// The `state-summary.json` report.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateSummary {
    /// `guided` or `unguided`.
    pub mode: String,
    /// Seconds run.
    pub elapsed_secs: u64,
    /// Inputs run.
    pub inputs: u64,
    /// Operations run, by name.
    pub ops: BTreeMap<String, u64>,
    /// Blocks built.
    pub blocks: u64,
    /// Times the nodes were wiped and the chain restarted from genesis.
    pub wipes: u64,
    /// Harness failures (the adapter could not build a block, and similar), by message.
    pub harness_errors: BTreeMap<String, u64>,
    /// Divergences by signature.
    pub findings: BTreeMap<String, StateFinding>,
    /// Counted events that are not divergences.
    pub observations: BTreeMap<String, u64>,
    /// Distinct features (node edge and hit-count bucket).
    pub features: u64,
    /// Distinct edges per node.
    pub edges: BTreeMap<String, u64>,
    /// Corpus size.
    pub corpus: u64,
    /// Inputs by origin (`fresh`, `mutant`, `saved`), tried and kept.
    pub origins: BTreeMap<String, MutationStats>,
    /// Per mutation operator, children tried and kept.
    pub mutations: BTreeMap<String, MutationStats>,
    /// Coverage over time.
    pub timeline: Vec<GuideSample>,
}

/// Runs the state campaign.
#[derive(Debug)]
pub struct StateCampaign;

impl StateCampaign {
    /// Time given to background persistence before an input's coverage is read.
    pub const SETTLE: Duration = Duration::from_millis(300);

    /// Runs until `config.duration` has passed; writes `state-summary.json` and the corpus
    /// (`state-corpus/`, guided runs only) under `config.out`.
    pub fn run(session: &mut StateSession, config: &StateConfig) -> eyre::Result<StateSummary> {
        fs::create_dir_all(&config.out)?;
        let started = Instant::now();
        let corpus_dir = config.guided.then(|| config.out.join("state-corpus"));
        let (mut corpus, saved) = Corpus::<StateInput>::open(corpus_dir)?;
        let mut queue: VecDeque<StateInput> = saved.into();
        let mut rng = Rng::new(&[config.seed, 0x57a7_ec00]);
        let max_depth = session.max_depth.min(u64::from(u8::MAX)) as u8;
        let mut mutator = StateMutator { rng: Rng::new(&[config.seed, 0x57a7_ec01]), max_depth };
        let mut summary = StateSummary {
            mode: if config.guided { "guided" } else { "unguided" }.to_owned(),
            ..Default::default()
        };
        for node in &mut session.nodes {
            if config.coverage_from.contains(&node.name) {
                node.enable_coverage_sink();
            }
        }
        for node in &session.nodes {
            node.reset_coverage();
        }
        let mut last_report = Instant::now();
        while started.elapsed() < config.duration {
            if session.height() >= config.max_blocks {
                session.reset()?;
                summary.wipes += 1;
            }
            let (input, origin, operators) = if let Some(input) = queue.pop_front() {
                (input, "saved", Vec::new())
            } else if !config.guided
                || corpus.entries.is_empty()
                || rng.chance(config.explore_percent)
            {
                (StateInput::random(&mut rng, max_depth), "fresh", Vec::new())
            } else {
                let parent = corpus.pick(&mut rng).expect("corpus is not empty");
                let parent = corpus.entries[parent].case.clone();
                let donor = corpus.entries[rng.index(corpus.entries.len())].case.clone();
                let (child, operators) = mutator.mutate(&parent, Some(&donor));
                (child, "mutant", operators)
            };
            for node in &session.nodes {
                let _ = node.take_coverage();
            }
            let blocks = summary.blocks;
            let divergences = Self::execute(session, &input, &mut rng, &mut summary);
            thread::sleep(Self::SETTLE);
            // Hit counts per block built: summed over a whole input they grow with its length
            // (and with background persistence work), so nearly every input would reach a new
            // hit-count bucket and be kept.
            let per = (summary.blocks - blocks).max(1);
            let mut coverage = BTreeMap::new();
            for node in &session.nodes {
                if let Some(edges) = node.take_coverage() {
                    for (edge, count) in edges {
                        coverage.insert(format!("{}:{edge}", node.name), count.div_ceil(per));
                    }
                }
            }
            let (features, novel) = corpus.observe(&coverage);
            let kept = config.guided && novel > 0;
            if kept && let Err(error) = corpus.add(input.clone(), features) {
                eprintln!("saving corpus input: {error:#}");
            }
            summary.inputs += 1;
            let entry = summary.origins.entry(origin.to_owned()).or_default();
            entry.tried += 1;
            entry.kept += u64::from(kept);
            for operator in operators {
                let entry = summary.mutations.entry(operator.to_owned()).or_default();
                entry.tried += 1;
                entry.kept += u64::from(kept);
            }
            match divergences {
                Ok(divergences) if !divergences.is_empty() => {
                    // Transient reads leave the nodes agreeing again: the chain carries on.
                    let lasting = divergences.iter().any(|d| !d.transient());
                    Self::record(session, config, &mut summary, divergences)?;
                    if lasting {
                        session.reset()?;
                        summary.wipes += 1;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let message = format!("{error:#}");
                    eprintln!("harness error: {message}");
                    let key: String = message.chars().take(160).collect();
                    *summary.harness_errors.entry(key).or_default() += 1;
                    session.reset()?;
                    summary.wipes += 1;
                }
            }
            if last_report.elapsed() > Duration::from_secs(60) {
                last_report = Instant::now();
                Self::sample(&corpus, session, &mut summary, started);
                Self::write(config, &summary)?;
            }
        }
        Self::sample(&corpus, session, &mut summary, started);
        Self::write(config, &summary)?;
        Ok(summary)
    }

    /// Runs an input's operations, each followed by a read of the head's state, and a deeper
    /// read at the end. Stops at the first operation with lasting divergences.
    fn execute(
        session: &mut StateSession,
        input: &StateInput,
        rng: &mut Rng,
        summary: &mut StateSummary,
    ) -> eyre::Result<Vec<Divergence>> {
        let built = session.blocks_built;
        let result: eyre::Result<Vec<Divergence>> = (|| {
            let mut transient = Vec::new();
            for op in &input.ops {
                *summary.ops.entry(op.name().to_owned()).or_default() += 1;
                let mut divergences = session.run(op)?;
                if divergences.is_empty() && !matches!(op, StateOp::Check { .. }) {
                    divergences = session.check_recovering(1, rng.next_u64());
                }
                if divergences.iter().any(|d| !d.transient()) {
                    return Ok(divergences);
                }
                transient.extend(divergences);
            }
            transient.extend(session.check_recovering(3, rng.next_u64()));
            Ok(transient)
        })();
        summary.blocks += session.blocks_built - built;
        let mut divergences = result?;
        // An injected fault can crash a node that persists in the background after the last
        // operation looked: recover it and check it again, as after any injected crash.
        let crashed: Vec<String> = session
            .procs
            .iter_mut()
            .filter_map(|proc| proc.injected_crash().map(|_| proc.name.clone()))
            .collect();
        if !crashed.is_empty() {
            // What the crashed node answered while it was going down does not count.
            divergences.retain(|d| !crashed.contains(&d.node));
            if let Some(recovered) = session.recover_faults() {
                divergences.extend(recovered);
            }
            if divergences.is_empty() {
                divergences = session.check(3, rng.next_u64());
            }
        }
        // A node that died explains whatever else failed.
        for (index, proc) in session.procs.iter_mut().enumerate() {
            if !proc.alive() {
                let name = session.nodes[index].name.clone();
                let log = proc.root.join(format!("{name}.log"));
                let tail = fs::read_to_string(&log)
                    .map(|text| text.lines().rev().take(40).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                divergences.retain(|d| d.node != name);
                divergences.push(Divergence {
                    kind: "crash".to_owned(),
                    node: name,
                    detail: "process exited".to_owned(),
                    block: Some(session.tree.head().number),
                    expected: serde_json::json!("running"),
                    got: serde_json::json!(tail),
                });
            }
        }
        Ok(divergences)
    }

    /// Records divergences; a new signature gets a reproducer, minimized when enabled.
    fn record(
        session: &mut StateSession,
        config: &StateConfig,
        summary: &mut StateSummary,
        divergences: Vec<Divergence>,
    ) -> eyre::Result<()> {
        for divergence in &divergences {
            eprintln!(
                "divergence: {} at block {:?}: expected {} got {}",
                divergence.signature(),
                divergence.block,
                divergence.expected,
                divergence.got
            );
        }
        let new: Vec<&Divergence> =
            divergences.iter().filter(|d| !summary.findings.contains_key(&d.signature())).collect();
        for divergence in &divergences {
            if let Some(finding) = summary.findings.get_mut(&divergence.signature()) {
                finding.count += 1;
            }
        }
        let Some(first) = new.first().map(|d| (*d).clone()) else {
            return Ok(());
        };
        let slug: String = first
            .signature()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = config.out.join("findings").join(format!("{slug}-{}", summary.findings.len()));
        session.save_reproducer(&dir.join("original"), &divergences)?;
        let mut ops = session.history.clone();
        if config.minimize_budget > 0 && !first.transient() {
            session.focus(&Self::replay_nodes(session, &divergences));
            let minimized =
                Self::minimize(session, &ops, &first.signature(), config.minimize_budget)
                    .and_then(|ops| Ok((session.replay(&ops)?, ops)));
            if let Ok((divergences, minimized)) = &minimized {
                session.save_reproducer(&dir.join("minimized"), divergences)?;
                ops.clone_from(minimized);
            }
            session.unfocus();
            minimized?;
        }
        for divergence in new {
            summary.findings.insert(
                divergence.signature(),
                StateFinding {
                    signature: divergence.signature(),
                    count: 1,
                    first: divergence.clone(),
                    ops: ops.len(),
                    reproducer: dir.clone(),
                },
            );
        }
        Ok(())
    }

    /// The nodes a minimization replays on: those that diverged, plus one other that starts
    /// fast (needlefish, else base/base) so the divergence still shows against an agreeing node.
    fn replay_nodes(session: &StateSession, divergences: &[Divergence]) -> Vec<String> {
        let mut keep: Vec<String> = divergences.iter().map(|d| d.node.clone()).collect();
        if keep.iter().any(|n| n == "all") {
            return session.names.clone();
        }
        let partner = ["needlefish", "base"]
            .into_iter()
            .chain(session.names.iter().map(String::as_str))
            .find(|n| session.names.iter().any(|m| m == n) && !keep.iter().any(|k| k == n));
        keep.extend(partner.map(str::to_owned));
        keep.sort();
        keep.dedup();
        keep
    }

    /// Shrinks `ops` while a replay from genesis still shows `signature`: drops chunks of
    /// operations (halving the chunk size), then single operations, then cuts block counts to
    /// one. At most `budget` replays.
    pub fn minimize(
        session: &mut StateSession,
        ops: &[StateOp],
        signature: &str,
        budget: usize,
    ) -> eyre::Result<Vec<StateOp>> {
        let mut best = ops.to_vec();
        let mut runs = 0;
        let reproduces = |session: &mut StateSession, candidate: &[StateOp], runs: &mut usize| {
            *runs += 1;
            session
                .replay(candidate)
                .is_ok_and(|divergences| divergences.iter().any(|d| d.signature() == signature))
        };
        let mut chunk = best.len().div_ceil(2).max(1);
        while chunk >= 1 && runs < budget {
            let mut start = 0;
            let mut shrunk = false;
            while start < best.len() && runs < budget {
                // The last operation is where the divergence showed; keep it.
                if start + chunk >= best.len() {
                    break;
                }
                let mut candidate = best.clone();
                candidate.drain(start..start + chunk);
                if reproduces(session, &candidate, &mut runs) {
                    best = candidate;
                    shrunk = true;
                } else {
                    start += chunk;
                }
            }
            if !shrunk {
                if chunk == 1 {
                    break;
                }
                chunk /= 2;
            }
        }
        for index in 0..best.len() {
            if runs >= budget {
                break;
            }
            let mut candidate = best.clone();
            match &mut candidate[index] {
                StateOp::Extend { blocks, .. } | StateOp::Fork { blocks, .. } if *blocks > 1 => {
                    *blocks = 1
                }
                _ => continue,
            }
            if reproduces(session, &candidate, &mut runs) {
                best = candidate;
            }
        }
        Ok(best)
    }

    fn sample(
        corpus: &Corpus<StateInput>,
        session: &StateSession,
        summary: &mut StateSummary,
        started: Instant,
    ) {
        summary.elapsed_secs = started.elapsed().as_secs();
        summary.features = corpus.ids.len() as u64;
        summary.corpus = corpus.entries.len() as u64;
        summary.observations = session.observations.clone();
        let mut edges: BTreeMap<String, u64> = BTreeMap::new();
        for name in &corpus.names {
            let node = name.rsplit_once(':').map_or(name.as_str(), |(node, _)| node);
            *edges.entry(node.to_owned()).or_default() += 1;
        }
        summary.edges = edges;
        let sample = GuideSample {
            secs: summary.elapsed_secs,
            requests: summary.inputs,
            features: summary.features,
            edges: corpus.names.len() as u64,
            corpus: summary.corpus,
        };
        eprintln!(
            "state: inputs {} blocks {} features {} edges {:?} corpus {} findings {}",
            summary.inputs,
            summary.blocks,
            summary.features,
            summary.edges,
            summary.corpus,
            summary.findings.len()
        );
        summary.timeline.push(sample);
    }

    fn write(config: &StateConfig, summary: &StateSummary) -> eyre::Result<()> {
        let tmp = config.out.join("state-summary.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(summary)?)?;
        fs::rename(tmp, config.out.join("state-summary.json"))?;
        Ok(())
    }
}
