//! Bounded worker lifecycle, cancellation, and capacity controls.

use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use rayon::{ThreadPool, ThreadPoolBuilder};
use thiserror::Error;
use tracing::error;

/// Victim work deadline in milliseconds.
pub const DEADLINE_MILLIS: u64 = 40;
/// Additional hang-classification grace in milliseconds.
pub const HANG_GRACE_MILLIS: u64 = 40;
/// Maximum direct fixture pools.
pub const MAX_POOLS: usize = 512;
/// Maximum total initialized ticks.
pub const MAX_TOTAL_TICKS: usize = 4_096;
/// Maximum exact-prefix transactions.
pub const MAX_PREFIX_TRANSACTIONS: usize = 4_096;
/// Maximum materialized accounts.
pub const MAX_ACCOUNTS: usize = 4_096;
/// Maximum materialized storage slots.
pub const MAX_STORAGE_SLOTS: usize = 65_536;
/// Maximum materialized code entries.
pub const MAX_CODE_ENTRIES: usize = 2_048;
/// Maximum total materialized code bytes.
pub const MAX_CODE_BYTES: usize = 16 * 1024 * 1024;
/// Maximum token pairs.
pub const MAX_PAIRS: usize = 8_192;
/// Maximum optimizer candidates.
pub const MAX_CANDIDATES: usize = 8_192;
/// Maximum canonical measurement bytes.
pub const MAX_CANONICAL_BYTES: usize = 1024 * 1024;
/// Maximum public plans emitted per frame.
pub const MAX_PLANS_PER_FRAME: usize = 1;
/// Dedicated analysis pool size.
pub const ANALYSIS_THREADS: usize = 4;

/// Complete count shape checked before any bounded allocation or clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadSize {
    /// Exact-prefix transaction count.
    pub prefix_transactions: usize,
    /// Registry pool count.
    pub pools: usize,
    /// Materialized account count.
    pub accounts: usize,
    /// Materialized storage-slot count.
    pub storage_slots: usize,
    /// Materialized code-entry count.
    pub code_entries: usize,
    /// Materialized code byte count.
    pub code_bytes: usize,
    /// Initialized tick count.
    pub initialized_ticks: usize,
    /// Token pair count.
    pub pairs: usize,
    /// Optimizer candidate count.
    pub candidates: usize,
    /// Canonical output byte count.
    pub canonical_bytes: usize,
    /// Public plan count.
    pub plans: usize,
}

/// Approved inclusive workload caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkCaps {
    /// Exact-prefix transaction cap.
    pub prefix_transactions: usize,
    /// Pool cap.
    pub pools: usize,
    /// Account cap.
    pub accounts: usize,
    /// Storage-slot cap.
    pub storage_slots: usize,
    /// Code-entry cap.
    pub code_entries: usize,
    /// Code-byte cap.
    pub code_bytes: usize,
    /// Initialized-tick cap.
    pub initialized_ticks: usize,
    /// Pair cap.
    pub pairs: usize,
    /// Candidate cap.
    pub candidates: usize,
    /// Canonical-byte cap.
    pub canonical_bytes: usize,
    /// Plan-per-frame cap.
    pub plans: usize,
}

impl Default for WorkCaps {
    fn default() -> Self {
        Self {
            prefix_transactions: MAX_PREFIX_TRANSACTIONS,
            pools: MAX_POOLS,
            accounts: MAX_ACCOUNTS,
            storage_slots: MAX_STORAGE_SLOTS,
            code_entries: MAX_CODE_ENTRIES,
            code_bytes: MAX_CODE_BYTES,
            initialized_ticks: MAX_TOTAL_TICKS,
            pairs: MAX_PAIRS,
            candidates: MAX_CANDIDATES,
            canonical_bytes: MAX_CANONICAL_BYTES,
            plans: MAX_PLANS_PER_FRAME,
        }
    }
}

impl WorkCaps {
    /// Returns true only when every checked count is within its inclusive cap.
    pub const fn admits(&self, size: WorkloadSize) -> bool {
        size.prefix_transactions <= self.prefix_transactions
            && size.pools <= self.pools
            && size.accounts <= self.accounts
            && size.storage_slots <= self.storage_slots
            && size.code_entries <= self.code_entries
            && size.code_bytes <= self.code_bytes
            && size.initialized_ticks <= self.initialized_ticks
            && size.pairs <= self.pairs
            && size.candidates <= self.candidates
            && size.canonical_bytes <= self.canonical_bytes
            && size.plans <= self.plans
    }
}

