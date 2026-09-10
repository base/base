// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

// This basis of this file has been taken from the rust-libp2p codebase:
// https://github.com/libp2p/rust-libp2p
//
use std::{
    collections::btree_map::{BTreeMap, Entry},
    time::{Duration, Instant},
};

use super::*;
use crate::discv5_protocol::{
    config::Config,
    kbucket::{Distance, Key, MAX_NODES_PER_BUCKET},
};

#[derive(Debug, Clone)]
pub struct FindNodeQuery<TNodeId> {
    /// The target key we are looking for
    target_key: Key<TNodeId>,

    /// The current state of progress of the query.
    progress: QueryProgress,

    /// The closest peers to the target, ordered by increasing distance.
    closest_peers: BTreeMap<Distance, QueryPeer<TNodeId>>,

    /// The number of peers for which the query is currently waiting for results.
    num_waiting: usize,

    /// The configuration of the query.
    config: FindNodeQueryConfig,
}

/// Configuration for a `Query`.
#[derive(Debug, Clone)]
pub struct FindNodeQueryConfig {
    /// Allowed level of parallelism.
    ///
    /// The `α` parameter in the Kademlia paper. The maximum number of peers that a query
    /// is allowed to wait for in parallel while iterating towards the closest
    /// nodes to a target. Defaults to `3`.
    pub parallelism: usize,

    /// Number of results to produce.
    ///
    /// The number of closest peers that a query must obtain successful results
    /// for before it terminates. Defaults to the maximum number of entries in a
    /// single k-bucket, i.e. the `k` parameter in the Kademlia paper.
    pub num_results: usize,

    /// The timeout for a single peer.
    ///
    /// If a successful result is not reported for a peer within this timeout
    /// window, the iterator considers the peer unresponsive and will not wait for
    /// the peer when evaluating the termination conditions, until and unless a
    /// result is delivered. Defaults to `10` seconds.
    pub peer_timeout: Duration,
}

impl FindNodeQueryConfig {
    pub fn new_from_config(config: &Config) -> Self {
        Self {
            parallelism: config.query_parallelism,
            num_results: MAX_NODES_PER_BUCKET,
            peer_timeout: config.query_peer_timeout,
        }
    }
}

