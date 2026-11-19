//! Per-sealed-block builder observations that join state-root time to block contents and
//! resource-metering usage.

use std::{fmt, time::Duration};

use alloy_primitives::{Address, B256};
use reth_trie_common::{HashedPostState, updates::TrieUpdates};
use revm::database::BundleState;
use tracing::Level;

use crate::ResourceMeteringDimension;

/// State-root time at or above which a sealed block's observation logs at `warn` instead of
/// `debug`.
///
/// On Mainnet the builder's p99 is about 140 ms, and about 2% of sampled follower blocks reach
/// 100 ms. A 50 ms threshold therefore warns on more than 1% of blocks, covering the whole slow
/// tail that resource metering should explain rather than only its worst blocks, while most
/// blocks stay at `debug`.
pub const SLOW_BLOCK_STATE_ROOT: Duration = Duration::from_millis(50);

/// How a sealed block's state root was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateRootSource {
    /// The parallel state-root job, fed by execution, returned the root.
    Task,
    /// The builder computed the root synchronously because no state-root job was provided.
    Synchronous,
    /// The state-root job failed and the builder computed the root synchronously.
    TaskFallback,
}

impl StateRootSource {
    /// Returns the low-cardinality label and log value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Synchronous => "synchronous",
            Self::TaskFallback => "task_fallback",
        }
    }
}

/// Time a sealed block spent on its state root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateRootTiming {
    /// How the root was computed.
    pub source: StateRootSource,
    /// Time blocked on the state-root job after execution; zero without a job.
    pub wait: Duration,
    /// Time from the end of transaction execution until the block was sealed. Includes
    /// [`Self::wait`], hashing the post-state, any synchronous state root, and block assembly.
    pub total: Duration,
}

impl StateRootTiming {
    /// Returns [`Self::total`] in whole microseconds.
    pub fn total_us(&self) -> u64 {
        u64::try_from(self.total.as_micros()).unwrap_or(u64::MAX)
    }

    /// Returns [`Self::wait`] in whole microseconds.
    pub fn wait_us(&self) -> u64 {
        u64::try_from(self.wait.as_micros()).unwrap_or(u64::MAX)
    }

    /// Returns whether [`Self::total`] meets or exceeds [`SLOW_BLOCK_STATE_ROOT`].
    pub fn is_slow(&self) -> bool {
        self.total >= SLOW_BLOCK_STATE_ROOT
    }
}

/// The account with the most storage changes in a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LargestStorage {
    /// Unhashed account address.
    pub address: Address,
    /// Changed storage slots of the account.
    pub slots: usize,
}

/// Sizes of one block's state diff and the trie updates its state root produced.
///
/// Counts come from iterating the maps once; nothing is cloned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateDiffSizes {
    /// Entries in [`HashedPostState::accounts`].
    pub hashed_accounts: usize,
    /// Entries in [`HashedPostState::storages`]: accounts with at least one changed slot or a
    /// wiped storage.
    pub hashed_storages: usize,
    /// Changed slots across every hashed storage.
    pub hashed_storage_slots: usize,
    /// The account with the most changed slots, if any slot changed.
    ///
    /// Taken from the bundle state, which holds the same slots as the hashed post-state but keeps
    /// the unhashed address. Ties resolve to the higher address.
    pub largest_storage: Option<LargestStorage>,
    /// Entries in [`TrieUpdates::account_nodes`].
    pub trie_account_nodes: usize,
    /// Entries in [`TrieUpdates::storage_tries`].
    pub trie_storage_tries: usize,
    /// Updated storage trie nodes across every storage trie.
    pub trie_storage_nodes: usize,
    /// Removed account trie nodes plus removed storage trie nodes.
    pub trie_removed_nodes: usize,
}

impl StateDiffSizes {
    /// Counts the sizes of a block's hashed post-state, trie updates, and bundle state.
    pub fn new(hashed: &HashedPostState, trie: &TrieUpdates, bundle: &BundleState) -> Self {
        let hashed_storage_slots =
            hashed.storages.values().map(|storage| storage.storage.len()).sum();
        let largest_storage = bundle
            .state
            .iter()
            .map(|(address, account)| (account.storage.len(), *address))
            .filter(|(slots, _)| *slots > 0)
            .max()
            .map(|(slots, address)| LargestStorage { address, slots });
        let (trie_storage_nodes, storage_removed_nodes) =
            trie.storage_tries.values().fold((0, 0), |(nodes, removed), storage| {
                (nodes + storage.storage_nodes.len(), removed + storage.removed_nodes.len())
            });
        Self {
            hashed_accounts: hashed.accounts.len(),
            hashed_storages: hashed.storages.len(),
            hashed_storage_slots,
            largest_storage,
            trie_account_nodes: trie.account_nodes.len(),
            trie_storage_tries: trie.storage_tries.len(),
            trie_storage_nodes,
            trie_removed_nodes: trie.removed_nodes.len() + storage_removed_nodes,
        }
    }
}

