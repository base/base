//! Concrete commonware simplex type choices.
//!
//! This module (compiled only under the `simplex` feature) pins the concrete
//! types for the simplex `Engine`'s cryptographic generics. It carries no live
//! consensus logic yet — subsequent Phase-2 units add the `Automaton` / `Relay` /
//! `Reporter` / `Elector` glue and drive the `Engine`.
//!
//! **Digest.** commonware's `Digest` is a foreign trait with heavy supertrait
//! bounds (`Array + Copy + Random`, transitively `Codec`/`Ord`/`Display`/
//! `Deref<[u8]>`), which Base's `PayloadHash` (an alloy `B256` newtype) does not
//! implement. Rather than implement the foreign trait on a foreign-ish type, we
//! reuse commonware's ready-made 32-byte digest, `sha256::Digest`. Its name is
//! cosmetic — structurally it is a `[u8; 32]` wrapper with `From<[u8; 32]>` — so
//! the decided value (a `PayloadHash`, per DESIGN.md "What does consensus
//! decide?") maps into it losslessly via [`SimplexDigest::from`].
//!
//! **Scheme.** simplex's `S: Scheme<D>` is the simplex-specific marker trait
//! `commonware_consensus::simplex::scheme::Scheme` (a blanket impl over
//! `commonware_cryptography::certificate::Scheme`), **not** a bare cryptography
//! `Scheme`. For the small fixed sequencer set we use the ed25519 certificate
//! scheme; bls12381 / `threshold_simplex` remain a Phase-2 revisit for succinct
//! certificates at larger `n` (see the fault-tolerance design question).

use std::time::Duration;

use commonware_actor::Feedback;
use commonware_consensus::{
    Automaton, CertifiableAutomaton, Relay, Reporter, Viewable,
    simplex::{
        Plan,
        config::{Floor, ForwardingPolicy},
        elector::{Config as ElectorConfig, Elector, Terms},
        types::Context,
    },
    types::{Epoch, Participant, Round, TermLength, ViewDelta},
};
use commonware_cryptography::certificate::Verifier;
use commonware_p2p::Blocker;
use commonware_runtime::buffer::paged::CacheRef;
use commonware_utils::{NZU16, NZU32, NZUsize, channel::oneshot, ordered::Set};
use tokio::sync::watch;

use super::ConsensusStatus;

/// Concrete digest type for the simplex `Engine` (`D: Digest`).
///
/// A 32-byte digest wrapper. The decided L2 block's `PayloadHash` (32 bytes) is
/// converted into this via `SimplexDigest::from(payload_hash.0)`.
pub type SimplexDigest = commonware_cryptography::sha256::Digest;

/// Concrete signature scheme for the simplex `Engine` (`S: Scheme<D>`).
///
/// The ed25519 certificate scheme, keyed over [`SimplexDigest`].
pub type SimplexScheme = commonware_consensus::simplex::scheme::ed25519::Scheme;

/// The public-key type of [`SimplexScheme`], projected via the scheme's
/// `Verifier` associated type so it tracks the scheme choice rather than
/// hardcoding `ed25519::PublicKey`.
pub type SimplexPublicKey = <SimplexScheme as Verifier>::PublicKey;

/// The certificate type of [`SimplexScheme`] (projected via `Verifier`), as seen
/// by [`Elector::elect`].
pub type SimplexCertificate = <SimplexScheme as Verifier>::Certificate;

/// The concrete consensus activity our [`StatusReporter`] receives from the
/// simplex `Engine` (`F: Reporter<Activity = Activity<S, D>>`).
pub type SimplexActivity =
    commonware_consensus::simplex::types::Activity<SimplexScheme, SimplexDigest>;

/// The parallel-execution strategy for the simplex `Engine` (`T: Strategy`).
///
/// `Sequential` (single-threaded, deterministic) is the safe default for the
/// small fixed sequencer set — no thread pool, easiest to reason about. commonware
/// also provides `Rayon`; revisit for parallel batch signature verification if the
/// voter set grows enough to warrant it.
pub type SimplexStrategy = commonware_parallel::Sequential;