impl<TNodeId> FindNodeQuery<TNodeId>
where
    TNodeId: Into<Key<TNodeId>> + Eq + Clone,
{
    /// Creates a new query with the given configuration.
    pub fn with_config<I>(
        config: FindNodeQueryConfig,
        target_key: Key<TNodeId>,
        known_closest_peers: I,
    ) -> Self
    where
        I: IntoIterator<Item = Key<TNodeId>>,
    {
        // Initialise the closest peers to begin the query with.
        let closest_peers = known_closest_peers
            .into_iter()
            .map(|key| {
                let key: Key<TNodeId> = key;
                let distance = key.distance(&target_key);
                let state = QueryPeerState::NotContacted;
                (distance, QueryPeer::new(key, state))
            })
            .take(config.num_results)
            .collect();

        // The query initially makes progress by iterating towards the target.
        let progress = QueryProgress::Iterating { no_progress: 0 };

        FindNodeQuery { config, target_key, progress, closest_peers, num_waiting: 0 }
    }

    /// Callback for delivering the result of a successful request to a peer
    /// that the query is waiting on.
    ///
    /// Delivering results of requests back to the query allows the query to make
    /// progress. The query is said to make progress either when the given
    /// `closer_peers` contain a peer closer to the target than any peer seen so far,
    /// or when the query did not yet accumulate `num_results` closest peers and
    /// `closer_peers` contains a new peer, regardless of its distance to the target.
    ///
    /// After calling this function, `next` should eventually be called again
    /// to advance the state of the query.
    ///
    /// If the query is finished, the query is not currently waiting for a
    /// result from `peer`, or a result for `peer` has already been reported,
    /// calling this function has no effect.
    pub fn on_success(&mut self, node_id: &TNodeId, closer_peers: Vec<TNodeId>) {
        if let QueryProgress::Finished = self.progress {
            return;
        }

        let key: Key<TNodeId> = node_id.clone().into();
        let distance = key.distance(&self.target_key);

        // Mark the peer's progress, the total nodes it has returned and it's current iteration.
        // If the node returned peers, mark it as succeeded.
        match self.closest_peers.entry(distance) {
            Entry::Vacant(..) => return,
            Entry::Occupied(mut e) => match e.get().state {
                QueryPeerState::Waiting(..) => {
                    debug_assert!(self.num_waiting > 0);
                    self.num_waiting -= 1;
                    let peer = e.get_mut();
                    peer.peers_returned += closer_peers.len();
                    // mark the peer as succeeded
                    peer.state = QueryPeerState::Succeeded;
                }
                QueryPeerState::Unresponsive => {
                    let peer = e.get_mut();
                    peer.peers_returned += closer_peers.len();
                    // mark the peer as succeeded
                    peer.state = QueryPeerState::Succeeded;
                }
                QueryPeerState::NotContacted
                | QueryPeerState::Failed
                | QueryPeerState::Succeeded => return,
            },
        }

        let mut progress = false;
        let num_closest = self.closest_peers.len();

        // Incorporate the reported closer peers into the query.
        for peer in closer_peers {
            let key: Key<TNodeId> = peer.into();
            let distance = self.target_key.distance(&key);
            let peer = QueryPeer::new(key, QueryPeerState::NotContacted);
            self.closest_peers.entry(distance).or_insert(peer);
            // The query makes progress if the new peer is either closer to the target
            // than any peer seen so far (i.e. is the first entry), or the query did
            // not yet accumulate enough closest peers.
            progress = self.closest_peers.keys().next() == Some(&distance)
                || num_closest < self.config.num_results;
        }

        // Update the query progress.
        self.progress = match self.progress {
            QueryProgress::Iterating { no_progress } => {
                let no_progress = if progress { 0 } else { no_progress + 1 };
                if no_progress >= self.config.parallelism {
                    QueryProgress::Stalled
                } else {
                    QueryProgress::Iterating { no_progress }
                }
            }
            QueryProgress::Stalled => {
                if progress {
                    QueryProgress::Iterating { no_progress: 0 }
                } else {
                    QueryProgress::Stalled
                }
            }
            QueryProgress::Finished => QueryProgress::Finished,
        }
    }

    /// Callback for informing the query about a failed request to a peer
    /// that the query is waiting on.
    ///
    /// After calling this function, `next` should eventually be called again
    /// to advance the state of the query.
    ///
    /// If the query is finished, the query is not currently waiting for a
    /// result from `peer`, or a result for `peer` has already been reported,
    /// calling this function has no effect.
    pub fn on_failure(&mut self, peer: &TNodeId) {
        if let QueryProgress::Finished = self.progress {
            return;
        }

        let key: Key<TNodeId> = peer.clone().into();
        let distance = key.distance(&self.target_key);

        match self.closest_peers.entry(distance) {
            Entry::Vacant(_) => {}
            Entry::Occupied(mut e) => match e.get().state {
                QueryPeerState::Waiting(..) => {
                    debug_assert!(self.num_waiting > 0);
                    self.num_waiting -= 1;
                    e.get_mut().state = QueryPeerState::Failed
                }
                QueryPeerState::Unresponsive => e.get_mut().state = QueryPeerState::Failed,
                _ => {}
            },
        }
    }

    /// Advances the state of the query, potentially getting a new peer to contact.
    ///
    /// See [`QueryState`].
    pub fn next(&mut self, now: Instant) -> QueryState<TNodeId> {
        if let QueryProgress::Finished = self.progress {
            return QueryState::Finished;
        }

        // Count the number of peers that returned a result. If there is a
        // request in progress to one of the `num_results` closest peers, the
        // counter is set to `None` as the query can only finish once
        // `num_results` closest peers have responded (or there are no more
        // peers to contact, see `active_counter`).
        let mut result_counter = Some(0);

        // Check if the query is at capacity w.r.t. the allowed parallelism.
        let at_capacity = self.at_capacity();

        for peer in self.closest_peers.values_mut() {
            match peer.state {
                QueryPeerState::NotContacted => {
                    // This peer is waiting to be reiterated.
                    if !at_capacity {
                        let timeout = now + self.config.peer_timeout;
                        peer.state = QueryPeerState::Waiting(timeout);
                        self.num_waiting += 1;
                        let peer = peer.key.preimage().clone();
                        return QueryState::Waiting(Some(peer));
                    } else {
                        return QueryState::WaitingAtCapacity;
                    }
                }

                QueryPeerState::Waiting(timeout) => {
                    if now >= timeout {
                        // Peers that don't respond within timeout are set to `Failed`.
                        debug_assert!(self.num_waiting > 0);
                        self.num_waiting -= 1;
                        peer.state = QueryPeerState::Unresponsive;
                    } else if at_capacity {
                        // The query is still waiting for a result from a peer and is
                        // at capacity w.r.t. the maximum number of peers being waited on.
                        return QueryState::WaitingAtCapacity;
                    } else {
                        // The query is still waiting for a result from a peer and the
                        // `result_counter` did not yet reach `num_results`. Therefore
                        // the query is not yet done, regardless of already successful
                        // queries to peers farther from the target.
                        result_counter = None;
                    }
                }

                QueryPeerState::Succeeded => {
                    if let Some(ref mut cnt) = result_counter {
                        *cnt += 1;
                        // If `num_results` successful results have been delivered for the
                        // closest peers, the query is done.
                        if *cnt >= self.config.num_results {
                            self.progress = QueryProgress::Finished;
                            return QueryState::Finished;
                        }
                    }
                }

                QueryPeerState::Failed | QueryPeerState::Unresponsive => {
                    // Skip over unresponsive or failed peers.
                }
            }
        }

        if self.num_waiting > 0 {
            // The query is still waiting for results and not at capacity w.r.t.
            // the allowed parallelism, but there are no new peers to contact
            // at the moment.
            QueryState::Waiting(None)
        } else {
            // The query is finished because all available peers have been contacted
            // and the query is not waiting for any more results.
            self.progress = QueryProgress::Finished;
            QueryState::Finished
        }
    }

    /// Consumes the query, returning the target and the closest peers.
    pub fn into_result(self) -> Vec<TNodeId> {
        self.closest_peers
            .into_values()
            .filter_map(|peer| {
                if let QueryPeerState::Succeeded = peer.state {
                    Some(peer.key.into_preimage())
                } else {
                    None
                }
            })
            .take(self.config.num_results)
            .collect()
    }

    /// Checks if the query is at capacity w.r.t. the permitted parallelism.
    ///
    /// While the query is stalled, up to `num_results` parallel requests
    /// are allowed. This is a slightly more permissive variant of the
    /// requirement that the initiator "resends the FIND_NODE to all of the
    /// k closest nodes it has not already queried".
    fn at_capacity(&self) -> bool {
        match self.progress {
            QueryProgress::Stalled => self.num_waiting >= self.config.num_results,
            QueryProgress::Iterating { .. } => self.num_waiting >= self.config.parallelism,
            QueryProgress::Finished => true,
        }
    }
}