/// One composed block's state-root time joined to its contents and metered usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedBlockObservation {
    /// Block number.
    pub block_number: u64,
    /// Sealed block hash.
    pub block_hash: B256,
    /// Transactions in the block, including sequencer transactions.
    pub tx_count: usize,
    /// Gas used by the block.
    pub gas_used: u64,
    /// Time spent on the state root.
    pub state_root: StateRootTiming,
    /// Sizes of the block's state diff and trie updates.
    pub state_diff: StateDiffSizes,
    /// Cumulative resource-metering units, aligned with the schedule dimensions. Empty when
    /// metering is inactive.
    pub resource_usage: Vec<u128>,
}

/// Displays usage as `name=value` pairs joined by commas, treating missing values as zero.
#[derive(Debug, Clone, Copy)]
pub struct DimensionUsage<'a> {
    /// Schedule dimensions, in order.
    pub dimensions: &'a [ResourceMeteringDimension],
    /// Usage aligned with [`Self::dimensions`].
    pub usage: &'a [u128],
}

impl fmt::Display for DimensionUsage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, dimension) in self.dimensions.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            let value = self.usage.get(index).copied().unwrap_or_default();
            write!(f, "{}={value}", dimension.name)?;
        }
        Ok(())
    }
}

/// Emits the observation line at `$level`, with the same static message and fields at every level.
macro_rules! sealed_block_event {
    ($level:expr, $observation:expr, $dimensions:expr, $slow:expr) => {{
        let observation = $observation;
        let diff = &observation.state_diff;
        tracing::event!(
            target: "payload_builder",
            $level,
            block_number = observation.block_number,
            block_hash = %observation.block_hash,
            tx_count = observation.tx_count,
            gas_used = observation.gas_used,
            slow = $slow,
            state_root_source = %observation.state_root.source.as_str(),
            state_root_us = observation.state_root.total_us(),
            state_root_wait_us = observation.state_root.wait_us(),
            hashed_accounts = diff.hashed_accounts,
            hashed_storages = diff.hashed_storages,
            hashed_storage_slots = diff.hashed_storage_slots,
            largest_storage_slots = diff.largest_storage.map_or(0, |largest| largest.slots),
            largest_storage_address =
                diff.largest_storage.map(|largest| tracing::field::display(largest.address)),
            trie_account_nodes = diff.trie_account_nodes,
            trie_storage_tries = diff.trie_storage_tries,
            trie_storage_nodes = diff.trie_storage_nodes,
            trie_removed_nodes = diff.trie_removed_nodes,
            resource_usage = %DimensionUsage {
                dimensions: $dimensions,
                usage: &observation.resource_usage,
            },
            "sealed block observation"
        )
    }};
}