/// The runtime environment for the simplex `Engine` (`E`, satisfying
/// `BufferPooler + Clock + CryptoRng + Spawner + Storage + Metrics`).
///
/// A fully-concrete, generic-free type. **Construction constraint:** a `Context`
/// value is only obtainable inside `commonware_runtime::tokio::Runner::start(|ctx|
/// ...)` (private fields, no standalone constructor). So `Engine::new(context,
/// cfg)` must run **inside** that runner closure — the actor drives the engine by
/// building a `Runner` and constructing/starting the engine within its `start`
/// closure (wired in a later unit).
///
/// `CryptoRng` is satisfied not by a direct impl but via `rand_core` 0.10.1's
/// blanket `impl CryptoRng for R: TryCryptoRng<Error = Infallible>` (`Context`
/// impls `TryRng`/`TryCryptoRng`). Both commonware crates pin `rand_core` 0.10.1,
/// so the bound holds; if those versions ever diverge, this bound breaks — a
/// coupling to watch at commonware upgrades.
pub type SimplexRuntimeContext = commonware_runtime::tokio::Context;

/// commonware-p2p channel id for simplex vote traffic (`Engine::start`'s first
/// channel pair). Channel ids are caller-chosen `u64`s; only distinctness and
/// cross-peer consistency matter.
pub const SIMPLEX_VOTE_CHANNEL: commonware_p2p::Channel = 0;
/// commonware-p2p channel id for simplex certificate traffic (second pair).
pub const SIMPLEX_CERTIFICATE_CHANNEL: commonware_p2p::Channel = 1;
/// commonware-p2p channel id for simplex resolver back-fill (third pair).
pub const SIMPLEX_RESOLVER_CHANNEL: commonware_p2p::Channel = 2;

/// The concrete p2p sender for a simplex channel, from the dedicated
/// `commonware-p2p::authenticated` transport (the approved Networking decision),
/// over our runtime env and public key.
pub type SimplexNetSender =
    commonware_p2p::authenticated::discovery::Sender<SimplexPublicKey, SimplexRuntimeContext>;
/// The concrete p2p receiver for a simplex channel (authenticated transport).
pub type SimplexNetReceiver = commonware_p2p::authenticated::discovery::Receiver<SimplexPublicKey>;

/// The fully-applied simplex `Engine` configuration for our concrete generics.
///
/// This alias binds all eight `Config<S, L, B, D, A, R, F, T>` generics to our
/// concrete types. Note `L` is the elector **config** ([`PinnedLeaderConfig`],
/// which impls `elector::Config`), not the built [`PinnedLeaderElector`] — the
/// engine calls `Config::build` internally with the participant set.
///
/// A `SimplexConfig` **value** is assembled inside the `Runner::start` closure
/// (the value-construction unit): every field is a constant, a pinned component,
/// or a genesis floor, except the page cache, which is built from the live
/// context via `CacheRef::from_pooler`. The full field list + example values are
/// captured in `docs/consensus-simplex/DESIGN.md` (In-process integration).
pub type SimplexConfig = commonware_consensus::simplex::Config<
    SimplexScheme,
    PinnedLeaderConfig,
    StubBlocker,
    SimplexDigest,
    StubAutomaton,
    StubRelay,
    StatusReporter,
    SimplexStrategy,
>;

/// Our read side: a commonware [`Reporter`] that projects consensus activity onto
/// the [`ConsensusStatus`] watch channel the sequencer coordinator samples.
///
/// `Reporter::report` is **synchronous**, takes `&mut self`, returns
/// [`Feedback`], and the trait requires `Clone + Send + 'static` — the `Engine`
/// owns the reporter and clones it, so state is shared through the cloneable
/// [`watch::Sender`].
///
/// Phase-2 scope: this updates the consensus `view` from notarization /
/// finalization certificates (the part fully derivable from `Activity`) and logs
/// the finalized payload digest. It does **not** yet populate
/// `finalized_head: Option<L2BlockInfo>` — resolving a [`SimplexDigest`] back to a
/// full `L2BlockInfo` is the `Relay`/engine's job, wired in Phase 3.
#[derive(Debug, Clone)]
pub struct StatusReporter {
    status_tx: watch::Sender<ConsensusStatus>,
}

impl StatusReporter {
    /// Creates a new status reporter over the given status watch sender.
    pub const fn new(status_tx: watch::Sender<ConsensusStatus>) -> Self {
        Self { status_tx }
    }

    /// Advances the reported consensus `view` **monotonically**: a later,
    /// possibly reordered or late-delivered, lower-view activity can never make
    /// the observable view regress. Only signals a watch change when the view
    /// strictly increases.
    ///
    /// Monotonicity matters because the sequencer coordinator samples this watch
    /// on its hot loop to answer leadership; a regressing view (last-write-wins)
    /// would feed a wrong leadership answer at the precise moment leadership is
    /// changing during failover.
    fn advance_view(&self, view: u64) {
        self.status_tx.send_if_modified(|status| {
            if view > status.view {
                status.view = view;
                true
            } else {
                false
            }
        });
    }
}

impl Reporter for StatusReporter {
    type Activity = SimplexActivity;