/// Process-wide fail-closed lifecycle state.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalState {
    /// Ingress and analysis may run.
    Running = 0,
    /// Permanently disabled until process restart.
    DisabledNoTrade = 1,
    /// Ingress is closed for shutdown.
    Closed = 2,
}

/// Reason for a permanent no-trade transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableReason {
    /// A task panicked.
    Panic,
    /// A cancelled task did not acknowledge drop within grace.
    Hung,
}

/// Shared global lifecycle controller.
#[derive(Debug)]
pub struct GlobalLifecycle {
    state: AtomicU8,
}

impl Default for GlobalLifecycle {
    fn default() -> Self {
        Self { state: AtomicU8::new(GlobalState::Running as u8) }
    }
}

impl GlobalLifecycle {
    /// Loads the global state with sequential consistency.
    pub fn state(&self) -> GlobalState {
        match self.state.load(Ordering::SeqCst) {
            0 => GlobalState::Running,
            1 => GlobalState::DisabledNoTrade,
            _ => GlobalState::Closed,
        }
    }

    /// Permanently disables work after panic or missing cancellation acknowledgement.
    pub fn disable(&self, reason: DisableReason) {
        if self
            .state
            .compare_exchange(
                GlobalState::Running as u8,
                GlobalState::DisabledNoTrade as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            error!(reason = ?reason, "MEV trader disabled to no-trade");
        }
    }

    /// Closes ingress for shutdown without starting replacement work.
    pub fn close(&self) {
        self.state.store(GlobalState::Closed as u8, Ordering::SeqCst);
    }
}

/// Per-task single-winner state.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Task may still produce output.
    Active = 0,
    /// Deadline or control requested cancellation.
    CancelRequested = 1,
    /// Task observed cancellation and dropped all output.
    DroppedAcked = 2,
    /// Task completed before cancellation won.
    Completed = 3,
}

/// Sequentially consistent cancellation token with a fixed deadline.
#[derive(Debug)]
pub struct CancellationToken {
    state: AtomicU8,
    deadline: Instant,
}

impl CancellationToken {
    /// Creates an active token with an immutable deadline.
    pub const fn new(deadline: Instant) -> Self {
        Self { state: AtomicU8::new(TaskState::Active as u8), deadline }
    }

    /// Creates an active token using the approved 40ms deadline.
    pub fn with_approved_deadline(now: Instant) -> Self {
        Self::new(now + Duration::from_millis(DEADLINE_MILLIS))
    }

    /// Returns the immutable task deadline.
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Loads the task state with sequential consistency.
    pub fn state(&self) -> TaskState {
        match self.state.load(Ordering::SeqCst) {
            0 => TaskState::Active,
            1 => TaskState::CancelRequested,
            2 => TaskState::DroppedAcked,
            _ => TaskState::Completed,
        }
    }