/// Stage of the query.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum QueryProgress {
    /// The query is making progress by iterating towards `num_results` closest
    /// peers to the target with a maximum of `parallelism` peers for which the
    /// query is waiting for results at a time.
    ///
    /// > **Note**: When the query switches back to `Iterating` after being
    /// > `Stalled`, it may temporarily be waiting for more than `parallelism`
    /// > results from peers, with new peers only being considered once
    /// > the number pending results drops below `parallelism`.
    Iterating {
        /// The number of consecutive results that did not yield a peer closer
        /// to the target. When this number reaches `parallelism` and no new
        /// peer was discovered or at least `num_results` peers are known to
        /// the query, it is considered `Stalled`.
        no_progress: usize,
    },

    /// A query is stalled when it did not make progress after `parallelism`
    /// consecutive successful results (see `on_success`).
    ///
    /// While the query is stalled, the maximum allowed parallelism for pending
    /// results is increased to `num_results` in an attempt to finish the query.
    /// If the query can make progress again upon receiving the remaining
    /// results, it switches back to `Iterating`. Otherwise it will be finished.
    Stalled,

    /// The query is finished.
    ///
    /// A query finishes either when it has collected `num_results` results
    /// from the closest peers (not counting those that failed or are unresponsive)
    /// or because the query ran out of peers that have not yet delivered
    /// results (or failed).
    Finished,
}

/// Representation of a peer in the context of a query.
#[derive(Debug, Clone)]
pub struct QueryPeer<TNodeId> {
    /// The `KBucket` key used to identify the peer.
    key: Key<TNodeId>,

    /// The number of peers that have been returned by this peer.
    peers_returned: usize,

    /// The current query state of this peer.
    state: QueryPeerState,
}

impl<TNodeId> QueryPeer<TNodeId> {
    pub fn new(key: Key<TNodeId>, state: QueryPeerState) -> Self {
        QueryPeer { key, peers_returned: 0, state }
    }
}

