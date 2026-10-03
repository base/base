use std::{
    fmt, process,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

/// Builder for deterministic event IDs.
#[derive(Debug, Clone)]
pub struct EventIdBuilder {
    hasher: Sha256,
}

impl EventIdBuilder {
    /// Creates an empty event ID builder.
    pub fn new() -> Self {
        Self { hasher: Sha256::new() }
    }

    /// Adds a stable component to the ID hash.
    pub fn part(mut self, name: &str, value: impl fmt::Display) -> Self {
        let value = value.to_string();
        self.hasher.update(name.as_bytes());
        self.hasher.update([0]);
        self.hasher.update(value.len().to_le_bytes());
        self.hasher.update(value.as_bytes());
        self.hasher.update([0xff]);
        self
    }

    /// Finalizes the event ID as a hex-encoded SHA-256 digest.
    pub fn finish(self) -> String {
        format!("0x{}", hex::encode(self.hasher.finalize()))
    }
}

impl Default for EventIdBuilder {
    fn default() -> Self {
        Self::new()
    }
}

static PROCESS_INSTANCE: OnceLock<u128> = OnceLock::new();
static OCCURRENCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Identity of one producer observation.
///
/// Event IDs are content hashes, so two observations built from identical ID parts share an ID
/// and downstream ingest keeps only the first. Emitters whose event type records *each* time
/// something happened (an RPC admission, a queue hand-off, a forward attempt) add an occurrence
/// so separate observations stay separate even when their semantic inputs match.
///
/// An occurrence pairs a random per-process instance with a process-wide monotonic sequence.
/// The instance distinguishes replicas and restarts; the sequence distinguishes emissions within
/// one process, including concurrent ones. The occurrence is hashed into the event ID once, when
/// the event is built, so transport retries and replays of the serialized event keep the same ID
/// and still deduplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventOccurrence {
    instance: u128,
    sequence: u64,
}

impl EventOccurrence {
    /// Creates an occurrence from explicit parts, for deterministic tests and replays.
    pub const fn new(instance: u128, sequence: u64) -> Self {
        Self { instance, sequence }
    }

    /// Allocates the next occurrence for this process.
    pub fn next() -> Self {
        Self::new(Self::process_instance(), OCCURRENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the random identifier chosen for this process on first use.
    ///
    /// Falls back to wall-clock time and process ID if the OS random source is unavailable, so
    /// event emission never fails on this path.
    pub fn process_instance() -> u128 {
        *PROCESS_INSTANCE.get_or_init(|| {
            let mut bytes = [0_u8; 16];
            if getrandom::fill(&mut bytes).is_ok() {
                return u128::from_le_bytes(bytes);
            }
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            nanos ^ (u128::from(process::id()) << 96)
        })
    }

    /// Returns the process instance this occurrence belongs to.
    pub const fn instance(&self) -> u128 {
        self.instance
    }

    /// Returns the position of this occurrence within its process instance.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, thread};

    use super::*;

    #[test]
    fn next_occurrences_share_the_process_instance_and_never_repeat() {
        let first = EventOccurrence::next();
        let second = EventOccurrence::next();

        assert_eq!(first.instance(), second.instance());
        assert_eq!(first.instance(), EventOccurrence::process_instance());
        assert_ne!(first, second);
    }

    #[test]
    fn concurrent_occurrences_are_unique() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 1_000;

        let occurrences: Vec<EventOccurrence> = thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        (0..PER_THREAD).map(|_| EventOccurrence::next()).collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect()
        });

        let unique: HashSet<_> = occurrences.iter().collect();
        assert_eq!(unique.len(), THREADS * PER_THREAD);
    }
}