impl SealedBlockObservation {
    /// Emits the observation's structured log line. `dimensions` names [`Self::resource_usage`];
    /// pass none when metering is inactive.
    ///
    /// The line logs at `debug`, or at `warn` with `slow=true` when the state-root time meets or
    /// exceeds [`SLOW_BLOCK_STATE_ROOT`].
    pub fn log(&self, dimensions: &[ResourceMeteringDimension]) {
        if self.state_root.is_slow() {
            sealed_block_event!(Level::WARN, self, dimensions, true);
        } else {
            sealed_block_event!(Level::DEBUG, self, dimensions, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use alloy_primitives::{Address, B256, U256, map::HashMap};
    use reth_trie_common::{
        BranchNodeCompact, HashedPostState, KeccakKeyHasher, Nibbles,
        updates::{StorageTrieUpdates, TrieUpdates},
    };
    use revm::{bytecode::Bytecode, database::BundleState, state::AccountInfo};

    use super::*;
    use crate::ResourceMeteringDimension;

    fn slots(count: u64) -> HashMap<U256, (U256, U256)> {
        (1..=count).map(|slot| (U256::from(slot), (U256::ZERO, U256::from(slot)))).collect()
    }

    fn node() -> BranchNodeCompact {
        BranchNodeCompact::new(0, 0, 0, Vec::new(), None)
    }

    fn nibbles(path: u8) -> Nibbles {
        Nibbles::from_nibbles([path])
    }

    /// Three accounts changed: one wrote three slots, one wrote one slot, and one wrote none.
    fn known_diff() -> (BundleState, HashedPostState, TrieUpdates) {
        let info = Some(AccountInfo { nonce: 1, ..Default::default() });
        let bundle = BundleState::new(
            [
                (Address::repeat_byte(1), None, info.clone(), slots(3)),
                (Address::repeat_byte(2), None, info.clone(), slots(1)),
                (Address::repeat_byte(3), None, info, HashMap::default()),
            ],
            Vec::<Vec<(Address, Option<Option<AccountInfo>>, Vec<(U256, U256)>)>>::new(),
            Vec::<(B256, Bytecode)>::new(),
        );
        let hashed = HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state());
        let trie = TrieUpdates {
            account_nodes: [(nibbles(1), node()), (nibbles(2), node())].into_iter().collect(),
            removed_nodes: [nibbles(3)].into_iter().collect(),
            storage_tries: [
                (
                    B256::repeat_byte(1),
                    StorageTrieUpdates {
                        storage_nodes: [(nibbles(1), node()), (nibbles(2), node())]
                            .into_iter()
                            .collect(),
                        removed_nodes: [nibbles(4), nibbles(5)].into_iter().collect(),
                    },
                ),
                (
                    B256::repeat_byte(2),
                    StorageTrieUpdates {
                        storage_nodes: [(nibbles(1), node())].into_iter().collect(),
                        removed_nodes: Default::default(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        (bundle, hashed, trie)
    }

    #[test]
    fn state_diff_sizes_count_a_known_diff() {
        let (bundle, hashed, trie) = known_diff();

        let sizes = StateDiffSizes::new(&hashed, &trie, &bundle);

        assert_eq!(sizes.hashed_accounts, 3);
        assert_eq!(sizes.hashed_storages, 2);
        assert_eq!(sizes.hashed_storage_slots, 4);
        assert_eq!(
            sizes.largest_storage,
            Some(LargestStorage { address: Address::repeat_byte(1), slots: 3 })
        );
        assert_eq!(sizes.trie_account_nodes, 2);
        assert_eq!(sizes.trie_storage_tries, 2);
        assert_eq!(sizes.trie_storage_nodes, 3);
        assert_eq!(sizes.trie_removed_nodes, 3);
    }

    #[test]
    fn state_diff_sizes_of_an_empty_diff_have_no_largest_storage() {
        let sizes = StateDiffSizes::new(
            &HashedPostState::default(),
            &TrieUpdates::default(),
            &BundleState::default(),
        );

        assert_eq!(sizes, StateDiffSizes::default());
        assert_eq!(sizes.largest_storage, None);
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn dimension(name: &str) -> ResourceMeteringDimension {
        ResourceMeteringDimension {
            name: name.to_string(),
            block_limit: 1_000,
            transaction_limit: 1_000,
            base_gas_weight: 1,
            operations: Vec::new(),
            dry_run: true,
        }
    }

    fn observation(state_root_total: Duration) -> SealedBlockObservation {
        let (bundle, hashed, trie) = known_diff();
        SealedBlockObservation {
            block_number: 7,
            block_hash: B256::repeat_byte(0xab),
            tx_count: 2,
            gas_used: 42_000,
            state_root: StateRootTiming {
                source: StateRootSource::Task,
                wait: Duration::from_micros(250),
                total: state_root_total,
            },
            state_diff: StateDiffSizes::new(&hashed, &trie, &bundle),
            resource_usage: vec![5, 11],
        }
    }

    /// Logs `observation` and returns the observation lines logged at any level.
    fn observation_lines(observation: &SealedBlockObservation) -> Vec<String> {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            observation.log(&[dimension("cpu"), dimension("state_root")]);
        });

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        output
            .lines()
            .filter(|line| line.contains("sealed block observation"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn observation_logs_one_structured_line() {
        let lines = observation_lines(&observation(Duration::from_micros(900)));

        assert_eq!(lines.len(), 1, "expected one observation line, got: {lines:?}");
        let line = &lines[0];
        for field in [
            "block_number=7",
            "tx_count=2",
            "gas_used=42000",
            "state_root_source=task",
            "state_root_us=900",
            "state_root_wait_us=250",
            "hashed_accounts=3",
            "hashed_storages=2",
            "hashed_storage_slots=4",
            "largest_storage_slots=3",
            "largest_storage_address=0x0101010101010101010101010101010101010101",
            "trie_account_nodes=2",
            "trie_storage_tries=2",
            "trie_storage_nodes=3",
            "trie_removed_nodes=3",
            "resource_usage=cpu=5,state_root=11",
        ] {
            assert!(line.contains(field), "expected `{field}` in: {line}");
        }
    }

    #[test]
    fn block_below_slow_threshold_logs_at_debug() {
        let lines =
            observation_lines(&observation(SLOW_BLOCK_STATE_ROOT - Duration::from_micros(1)));

        assert_eq!(lines.len(), 1, "expected one observation line, got: {lines:?}");
        assert!(lines[0].contains(" DEBUG "), "expected a debug line, got: {}", lines[0]);
        assert!(lines[0].contains("slow=false"), "expected slow=false in: {}", lines[0]);
    }

    #[test]
    fn block_at_or_above_slow_threshold_logs_at_warn() {
        for state_root_total in [SLOW_BLOCK_STATE_ROOT, Duration::from_millis(140)] {
            let lines = observation_lines(&observation(state_root_total));

            assert_eq!(lines.len(), 1, "expected one observation line, got: {lines:?}");
            let line = &lines[0];
            assert!(line.contains(" WARN "), "expected a warn line, got: {line}");
            assert!(line.contains("slow=true"), "expected slow=true in: {line}");
            assert!(
                line.contains(&format!("state_root_us={}", state_root_total.as_micros())),
                "the warn line keeps the observation fields: {line}"
            );
        }
    }
}