/// The state of `QueryPeer` in the context of a query.
#[derive(Debug, Copy, Clone)]
pub enum QueryPeerState {
    /// The peer has not yet been contacted.
    ///
    /// This is the starting state for every peer known to, or discovered by, a query.
    NotContacted,

    /// The query is waiting for a result from the peer.
    Waiting(Instant),

    /// A result was not delivered for the peer within the configured timeout.
    ///
    /// The peer is not taken into account for the termination conditions
    /// of the iterator until and unless it responds.
    Unresponsive,

    /// Obtaining a result from the peer has failed.
    ///
    /// This is a final state, reached as a result of a call to `on_failure`.
    Failed,

    /// A successful result from the peer has been delivered.
    ///
    /// This is a final state, reached as a result of a call to `on_success`.
    Succeeded,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use enr::NodeId;
    use quickcheck::*;
    use rand_0_7::{Rng, thread_rng};

    use super::*;

    type TestQuery = FindNodeQuery<NodeId>;

    fn random_nodes(n: usize) -> impl Iterator<Item = NodeId> + Clone {
        (0..n).map(|_| NodeId::random())
    }

    fn random_query<G: Rng>(g: &mut G) -> TestQuery {
        let known_closest_peers = random_nodes(g.gen_range(1, 60)).map(Key::from);
        let target = NodeId::random();
        let config = FindNodeQueryConfig {
            parallelism: g.gen_range(1, 10),
            num_results: g.gen_range(1, 25),
            peer_timeout: Duration::from_secs(g.gen_range(10, 30)),
        };
        FindNodeQuery::with_config(config, target.into(), known_closest_peers)
    }

    fn sorted(target: &Key<NodeId>, peers: &[Key<NodeId>]) -> bool {
        peers.windows(2).all(|w| w[0].distance(target) < w[1].distance(target))
    }

    impl Arbitrary for TestQuery {
        fn arbitrary<G: Gen>(g: &mut G) -> TestQuery {
            random_query(g)
        }
    }

    #[test]
    fn new_query() {
        let query = random_query(&mut thread_rng());
        let target = query.target_key.clone();

        let (keys, states): (Vec<_>, Vec<_>) =
            query.closest_peers.values().map(|e| (e.key.clone(), &e.state)).unzip();

        let none_contacted = states.iter().all(|s| matches!(s, QueryPeerState::NotContacted));

        assert!(none_contacted, "Unexpected peer state in new query.");
        assert!(
            sorted(&target, &keys),
            "Closest peers in new query not sorted by distance to target."
        );
        assert_eq!(query.num_waiting, 0, "Unexpected peers in progress in new query.");
        assert!(query.into_result().is_empty(), "Unexpected closest peers in new query");
    }

