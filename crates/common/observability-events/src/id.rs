/// Random transaction event identifier.
///
/// Every emitted event gets its own ID, chosen when the event is built: 32 bytes from the
/// thread-local CSPRNG, hex-encoded with a `0x` prefix. The ID identifies one emission, not
/// the fact it describes. Two emissions about the same transaction always get different IDs,
/// so downstream ingest never discards one observation as a duplicate of another. Collector
/// retries and journal replays resend the serialized event, so they keep its ID and ingest
/// still drops the redelivered copy.
#[derive(Debug, Clone, Copy)]
pub struct EventId;

impl EventId {
    /// Returns a new random event ID.
    pub fn random() -> String {
        format!("0x{}", hex::encode(rand::random::<[u8; 32]>()))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, thread};

    use super::*;

    #[test]
    fn random_ids_keep_the_existing_wire_format() {
        let id = EventId::random();

        assert_eq!(id.len(), 66);
        assert!(id.starts_with("0x"));
        assert!(id[2..].bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
    }

    #[test]
    fn concurrent_random_ids_are_unique() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 1_000;

        let ids: Vec<String> = thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| (0..PER_THREAD).map(|_| EventId::random()).collect::<Vec<_>>())
                })
                .collect();
            handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect()
        });

        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), THREADS * PER_THREAD);
    }
}