    fn report(&mut self, activity: Self::Activity) -> Feedback {
        match activity {
            SimplexActivity::Finalization(finalization) => {
                let view = finalization.view().get();
                let digest = finalization.proposal.payload;
                info!(target: "simplex", view, digest = %digest, "consensus finalized payload");
                self.advance_view(view);
            }
            // A view is *entered* on exactly one 2f+1 event: a **certified**
            // notarization (commonware advances the view iff the application
            // certifies the notarization) or a **nullification** certificate (the
            // leader-timeout skip — this IS the failover primitive, so it must be
            // observed or the view goes stale exactly during failover). A bare
            // `Notarization` is a formed certificate that has NOT yet advanced the
            // view, so it does not move the reported view here (advancing on it
            // would briefly mis-report an uncertified-then-nullified view).
            SimplexActivity::Certification(notarization) => {
                self.advance_view(notarization.view().get());
            }
            SimplexActivity::Nullification(nullification) => {
                self.advance_view(nullification.view().get());
            }
            // Per-validator votes, bare notarizations, and fault evidence do not
            // move the observable status in Phase 2. Fault variants
            // (ConflictingNotarize/Finalize, NullifyFinalize) feed equivocation
            // attribution in a later unit (see the review inbox item).
            _ => {}
        }
        Feedback::Ok
    }
}

/// Our dissemination side: a commonware [`Relay`] that broadcasts the body
/// behind a decided digest to peers.
///
/// simplex itself moves only digests + votes; the full block body travels via
/// this `Relay` (see DESIGN.md "What does consensus decide?"). `Relay::broadcast`
/// is **synchronous** and returns [`Feedback`]; the trait requires
/// `Clone + Send + 'static`.
///
/// Phase-2 scope: a compiling stub that acknowledges (`Feedback::Ok`) without
/// yet shipping bodies. The real implementation — mapping a [`SimplexDigest`]
/// back to its `BaseExecutionPayloadEnvelope` and pushing it over the dedicated
/// commonware-p2p transport — is wired when the `Engine` is constructed (Phase 2)
/// and the digest↔body store exists (Phase 3).
#[derive(Debug, Clone, Default)]
pub struct StubRelay;

impl Relay for StubRelay {
    type Digest = SimplexDigest;
    type PublicKey = SimplexPublicKey;
    type Plan = Plan<SimplexPublicKey>;

    fn broadcast(&mut self, _payload: Self::Digest, _plan: Self::Plan) -> Feedback {
        Feedback::Ok
    }
}

/// Our block-production / verification side: a commonware [`CertifiableAutomaton`]
/// (supertrait [`Automaton`]).
///
/// `propose` is asked for a payload when we lead a view; `verify` validates a
/// peer's proposal; `certify` gates a *notarized* payload into finalization and
/// **must be deterministic across honest nodes** (it depends only on the digest +
/// deterministically-derivable data — never local timing/availability).
///
/// Phase-2 scope: a compiling, deliberately **inert** stub. `propose` and
/// `verify` return a channel whose sender is dropped — no payload proposed, no
/// verdict rendered — and `certify` keeps commonware's deterministic
/// always-true default. This stub is **never wired to a live authoritative
/// `Engine`** in Phase 2. Real block production (`propose` → the sequencer's
/// built `BaseExecutionPayloadEnvelope`), payload validation (`verify` against the
/// engine), and a real deterministic `certify` land in Phase 3.
#[derive(Debug, Clone, Default)]
pub struct StubAutomaton;

impl Automaton for StubAutomaton {
    type Context = Context<SimplexDigest, SimplexPublicKey>;
    type Digest = SimplexDigest;

    async fn propose(&mut self, _context: Self::Context) -> oneshot::Receiver<Self::Digest> {
        // Inert stub: drop the sender so no payload is proposed (a valid "cannot
        // generate a payload" per the trait contract). Real proposal is Phase 3.
        let (_tx, rx) = oneshot::channel();
        rx
    }

    async fn verify(
        &mut self,
        _context: Self::Context,
        _payload: Self::Digest,
    ) -> oneshot::Receiver<bool> {
        // Inert stub: drop the sender so no verdict is rendered — deliberately
        // never affirms validity. Real verification against the engine is Phase 3;
        // this stub must not be wired to a live authoritative Engine.
        let (_tx, rx) = oneshot::channel();
        rx
    }
}

// `certify` keeps commonware's deterministic always-true default; a real
// deterministic certify (gating notarized payloads into finalization) is Phase 3.
impl CertifiableAutomaton for StubAutomaton {}

