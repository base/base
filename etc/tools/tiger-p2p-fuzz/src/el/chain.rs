//! The local chain both sides check Status and fork ids against in-process: a Base-style fork
//! schedule with forks at genesis, one passed timestamp fork and two future ones.

use alloy_eip2124::{ForkFilter, ForkFilterKey, ForkHash, Head as RethHead};
use alloy_primitives::{B256, keccak256};
use base_tiger_net::{ForkActivation, ForkId, ForkSchedule, Head};

/// Chain parameters shared by both sides.
#[derive(Debug, Clone)]
pub(crate) struct LocalChain {
    /// The network id (chain id).
    pub(crate) network_id: u64,
    /// The fork schedule (genesis hash and activations).
    pub(crate) schedule: ForkSchedule,
    /// The local head.
    pub(crate) head: Head,
    /// The local head hash.
    pub(crate) head_hash: B256,
}

impl LocalChain {
    /// The default fuzzing chain: genesis at `1_700_000_000`, Jovian passed, Azul and Beryl ahead.
    pub(crate) fn fuzz() -> Self {
        let genesis_timestamp = 1_700_000_000;
        let t = genesis_timestamp;
        let schedule = ForkSchedule::base(
            keccak256(b"tiger-p2p-fuzz genesis"),
            genesis_timestamp,
            0,
            &[t, t, t, t, t, t, t, t + 1_000, t + 2_000, t + 3_000],
        );
        Self {
            network_id: 845_300,
            schedule,
            head: Head { number: 1_500, timestamp: t + 1_500 },
            head_hash: keccak256(b"tiger-p2p-fuzz head"),
        }
    }

    /// base-reth's filter over the same schedule.
    pub(crate) fn fork_filter(&self) -> ForkFilter {
        filter(&self.schedule, self.head)
    }

    /// Fork ids of every point of the schedule: (hash, next) for each passed and future fork.
    pub(crate) fn schedule_ids(&self) -> Vec<ForkId> {
        schedule_ids(&self.schedule)
    }
}

/// reth's fork filter over a tiger schedule.
pub(crate) fn filter(schedule: &ForkSchedule, head: Head) -> ForkFilter {
    let keys = schedule.forks.iter().map(|f| match f {
        ForkActivation::Block(n) => ForkFilterKey::Block(*n),
        ForkActivation::Timestamp(t) => ForkFilterKey::Time(*t),
    });
    let head = RethHead { number: head.number, timestamp: head.timestamp, ..Default::default() };
    ForkFilter::new(head, schedule.genesis_hash, schedule.genesis_timestamp, keys)
}

/// The fork ids a peer at each point of the schedule announces (computed by tiger at heads just
/// past each activation).
pub(crate) fn schedule_ids(schedule: &ForkSchedule) -> Vec<ForkId> {
    let mut ids = vec![schedule.fork_id(Head { number: 0, timestamp: schedule.genesis_timestamp })];
    for fork in &schedule.forks {
        let head = match fork {
            ForkActivation::Block(n) => Head { number: *n, timestamp: schedule.genesis_timestamp },
            ForkActivation::Timestamp(t) => Head { number: u64::MAX / 2, timestamp: *t },
        };
        let id = schedule.fork_id(head);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids
}

/// A fork hash in reth's type.
pub(crate) const fn reth_id(id: ForkId) -> alloy_eip2124::ForkId {
    alloy_eip2124::ForkId { hash: ForkHash(id.hash), next: id.next }
}