    #[test]
    fn termination_and_parallelism() {
        fn prop(mut query: TestQuery) {
            let now = Instant::now();
            let mut rng = thread_rng();

            let mut expected =
                query.closest_peers.values().map(|e| e.key.clone()).collect::<Vec<_>>();
            let num_known = expected.len();
            let max_parallelism = usize::min(query.config.parallelism, num_known);

            let target = query.target_key.clone();
            let mut remaining;
            let mut num_failures = 0;

            'finished: loop {
                if expected.is_empty() {
                    break;
                }
                // Split off the next up to `parallelism` expected peers.
                else if expected.len() < max_parallelism {
                    remaining = Vec::new();
                } else {
                    remaining = expected.split_off(max_parallelism);
                }

                // Advance the query for maximum parallelism.
                for k in expected.iter() {
                    match query.next(now) {
                        QueryState::Finished => break 'finished,
                        QueryState::Waiting(Some(p)) => assert_eq!(&p, k.preimage()),
                        QueryState::Waiting(None) => panic!("Expected another peer."),
                        QueryState::WaitingAtCapacity => panic!("Unexpectedly reached capacity."),
                    }
                }
                let num_waiting = query.num_waiting;
                assert_eq!(num_waiting, expected.len());

                // Check the bounded parallelism.
                if query.at_capacity() {
                    assert_eq!(query.next(now), QueryState::WaitingAtCapacity)
                }

                // Report results back to the query with a random number of "closer"
                // peers or an error, thus finishing the "in-flight requests".
                for (i, k) in expected.iter().enumerate() {
                    if rng.gen_bool(0.75) {
                        let num_closer = rng.gen_range(0, query.config.num_results + 1);
                        let closer_peers = random_nodes(num_closer).collect::<Vec<_>>();
                        // let _: () = remaining;
                        remaining.extend(closer_peers.iter().map(|x| Key::from(*x)));
                        query.on_success(k.preimage(), closer_peers);
                    } else {
                        num_failures += 1;
                        query.on_failure(k.preimage());
                    }
                    assert_eq!(query.num_waiting, num_waiting - (i + 1));
                }

                // Re-sort the remaining expected peers for the next "round".
                remaining.sort_by_key(|k| target.distance(k));

                expected = remaining
            }

            // The query must be finished.
            assert_eq!(query.next(now), QueryState::Finished);
            assert_eq!(query.progress, QueryProgress::Finished);

            // Determine if all peers have been contacted by the query. This _must_ be
            // the case if the query finished with fewer than the requested number
            // of results.
            let all_contacted = query.closest_peers.values().all(|e| {
                !matches!(e.state, QueryPeerState::NotContacted | QueryPeerState::Waiting { .. })
            });

            let target_key = query.target_key.clone();
            let num_results = query.config.num_results;
            let result = query.into_result();
            let closest = result.into_iter().map(Key::from).collect::<Vec<_>>();

            // assert_eq!(result.target, target);
            assert!(sorted(&target_key, &closest));

            if closest.len() < num_results {
                // The query returned fewer results than requested. Therefore
                // either the initial number of known peers must have been
                // less than the desired number of results, or there must
                // have been failures.
                assert!(num_known < num_results || num_failures > 0);
                // All peers must have been contacted.
                assert!(all_contacted, "Not all peers have been contacted.");
            } else {
                assert_eq!(num_results, closest.len(), "Too  many results.");
            }
        }

        QuickCheck::new().tests(10).quickcheck(prop as fn(_) -> _)
    }

    #[test]
    fn no_duplicates() {
        fn prop(mut query: TestQuery) -> bool {
            let now = Instant::now();
            let closer: Vec<NodeId> = random_nodes(1).collect();

            // A first peer reports a "closer" peer.
            let peer1 = if let QueryState::Waiting(Some(p)) = query.next(now) {
                p
            } else {
                panic!("No peer.");
            };
            query.on_success(&peer1, closer.clone());
            // Duplicate result from the same peer.
            query.on_success(&peer1, closer.clone());

            // If there is a second peer, let it also report the same "closer" peer.
            match query.next(now) {
                QueryState::Waiting(Some(p)) => {
                    let peer2 = p;
                    query.on_success(&peer2, closer.clone())
                }
                QueryState::Finished => {}
                _ => panic!("Unexpectedly query state."),
            };

            // The "closer" peer must only be in the query once.
            let n = query.closest_peers.values().filter(|e| e.key.preimage() == &closer[0]).count();
            assert_eq!(n, 1);

            true
        }

        QuickCheck::new().tests(10).quickcheck(prop as fn(_) -> _)
    }

    #[test]
    fn timeout() {
        fn prop(mut query: TestQuery) -> bool {
            let mut now = Instant::now();
            let peer = query.closest_peers.values().next().unwrap().key.clone().into_preimage();
            // Poll the query for the first peer to be in progress.
            match query.next(now) {
                QueryState::Waiting(Some(id)) => assert_eq!(id, peer),
                _ => panic!(),
            }

            // Artificially advance the clock.
            now += query.config.peer_timeout;

            // Advancing the query again should mark the first peer as unresponsive.
            let _ = query.next(now);
            match &query.closest_peers.values().next().unwrap() {
                QueryPeer { key, state: QueryPeerState::Unresponsive, .. } => {
                    assert_eq!(key.preimage(), &peer);
                }
                QueryPeer { state, .. } => panic!("Unexpected peer state: {:?}", state),
            }

            let finished = query.progress == QueryProgress::Finished;
            query.on_success(&peer, Vec::<NodeId>::new());
            let closest = query.into_result();

            if finished {
                // Delivering results when the query already finished must have
                // no effect.
                assert_eq!(Vec::<NodeId>::new(), closest);
            } else {
                // Unresponsive peers can still deliver results while the iterator
                // is not finished.
                assert_eq!(vec![peer], closest)
            }
            true
        }

        QuickCheck::new().tests(10).quickcheck(prop as fn(_) -> _)
    }
}