    /// Lets cancellation win only from Active.
    pub fn request_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                TaskState::Active as u8,
                TaskState::CancelRequested as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Acknowledges dropped output only after cancellation won.
    pub fn acknowledge_drop(&self) -> bool {
        self.state
            .compare_exchange(
                TaskState::CancelRequested as u8,
                TaskState::DroppedAcked as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Lets completion win only before deadline while authority and global running remain true.
    pub fn complete(
        &self,
        now: Instant,
        current_authority: bool,
        global: &GlobalLifecycle,
    ) -> bool {
        if now >= self.deadline || !current_authority || global.state() != GlobalState::Running {
            self.request_cancel();
            return false;
        }
        self.state
            .compare_exchange(
                TaskState::Active as u8,
                TaskState::Completed as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }
}

/// Cheap cancellation checkpoint shared by every bounded analysis loop.
#[derive(Debug, Clone)]
pub struct CancellationProbe {
    token: Arc<CancellationToken>,
    global: Arc<GlobalLifecycle>,
}

impl CancellationProbe {
    /// Binds a task token to the global fail-closed lifecycle.
    pub const fn new(token: Arc<CancellationToken>, global: Arc<GlobalLifecycle>) -> Self {
        Self { token, global }
    }

    /// Returns true only while task, deadline, authority, and global state remain live.
    pub fn checkpoint(&self, now: Instant, current_authority: bool) -> bool {
        if now >= self.token.deadline() {
            self.token.request_cancel();
        }
        self.token.state() == TaskState::Active
            && current_authority
            && self.global.state() == GlobalState::Running
    }

    /// Acknowledges that this task dropped all output after observing cancellation.
    pub fn acknowledge_drop(&self) -> bool {
        self.token.acknowledge_drop()
    }

    /// Returns the bound token.
    pub const fn token(&self) -> &Arc<CancellationToken> {
        &self.token
    }

    /// Returns the bound global lifecycle.
    pub const fn global(&self) -> &Arc<GlobalLifecycle> {
        &self.global
    }
}

/// Outcome of watchdog inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogStatus {
    /// Deadline has not elapsed.
    Active,
    /// Cancellation was requested and grace has not elapsed.
    Grace,
    /// Cancellation was acknowledged with zero output.
    Dropped,
    /// Task completed before cancellation.
    Completed,
    /// Missing acknowledgement permanently disabled the runtime.
    HungDisabled,
}

/// Separate deadline and hang-classification controller.
#[derive(Debug, Default, Clone, Copy)]
pub struct Watchdog;

impl Watchdog {
    /// Applies 40ms cancellation and the additional 40ms classification-only grace.
    pub fn inspect(
        &self,
        now: Instant,
        token: &CancellationToken,
        global: &GlobalLifecycle,
    ) -> WatchdogStatus {
        if now < token.deadline() {
            return WatchdogStatus::Active;
        }
        token.request_cancel();
        match token.state() {
            TaskState::DroppedAcked => WatchdogStatus::Dropped,
            TaskState::Completed => WatchdogStatus::Completed,
            TaskState::Active | TaskState::CancelRequested
                if now < token.deadline() + Duration::from_millis(HANG_GRACE_MILLIS) =>
            {
                WatchdogStatus::Grace
            }
            TaskState::Active | TaskState::CancelRequested => {
                global.disable(DisableReason::Hung);
                WatchdogStatus::HungDisabled
            }
        }
    }
}

/// Capacity-one latest-wins submit result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotSubmit {
    /// Item filled an empty slot.
    Accepted,
    /// Item replaced one older unclaimed item.
    Replaced,
    /// Global lifecycle rejected ingress.
    Closed,
}

/// Capacity-one latest-wins slot consumed by a sole worker.
#[derive(Debug)]
pub struct LatestSlot<T> {
    value: Mutex<Option<T>>,
    global: Arc<GlobalLifecycle>,
}

impl<T> LatestSlot<T> {
    /// Creates one empty capacity-one slot.
    pub const fn new(global: Arc<GlobalLifecycle>) -> Self {
        Self { value: Mutex::new(None), global }
    }

    /// Stores the latest item, replacing at most one unclaimed older item.
    pub fn submit(&self, item: T) -> SlotSubmit {
        if self.global.state() != GlobalState::Running {
            return SlotSubmit::Closed;
        }
        let mut value = self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if value.replace(item).is_some() { SlotSubmit::Replaced } else { SlotSubmit::Accepted }
    }

    /// Takes the sole currently queued item without blocking.
    pub fn try_take(&self) -> Option<T> {
        self.value.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take()
    }
}

/// Errors while establishing the sole worker or dedicated analysis pool.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// Dedicated Rayon4 pool construction failed.
    #[error("dedicated analysis pool construction failed")]
    PoolBuild,
    /// The sole worker was already claimed and is never replaced.
    #[error("sole worker already claimed")]
    WorkerAlreadyClaimed,
}

/// One-time sole-worker claim guard with no replacement path.
#[derive(Debug, Default)]
pub struct SoleWorker {
    claimed: AtomicBool,
}

impl SoleWorker {
    /// Claims the only worker exactly once for the process lifetime.
    pub fn claim(&self) -> Result<WorkerClaim, LifecycleError> {
        self.claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| LifecycleError::WorkerAlreadyClaimed)?;
        Ok(WorkerClaim { marker: () })
    }
}

/// Irreversible sole-worker claim.
#[derive(Debug)]
pub struct WorkerClaim {
    marker: (),
}