/// Leader-election config that pins leadership to a **subset** of the voter set.
///
/// This realizes the core fault-tolerance decision (see DESIGN.md
/// "Fault-tolerance target & voter/leader decoupling"): all `n` participants
/// vote, but only the sequencer subset is ever elected leader. commonware's
/// built-in `RoundRobin`/`Random` electors spread leadership across **all**
/// participants, which is not what we want.
///
/// `leader_indices` are indices into the ordered participant (voter) set. An
/// **empty** set — the `Default` required by the `Config` trait — degrades to
/// "all participants eligible"; production must set the pinned sequencer indices.
/// Determinism (required by `Elector`) holds: election is a pure function of
/// `(round, leader set, term policy)`.
///
/// `terms` is the **stable-leader** policy: a pinned leader holds across a whole
/// *term* (a run of `Terms::length()` consecutive views) and rotates only at term
/// boundaries — or sooner via `stall_timeout` on leader inactivity, which is the
/// failover primitive. [`PinnedLeaderConfig::new`] uses [`Self::default_terms`]
/// (Phase-3/4 tuning placeholders); [`PinnedLeaderConfig::with_terms`] takes an
/// explicit policy (e.g. `Terms::rotating()` for one-leader-per-view).
#[derive(Clone, Debug)]
pub struct PinnedLeaderConfig {
    leader_indices: Vec<usize>,
    terms: Terms,
}

impl PinnedLeaderConfig {
    /// Creates a config pinning leadership to the given participant indices, using
    /// the [default stable-leader term policy](Self::default_terms).
    pub const fn new(leader_indices: Vec<usize>) -> Self {
        Self { leader_indices, terms: Self::default_terms() }
    }

    /// Creates a config with the given pinned indices and an explicit term policy
    /// (e.g. `Terms::rotating()` for one leader per view, or a custom stable term).
    pub const fn with_terms(leader_indices: Vec<usize>, terms: Terms) -> Self {
        Self { leader_indices, terms }
    }

    /// The default stable-leader term policy. **Phase-3/4 tuning placeholders:** a
    /// pinned leader holds for `length` consecutive views before deterministic
    /// rotation, `stall_timeout` hands off sooner if the leader stalls (the
    /// failover primitive, analogous to Raft's election timeout), and
    /// `optimistic_views` disables optimistic look-ahead for now. `stall_timeout`
    /// must exceed the Engine's `certification_timeout` (an Engine invariant, and
    /// the config builder uses 2s). Real values (cross-region Δ budget) are a later
    /// tuning task tracked in the review inbox.
    const fn default_terms() -> Terms {
        Terms::stable(TermLength::new(NZU32!(100)), Duration::from_secs(5), ViewDelta::new(0))
    }
}

impl Default for PinnedLeaderConfig {
    fn default() -> Self {
        Self { leader_indices: Vec::new(), terms: Self::default_terms() }
    }
}

impl ElectorConfig<SimplexScheme> for PinnedLeaderConfig {
    type Elector = PinnedLeaderElector;

    fn build(self, participants: &Set<SimplexPublicKey>) -> PinnedLeaderElector {
        assert!(!participants.is_empty(), "no participants");
        let n = participants.len();
        let leaders: Vec<Participant> = if self.leader_indices.is_empty() {
            (0..n).map(Participant::from_usize).collect()
        } else {
            self.leader_indices
                .iter()
                .map(|&index| {
                    assert!(index < n, "leader index {index} out of range for {n} participants");
                    Participant::from_usize(index)
                })
                .collect()
        };
        PinnedLeaderElector { leaders, terms: self.terms }
    }
}

/// Initialized elector that keeps a **stable** leader within each term, rotating
/// only among the pinned subset at term boundaries.
///
/// Created via [`PinnedLeaderConfig::build`] (called internally by consensus).
#[derive(Clone, Debug)]
pub struct PinnedLeaderElector {
    leaders: Vec<Participant>,
    terms: Terms,
}

impl Elector<SimplexScheme> for PinnedLeaderElector {
    fn terms(&self) -> Terms {
        self.terms
    }

    fn elect(&self, round: Round, _certificate: Option<&SimplexCertificate>) -> Participant {
        // Stable-leader: the leader is a pure function of the *term index* — a run
        // of `terms().length()` consecutive views — not the raw view, so one
        // pinned leader holds across a whole term and rotation happens only at term
        // boundaries (or sooner via `stall_timeout` on leader inactivity). Mirrors
        // commonware `RoundRobinElector`'s index math, but over the pinned subset.
        let term_index = round.view().term_index(self.terms.length());
        let n = u64::try_from(self.leaders.len()).expect("leader count fits in u64");
        let index = round.epoch().get().wrapping_add(term_index) % n;
        self.leaders[usize::try_from(index).expect("leader index fits in usize")]
    }
}

/// Peer-blocking hook for the simplex `Engine` (`B: Blocker`).
///
/// The `Engine` calls `block` to disconnect and ban a misbehaving peer
/// (equivocation, invalid signatures). `Blocker::block` is **synchronous** and
/// returns [`Feedback`]; the trait requires `Clone + Send + 'static`.
///
/// Phase-2 scope: a compiling stub that logs the block request and acknowledges.
/// Real blocking — disconnecting/banning the peer on the dedicated commonware-p2p
/// transport — is wired when that transport is constructed alongside the `Engine`.
#[derive(Debug, Clone, Default)]
pub struct StubBlocker;

impl Blocker for StubBlocker {
    type PublicKey = SimplexPublicKey;

    fn block(&mut self, peer: Self::PublicKey) -> Feedback {
        warn!(
            target: "simplex",
            peer = %peer,
            "simplex requested peer block (stub: no-op until the p2p transport is wired)"
        );
        Feedback::Ok
    }
}

/// Assembler for the [`SimplexConfig`] value the `Engine` consumes.
///
/// A unit-struct carrier so the public API exports a type with a `build` method
/// rather than a loose function (per `CLAUDE.md`); [`SimplexConfig`] is a foreign
/// type alias, so an inherent `SimplexConfig::build` is not possible.
#[derive(Debug, Clone, Copy, Default)]
pub struct SimplexConfigBuilder;