impl WorkerClaim {
    /// Returns a stable unit marker without exposing replacement authority.
    pub const fn marker(&self) {
        self.marker
    }
}

/// Dedicated four-thread Rayon analysis pool.
pub struct DedicatedAnalysisPool {
    pool: ThreadPool,
}

impl fmt::Debug for DedicatedAnalysisPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DedicatedAnalysisPool")
            .field("threads", &self.pool.current_num_threads())
            .finish()
    }
}

impl DedicatedAnalysisPool {
    /// Builds the exact dedicated Rayon4 pool.
    pub fn new() -> Result<Self, LifecycleError> {
        let pool = ThreadPoolBuilder::new()
            .num_threads(ANALYSIS_THREADS)
            .build()
            .map_err(|_| LifecycleError::PoolBuild)?;
        Ok(Self { pool })
    }

    /// Runs one bounded task through the dedicated pool's `install` edge.
    pub fn install<Output, Work>(&self, probe: &CancellationProbe, work: Work) -> Option<Output>
    where
        Output: Send,
        Work: FnOnce(&CancellationProbe) -> Output + Send,
    {
        if !probe.checkpoint(Instant::now(), true) {
            return None;
        }
        let output = self.pool.install(|| work(probe));
        probe.checkpoint(Instant::now(), true).then_some(output)
    }

    /// Returns the fixed number of dedicated analysis threads.
    pub fn thread_count(&self) -> usize {
        self.pool.current_num_threads()
    }
}

/// Result of panic-isolated task execution.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskRun<Output> {
    /// Task returned normally; deadline completion still controls publication.
    Returned(Output),
    /// Task panicked and permanently disabled the runtime.
    Panicked,
}

/// Panic isolation around the dedicated analysis edge.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskRunner;

impl TaskRunner {
    /// Runs one task and converts any panic into permanent no-trade.
    pub fn run<Output, Work>(&self, global: &GlobalLifecycle, work: Work) -> TaskRun<Output>
    where
        Work: FnOnce() -> Output,
    {
        catch_unwind(AssertUnwindSafe(work)).map_or_else(
            |_| {
                global.disable(DisableReason::Panic);
                TaskRun::Panicked
            },
            TaskRun::Returned,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use rayon::prelude::*;

    use super::*;

    fn maximum_size() -> WorkloadSize {
        WorkloadSize {
            prefix_transactions: MAX_PREFIX_TRANSACTIONS,
            pools: MAX_POOLS,
            accounts: MAX_ACCOUNTS,
            storage_slots: MAX_STORAGE_SLOTS,
            code_entries: MAX_CODE_ENTRIES,
            code_bytes: MAX_CODE_BYTES,
            initialized_ticks: MAX_TOTAL_TICKS,
            pairs: MAX_PAIRS,
            candidates: MAX_CANDIDATES,
            canonical_bytes: MAX_CANONICAL_BYTES,
            plans: MAX_PLANS_PER_FRAME,
        }
    }

    #[test]
    fn caps_are_inclusive_and_never_truncate() {
        let caps = WorkCaps::default();
        assert!(caps.admits(maximum_size()));
        let mut exceeded = maximum_size();
        exceeded.storage_slots += 1;
        assert!(!caps.admits(exceeded));
        exceeded = maximum_size();
        exceeded.code_bytes += 1;
        assert!(!caps.admits(exceeded));
        exceeded = maximum_size();
        exceeded.plans += 1;
        assert!(!caps.admits(exceeded));
    }

    #[test]
    fn capacity_one_keeps_only_latest_for_sole_worker() {
        let global = Arc::new(GlobalLifecycle::default());
        let slot = LatestSlot::new(Arc::clone(&global));
        assert_eq!(slot.submit(1), SlotSubmit::Accepted);
        assert_eq!(slot.submit(2), SlotSubmit::Replaced);
        assert_eq!(slot.try_take(), Some(2));
        assert_eq!(slot.try_take(), None);
        global.close();
        assert_eq!(slot.submit(3), SlotSubmit::Closed);
    }

    #[test]
    fn worker_claim_has_no_replacement() {
        let worker = SoleWorker::default();
        {
            let claim = worker.claim().expect("first claim");
            claim.marker();
            assert!(matches!(worker.claim(), Err(LifecycleError::WorkerAlreadyClaimed)));
        }
        assert!(matches!(worker.claim(), Err(LifecycleError::WorkerAlreadyClaimed)));
    }

    #[test]
    fn completion_and_cancellation_have_one_seqcst_winner() {
        let now = Instant::now();
        let global = GlobalLifecycle::default();
        let completed = CancellationToken::with_approved_deadline(now);
        assert!(completed.complete(now, true, &global));
        assert!(!completed.request_cancel());
        assert_eq!(completed.state(), TaskState::Completed);

        let cancelled = CancellationToken::with_approved_deadline(now);
        assert!(cancelled.request_cancel());
        assert!(!cancelled.complete(now, true, &global));
        assert!(cancelled.acknowledge_drop());
        assert_eq!(cancelled.state(), TaskState::DroppedAcked);
    }

    #[test]
    fn completion_requires_current_authority_and_running_global_state() {
        let now = Instant::now();

        let stale = CancellationToken::with_approved_deadline(now);
        assert!(!stale.complete(now, false, &GlobalLifecycle::default()));
        assert_eq!(stale.state(), TaskState::CancelRequested);

        let global = GlobalLifecycle::default();
        global.close();
        let closed = CancellationToken::with_approved_deadline(now);
        assert!(!closed.complete(now, true, &global));
        assert_eq!(closed.state(), TaskState::CancelRequested);
    }

    #[test]
    fn deadline_is_output_zero_and_grace_only_classifies_hang() {
        let now = Instant::now();
        let global = GlobalLifecycle::default();
        let token = CancellationToken::with_approved_deadline(now);
        let deadline = token.deadline();

        assert!(!token.complete(deadline, true, &global));
        assert_eq!(Watchdog.inspect(deadline, &token, &global), WatchdogStatus::Grace);
        assert_eq!(
            Watchdog.inspect(deadline + Duration::from_millis(HANG_GRACE_MILLIS), &token, &global,),
            WatchdogStatus::HungDisabled
        );
        assert_eq!(global.state(), GlobalState::DisabledNoTrade);
    }

    #[test]
    fn acknowledged_drop_before_grace_never_disables_global_state() {
        let now = Instant::now();
        let global = GlobalLifecycle::default();
        let token = CancellationToken::with_approved_deadline(now);
        let deadline = token.deadline();

        assert_eq!(Watchdog.inspect(deadline, &token, &global), WatchdogStatus::Grace);
        assert!(token.acknowledge_drop());
        assert_eq!(
            Watchdog.inspect(deadline + Duration::from_millis(HANG_GRACE_MILLIS), &token, &global,),
            WatchdogStatus::Dropped
        );
        assert_eq!(global.state(), GlobalState::Running);
    }

    #[test]
    fn panic_disables_without_replacement() {
        let global = GlobalLifecycle::default();
        let result: TaskRun<()> = TaskRunner.run(&global, || panic!("fixture panic"));
        assert_eq!(result, TaskRun::Panicked);
        assert_eq!(global.state(), GlobalState::DisabledNoTrade);
    }

    #[test]
    fn warmup_ten_then_thousand_cancellations_ack_within_deadline() {
        let pool = DedicatedAnalysisPool::new().expect("Rayon4");
        assert_eq!(pool.thread_count(), ANALYSIS_THREADS);
        let mut maximum_ack = Duration::ZERO;
        for iteration in 0..1_010 {
            let global = Arc::new(GlobalLifecycle::default());
            let token = Arc::new(CancellationToken::with_approved_deadline(Instant::now()));
            let probe = CancellationProbe::new(Arc::clone(&token), global);
            let token_for_task = Arc::clone(&token);
            let started = Instant::now();
            let _ = pool.install(&probe, |probe| {
                (0..ANALYSIS_THREADS).into_par_iter().for_each(|lane| {
                    black_box(lane.wrapping_mul(17));
                });
                token_for_task.request_cancel();
                assert!(!probe.checkpoint(Instant::now(), true));
                probe.acknowledge_drop();
            });
            let elapsed = started.elapsed();
            assert_eq!(token.state(), TaskState::DroppedAcked);
            if iteration >= 10 {
                maximum_ack = maximum_ack.max(elapsed);
            }
        }
        assert!(maximum_ack <= Duration::from_millis(DEADLINE_MILLIS));
    }
}