impl SimplexConfigBuilder {
    /// Assembles the [`SimplexConfig`] value for the `Engine`.
    ///
    /// The identity-bearing / stateful pieces are passed in — `scheme` (this
    /// node's signing identity + participant set), `elector` (which participant
    /// indices may lead), `reporter` (the read-side watch), and the `genesis`
    /// digest for the `Floor` — while the inert Phase-2 stubs
    /// (`StubBlocker`/`StubAutomaton`/`StubRelay`) and `Sequential` strategy are
    /// defaulted, and the timing/buffer scalars use the placeholder constants
    /// below.
    ///
    /// Must be called with a live runtime `context` (only `page_cache` needs it,
    /// via `CacheRef::from_pooler`). Generic over the pooler so production
    /// ([`SimplexRuntimeContext`] = tokio) and tests (`deterministic::Context`)
    /// can both build it — `page_cache` is a concrete `CacheRef` regardless of
    /// which pooler produced it, and the Engine's `E` is a separate generic. The
    /// **timeout / buffer / epoch values are Phase-2 placeholders** mirroring
    /// commonware's own tests — real tuning (cross-region Δ budget, `ViewDelta`
    /// windows, partition naming, real genesis) is a Phase-3/4 task tracked in the
    /// review inbox. This does not start consensus; it only builds the config the
    /// `Engine::new` unit consumes inside a `Runner::start` closure.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        context: &impl commonware_runtime::BufferPooler,
        scheme: SimplexScheme,
        elector: PinnedLeaderConfig,
        reporter: StatusReporter,
        partition: String,
        genesis: SimplexDigest,
    ) -> SimplexConfig {
        commonware_consensus::simplex::Config {
            scheme,
            elector,
            blocker: StubBlocker,
            automaton: StubAutomaton,
            relay: StubRelay,
            reporter,
            // Off by default: extra per-validator historical vote reporting (the
            // equivocation-attribution path, an open Phase-2/4 verify item).
            track_historical_votes: false,
            strategy: SimplexStrategy::default(),
            partition,
            mailbox_size: NZUsize!(1024),
            epoch: Epoch::new(0),
            floor: Floor::Genesis(genesis),
            leader_timeout: Duration::from_secs(1),
            certification_timeout: Duration::from_secs(2),
            timeout_retry: Duration::from_secs(10),
            fetch_timeout: Duration::from_secs(1),
            // On this git rev `skip_timeout` is a `Duration` (was a `ViewDelta` on
            // =2026.7.0), and `view_retention` (`ViewDelta`) replaces the removed
            // `activity_timeout`/`fetch_concurrent`. Phase-3/4 tuning placeholders;
            // Config invariant: `skip_timeout` must exceed BOTH
            // `certification_timeout` (2s) and `timeout_retry` (10s).
            skip_timeout: Duration::from_secs(15),
            view_retention: ViewDelta::new(10),
            replay_buffer: NZUsize!(1024 * 1024),
            write_buffer: NZUsize!(1024 * 1024),
            page_cache: CacheRef::from_pooler(context, NZU16!(1024), NZUsize!(10)),
            forwarding: ForwardingPolicy::Disabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-byte hash (as produced by `BaseExecutionPayloadEnvelope::payload_hash`)
    /// round-trips losslessly into and out of [`SimplexDigest`], confirming the
    /// decided-value mapping holds at the type level.
    #[test]
    fn payload_hash_bytes_round_trip_through_digest() {
        let bytes = [7u8; 32];
        let digest = SimplexDigest::from(bytes);
        assert_eq!(digest.as_ref(), &bytes[..]);
    }

    /// The reporter constructs over a status watch and is `Clone` (required by the
    /// `Reporter` supertrait bound — the `Engine` owns and clones it while state is
    /// shared through the cloneable `watch::Sender`). Both the reporter and a clone
    /// observe the same channel.
    ///
    /// Note: driving a `Finalization` through `report` to assert the `view`
    /// projection requires constructing an ed25519 `S::Certificate`, which is only
    /// practical inside commonware's Engine test harness (or under its `mocks`
    /// feature); that behavioral test is deferred to the Engine-integration unit.
    #[test]
    fn reporter_is_clone_over_shared_status_channel() {
        let (status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let reporter = StatusReporter::new(status_tx);
        // Cloning yields an independent handle over the SAME channel: drop the
        // original, and a clone still keeps the receiver alive and observable.
        let clone = reporter.clone();
        drop(reporter);
        assert!(!status_rx.has_changed().unwrap());
        assert_eq!(status_rx.borrow().view, 0);
        drop(clone);
    }

    /// The reporter advances the observed `view` **monotonically**: a reordered
    /// or late-delivered lower-view activity must never make the reported view
    /// regress, and an equal view is a no-op. This is the core of the fix for the
    /// last-write-wins projection that could feed a stale/regressing leadership
    /// answer during failover.
    ///
    /// The projection is exercised through the private `advance_view` helper
    /// rather than by driving real `Activity` variants through `report`, because
    /// constructing a `Nullification`/`Certification`/`Finalization` requires a
    /// signed `S::Certificate` that is only practical to produce inside
    /// commonware's Engine test harness (same rationale as the reporter clone test
    /// above); a behavioral Activity-driven test lands with the multi-validator
    /// Engine tests.
    #[test]
    fn reporter_view_is_monotonic() {
        let (status_tx, status_rx) = watch::channel(ConsensusStatus::default());
        let reporter = StatusReporter::new(status_tx);

        reporter.advance_view(5);
        assert_eq!(status_rx.borrow().view, 5);

        // Reordered / late lower-view activity must not regress the view.
        reporter.advance_view(3);
        assert_eq!(status_rx.borrow().view, 5, "view must never regress");

        // Equal view is a no-op.
        reporter.advance_view(5);
        assert_eq!(status_rx.borrow().view, 5);

        // A strictly higher view advances (e.g. a nullification skip during
        // failover, which the old projection dropped entirely).
        reporter.advance_view(9);
        assert_eq!(status_rx.borrow().view, 9);
    }

    /// The stub relay accepts a broadcast for a `Propose` plan and acknowledges
    /// it. Confirms the `Relay` associated types (Digest/PublicKey/Plan) resolve
    /// against our concrete scheme and that `broadcast` is the synchronous
    /// `Feedback`-returning shape.
    #[test]
    fn stub_relay_broadcast_acknowledges() {
        use commonware_consensus::types::{Epoch, Round, View};

        let mut relay = StubRelay;
        let plan = Plan::Propose { round: Round::new(Epoch::new(0), View::new(1)) };
        let digest = SimplexDigest::from([0u8; 32]);
        assert!(matches!(relay.broadcast(digest, plan), Feedback::Ok));
    }

    /// The stub satisfies `CertifiableAutomaton` (and its `Automaton` supertrait,
    /// which requires `Clone`) — asserted at compile time via the bound below.
    /// A behavioral `propose`/`verify` test needs a `Context` carrying a real
    /// leader `PublicKey` (only practical inside the Engine test harness), so it
    /// is deferred to the Engine-integration unit.
    #[test]
    fn stub_automaton_satisfies_certifiable_automaton() {
        fn assert_certifiable<A: CertifiableAutomaton>(_: &A) {}
        let automaton = StubAutomaton;
        assert_certifiable(&automaton);
        assert_eq!(format!("{automaton:?}"), "StubAutomaton");
    }

    /// A pinned-leader elector elects **only** from the pinned subset (here, the
    /// single participant index 1), regardless of round — realizing leader ⊆
    /// voter. An empty config with `Terms::rotating()` degrades to all
    /// participants, one leader per view (round-robin over the full set).
    #[test]
    fn pinned_leader_elector_rotates_only_over_subset() {
        use commonware_consensus::types::{Epoch, View};
        use commonware_cryptography::{Signer, ed25519::PrivateKey};

        // Three voters; deterministic keys via insecure test-only from_seed.
        let participants: Set<SimplexPublicKey> =
            Set::from_iter_dedup((0..3u64).map(|seed| PrivateKey::from_seed(seed).public_key()));

        // Pin leadership to participant index 1 only: every round elects it
        // (single pinned leader ⇒ same leader regardless of the term policy).
        let pinned = PinnedLeaderConfig::new(vec![1]).build(&participants);
        for (epoch, view) in [(0u64, 1u64), (0, 2), (1, 5), (7, 3)] {
            let leader = pinned.elect(Round::new(Epoch::new(epoch), View::new(view)), None);
            assert_eq!(leader, Participant::from_usize(1), "pinned leader must be index 1");
        }

        // Empty config + rotating terms degrades to all participants, one leader
        // per view (round-robin over the full set).
        let all = PinnedLeaderConfig::with_terms(vec![], Terms::rotating()).build(&participants);
        assert_eq!(
            all.elect(Round::new(Epoch::new(0), View::new(0)), None),
            Participant::from_usize(0)
        );
        assert_eq!(
            all.elect(Round::new(Epoch::new(0), View::new(1)), None),
            Participant::from_usize(1)
        );
        assert_eq!(
            all.elect(Round::new(Epoch::new(0), View::new(3)), None),
            Participant::from_usize(0)
        );
    }

    /// **Stable-leader**: within a term (a run of `Terms::length()` consecutive
    /// views) the SAME pinned leader is elected for every view; the leader advances
    /// to the next pinned candidate only at the term boundary. This is the core
    /// latency property of the effort — one leader proposing across consecutive
    /// views on the happy path, not a cross-region hop every view. Uses an explicit
    /// term length of 3 over k=3 pinned leaders (indices 0,1,2) of a 5-voter set,
    /// so leadership stability and leader ⊆ voter are both exercised.
    #[test]
    fn pinned_leader_elector_is_stable_within_term() {
        use commonware_consensus::types::{Epoch, View};
        use commonware_cryptography::{Signer, ed25519::PrivateKey};

        let participants: Set<SimplexPublicKey> =
            Set::from_iter_dedup((0..5u64).map(|seed| PrivateKey::from_seed(seed).public_key()));

        // Term length 3: a pinned leader holds for 3 consecutive views.
        let terms =
            Terms::stable(TermLength::new(NZU32!(3)), Duration::from_secs(10), ViewDelta::new(0));
        let elector = PinnedLeaderConfig::with_terms(vec![0, 1, 2], terms).build(&participants);
        let leader = |view: u64| elector.elect(Round::new(Epoch::new(0), View::new(view)), None);

        // Views 1,2,3 are one term ⇒ the SAME leader across all three.
        let first = leader(1);
        assert_eq!(first, leader(2), "leader must not rotate within a term");
        assert_eq!(first, leader(3), "leader must not rotate within a term");

        // View 4 opens the next term ⇒ the leader advances, then stays stable.
        let second = leader(4);
        assert_ne!(first, second, "leader must advance at the term boundary");
        assert_eq!(second, leader(5), "leader must stay stable across the new term");
        assert_eq!(second, leader(6), "leader must stay stable across the new term");

        // Every elected leader is within the pinned subset {0,1,2}.
        let pinned = [0usize, 1, 2].map(Participant::from_usize);
        for view in 1..=9 {
            assert!(pinned.contains(&leader(view)), "elected leader must be in the pinned subset");
        }
    }

    /// The stub blocker acknowledges a block request for a real peer key.
    #[test]
    fn stub_blocker_acknowledges_block() {
        use commonware_cryptography::{Signer, ed25519::PrivateKey};

        let mut blocker = StubBlocker;
        let peer = PrivateKey::from_seed(0).public_key();
        assert!(matches!(blocker.block(peer), Feedback::Ok));
    }

    /// The chosen strategy satisfies commonware's `Strategy` (the Engine's `T`),
    /// asserted at compile time; `Sequential` constructs trivially.
    #[test]
    fn simplex_strategy_satisfies_strategy() {
        fn assert_strategy<T: commonware_parallel::Strategy>(_: &T) {}
        let strategy = SimplexStrategy::default();
        assert_strategy(&strategy);
    }

    /// The runtime context satisfies the five nameable `commonware_runtime` bounds
    /// of the Engine's `E` (compile-time). The sixth, `rand_core::CryptoRng`, is
    /// satisfied via a blanket impl (source-verified) and is checked for real when
    /// `Engine::new` is written; asserting it here would require adding `rand_core`
    /// as a direct dep just for a test.
    #[test]
    fn simplex_runtime_context_satisfies_runtime_env() {
        use commonware_runtime::{BufferPooler, Clock, Metrics, Spawner, Storage};
        fn assert_env<E: BufferPooler + Clock + Spawner + Storage + Metrics>() {}
        assert_env::<SimplexRuntimeContext>();
    }

    /// `SimplexConfig` is well-formed — naming it as a concrete type forces the
    /// compiler to check that all eight `Config<S,L,B,D,A,R,F,T>` where-clause
    /// bounds hold *together* for our concrete components (e.g.
    /// `A: CertifiableAutomaton<Context = Context<D, S::PublicKey>>`,
    /// `F: Reporter<Activity = Activity<S, D>>`, `L: elector::Config<S>`). If the
    /// eight pieces didn't fit the Engine, this would not compile.
    #[test]
    fn simplex_config_type_is_well_formed() {
        fn assert_well_formed(_: Option<SimplexConfig>) {}
        assert_well_formed(None);
    }

    /// The concrete authenticated-transport channel types satisfy exactly what
    /// `Engine::start` requires for each of its three channel pairs:
    /// `Sender`/`Receiver` keyed by `SimplexPublicKey`. Compile-time.
    #[test]
    fn simplex_net_channel_types_match_engine_start() {
        fn assert_channel<Se, Re>()
        where
            Se: commonware_p2p::Sender<PublicKey = SimplexPublicKey>,
            Re: commonware_p2p::Receiver<PublicKey = SimplexPublicKey>,
        {
        }
        assert_channel::<SimplexNetSender, SimplexNetReceiver>();
    }

    /// End-to-end construction: build a real ed25519 scheme + all our glue for a
    /// single validator, wire a `simulated` network's three channels, assemble the
    /// config, and `Engine::new` + `engine.start`. Exercises the whole Phase-2
    /// stack against a REAL commonware Engine on the `deterministic` runtime — the
    /// payoff that ties every pinned type + impl together at runtime. Uses the
    /// production `Scheme::signer` ctor (no `mocks` feature) and the `simulated`
    /// network (no sockets). Asserts construction + start succeed; consensus
    /// progress itself is exercised by later multi-validator tests.
    #[test]
    fn single_validator_engine_constructs_and_starts() {
        use std::num::NonZeroU32;

        use commonware_cryptography::{Signer, ed25519};
        use commonware_p2p::simulated;
        use commonware_runtime::{Quota, Runner as _, Supervisor as _, deterministic};

        let executor = deterministic::Runner::timed(Duration::from_secs(5));
        executor.start(|context| async move {
            // One validator: deterministic key, participant set of just itself.
            let private_key = ed25519::PrivateKey::from_seed(0);
            let public_key = private_key.public_key();
            let participants: Set<SimplexPublicKey> = Set::from_iter_dedup([public_key.clone()]);
            let scheme = SimplexScheme::signer(b"base-simplex-test", participants, private_key)
                .expect("signer key is in the participant set");

            // Simulated network with a single peer; register the 3 simplex channels.
            let (network, oracle) = simulated::Network::new_with_peers(
                context.child("network"),
                simulated::Config {
                    max_size: 1024 * 1024,
                    max_peers_per_set: NZUsize!(1),
                    disconnect_on_block: true,
                    tracked_peer_sets: NZUsize!(1),
                },
                std::iter::once(public_key.clone()),
            )
            .await;
            network.start();
            let control = oracle.control(public_key.clone());
            let quota = Quota::per_second(NonZeroU32::MAX);
            let vote = control.register(SIMPLEX_VOTE_CHANNEL, quota).await.unwrap();
            let certificate = control.register(SIMPLEX_CERTIFICATE_CHANNEL, quota).await.unwrap();
            let resolver = control.register(SIMPLEX_RESOLVER_CHANNEL, quota).await.unwrap();

            // Our glue + config value.
            let (status_tx, _status_rx) = watch::channel(ConsensusStatus::default());
            let cfg = SimplexConfigBuilder::build(
                &context,
                scheme,
                PinnedLeaderConfig::new(vec![0]),
                StatusReporter::new(status_tx),
                public_key.to_string(),
                SimplexDigest::from([0u8; 32]),
            );

            // Construct + start a real Engine, then abort (we assert wiring, not
            // full consensus progress here).
            let engine = commonware_consensus::simplex::Engine::new(context.child("engine"), cfg);
            let handle = engine.start(vote, certificate, resolver);
            handle.abort();
        });
    }
}
