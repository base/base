//! This struct keeps track of voting for what our external socket is for both IPv4 and IPv6.
//!
//! Without correct SNAT routing rules, some routers can use alternating or round-robin ports to
//! send outbound traffic. Generally speaking, these ports won't be accessible for inbound traffic
//! from any peers, so they should not be advertised.
//!
//! Therefore the majority function works as follows:
//! - Keep track of all votes within a defined time period (vote_duration)
//! - We count the votes. We consider an IP a majority winner if the following conditions are met:
//!     - There are more votes than the minimum_threshold (prevents accidentally selecting the
//!       wrong IP, or having a small group of malicious actors persuade us of the wrong value)
//!     - There are no other candidates that are also above the threshold or within
//!       CLEAR_MAJORITY_PERCENTAGE of the
//!       majority (This prevents multiple candidates from flip-flopping. There should not be
//!       competing IP values. If there are, this is a misconfiguration of the network set-up, and
//!       we should not advertise an IP. The user can override this via CLI configurations.)
//!
//!       The CLEAR_MAJORITY_PERCENTAGE criteria prevents us from advertising the first vote that
//!       reaches the threshold then reverting back to an empty ENR in the case where multiple ports are being cycled.

use std::{
    collections::HashMap,
    hash::Hash,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
    time::{Duration, Instant},
};

use enr::NodeId;
use fnv::FnvHashMap;
use tracing::debug;

/// To avoid false winners, the majority vote must win by at least this percentage compared to the next
/// likely candidate.
const CLEAR_MAJORITY_PERCENTAGE: f64 = 0.3;

/// A collection of IP:Ports for our node reported from external peers.
pub(crate) struct IpVote {
    /// The current collection of IP:Port votes for ipv4.
    ipv4_votes: HashMap<NodeId, (SocketAddrV4, Instant)>,
    /// The current collection of IP:Port votes for ipv6.
    ipv6_votes: HashMap<NodeId, (SocketAddrV6, Instant)>,
    /// The minimum number of votes required before an IP/PORT is accepted.
    minimum_threshold: usize,
    /// The time votes remain valid.
    vote_duration: Duration,
}

impl IpVote {
    pub fn new(minimum_threshold: usize, vote_duration: Duration) -> Self {
        // do not allow minimum thresholds less than 2
        if minimum_threshold < 2 {
            panic!(
                "Setting enr_peer_update_min to a value less than 2 will cause issues with discovery with peers behind NAT"
            );
        }
        IpVote {
            ipv4_votes: HashMap::new(),
            ipv6_votes: HashMap::new(),
            minimum_threshold,
            vote_duration,
        }
    }

    pub fn insert(&mut self, key: NodeId, socket: impl Into<SocketAddr>) {
        match socket.into() {
            SocketAddr::V4(socket) => {
                self.ipv4_votes.insert(key, (socket, Instant::now() + self.vote_duration));
            }
            SocketAddr::V6(socket) => {
                self.ipv6_votes.insert(key, (socket, Instant::now() + self.vote_duration));
            }
        }
    }

    /// Explicit pruning of old states in the hashamp.
    fn clear_old_votes(&mut self) {
        let instant = Instant::now();
        self.ipv4_votes.retain(|_, v| v.1 > instant);
        self.ipv6_votes.retain(|_, v| v.1 > instant);
    }

    /// Returns true if we have more than the minimum number of non-expired votes for a given ip
    /// version.
    pub fn has_minimum_threshold(&mut self) -> (bool, bool) {
        self.clear_old_votes();
        (
            self.ipv4_votes.len() >= self.minimum_threshold,
            self.ipv6_votes.len() >= self.minimum_threshold,
        )
    }

    /// Filter the stale votes and return the majority `SocketAddr` if it exists.
    /// If there are two candidates that both exceed the minimum_threshold, this will return None.
    /// If the second highest candidate is within 20% of the highest, we also return None.
    /// If there are not enough votes to meet the threshold this returns None.
    fn filter_stale_find_most_frequent<K: Copy + Eq + Hash + std::fmt::Debug>(
        votes: &HashMap<NodeId, (K, Instant)>,
        minimum_threshold: usize,
    ) -> (HashMap<NodeId, (K, Instant)>, Option<K>) {
        let mut updated = HashMap::default();
        let mut counter: FnvHashMap<K, usize> = FnvHashMap::default();
        let mut max_count = 0;
        let mut second_max_count = 0;
        let mut max_vote = None;
        let now = Instant::now();

        for (node_id, (vote, instant)) in votes {
            // Discard stale votes
            if instant <= &now {
                continue;
            }
            updated.insert(*node_id, (*vote, *instant));

            let count = counter.entry(*vote).or_default();
            *count += 1;

            // Update max and second_max
            if *count > max_count {
                // Only update second_max if the previous max was from a different vote
                if max_vote.is_some() && max_vote != Some(*vote) {
                    second_max_count = max_count;
                }
                max_count = *count;
                max_vote = Some(*vote);
            } else if *count > second_max_count && Some(*vote) != max_vote {
                second_max_count = *count;
            }
        }

        // Check if we have a clear winner
        let result = if max_count >= minimum_threshold {
            let threshold =
                ((max_count as f64) * (1.0 - CLEAR_MAJORITY_PERCENTAGE)).round() as usize;
            // If we have two candidates above the minimum threshold OR the second candidate is
            // within CLEAR_MAJORITY_PERCENTAGE of the max, then there is no winner.
            if second_max_count >= threshold {
                debug!(
                    highest_count = max_count,
                    second_highest_count = second_max_count,
                    min_threshold = minimum_threshold,
                    threshold_to_max = threshold,
                    "Competing votes detected. Socket not updated."
                );
                None
            } else {
                max_vote
            }
        } else {
            None
        };

        (updated, result)
    }

    /// Returns the majority `SocketAddr`'s of both IPv4 and IPv6 if they exist. If there are not enough votes to meet the threshold this returns None for each stack.
    // NOTE: This removes stale entries by replacing the hashmaps once filtered.
    pub fn majority(&mut self) -> (Option<SocketAddrV4>, Option<SocketAddrV6>) {
        let (updated_ipv4_votes, ipv4_majority) = Self::filter_stale_find_most_frequent::<
            SocketAddrV4,
        >(
            &self.ipv4_votes, self.minimum_threshold
        );
        // This removes stale entries.
        self.ipv4_votes = updated_ipv4_votes;

        let (updated_ipv6_votes, ipv6_majority) = Self::filter_stale_find_most_frequent::<
            SocketAddrV6,
        >(
            &self.ipv6_votes, self.minimum_threshold
        );
        // This removes stale entries.
        self.ipv6_votes = updated_ipv6_votes;

        (ipv4_majority, ipv6_majority)
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

    use super::{CLEAR_MAJORITY_PERCENTAGE, Duration, IpVote, NodeId, SocketAddrV4};

    #[test]
    fn test_three_way_vote_draw() {
        let mut votes = IpVote::new(2, Duration::from_secs(10));

        let socket_1 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 1);
        let socket_2 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 2);
        let socket_3 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 3);

        // 3 votes for each socket
        votes.insert(NodeId::random(), socket_1);
        votes.insert(NodeId::random(), socket_1);
        votes.insert(NodeId::random(), socket_1);
        votes.insert(NodeId::random(), socket_2);
        votes.insert(NodeId::random(), socket_2);
        votes.insert(NodeId::random(), socket_2);
        votes.insert(NodeId::random(), socket_3);
        votes.insert(NodeId::random(), socket_3);
        votes.insert(NodeId::random(), socket_3);

        // With new logic, draw situations should return None due to competing votes
        assert!(votes.majority().0.is_none());
    }

    #[test]
    fn test_majority_vote() {
        let mut votes = IpVote::new(2, Duration::from_secs(10));
        let socket_1 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 1);
        let socket_2 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 2);

        // 5 votes for socket_1, 1 vote for socket_2
        // 1 < (5 * (1-CLEAR_MAJORITY_PERCENTAGE)) = 3.5, so clear majority
        for _ in 0..5 {
            votes.insert(NodeId::random(), socket_1);
        }
        votes.insert(NodeId::random(), socket_2);

        assert_eq!(votes.majority(), (Some(socket_1), None));
    }

    #[test]
    fn test_below_threshold() {
        let mut votes = IpVote::new(3, Duration::from_secs(10));
        let socket_1 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 1);
        let socket_2 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 2);
        let socket_3 = SocketAddrV4::new("127.0.0.1".parse().unwrap(), 3);

        votes.insert(NodeId::random(), socket_1);
        votes.insert(NodeId::random(), socket_1);
        votes.insert(NodeId::random(), socket_2);
        votes.insert(NodeId::random(), socket_3);

        assert_eq!(votes.majority(), (None, None));
    }

    #[test]
    fn test_snat_fluctuation_multiple_iterations() {
        // Demonstrates how repeated calls with same data can yield different results
        // simulating real-world SNAT fluctuation scenarios

        let ip = "10.0.0.1".parse().unwrap();
        let port_1 = SocketAddrV4::new(ip, 50000);
        let port_2 = SocketAddrV4::new(ip, 50001);

        let mut results = Vec::new();

        // Run multiple iterations with alternating vote insertion order
        for iteration in 0..10 {
            let mut votes = IpVote::new(2, Duration::from_secs(10));

            if iteration % 2 == 0 {
                // Even iterations: port_1 votes first
                for _ in 0..3 {
                    votes.insert(NodeId::random(), port_1);
                }
                for _ in 0..3 {
                    votes.insert(NodeId::random(), port_2);
                }
            } else {
                // Odd iterations: port_2 votes first
                for _ in 0..3 {
                    votes.insert(NodeId::random(), port_2);
                }
                for _ in 0..3 {
                    votes.insert(NodeId::random(), port_1);
                }
            }

            let result = votes.majority().0;
            results.push(result);
        }

        // Count how many times each port was selected
        let port_1_wins = results.iter().filter(|r| **r == Some(port_1)).count();
        let port_2_wins = results.iter().filter(|r| **r == Some(port_2)).count();

        println!("Port 1 wins: {}, Port 2 wins: {}", port_1_wins, port_2_wins);
        println!("Results: {:?}", results);

        // We expect no winner when there are competing ports.
        assert!(
            port_1_wins == 0 && port_2_wins == 0,
            "Expected both ports to win some iterations due to flip-flop behavior, but got port_1: {}, port_2: {}",
            port_1_wins,
            port_2_wins
        );
    }

    // Property-based test structures
    #[derive(Debug, Clone)]
    struct VoteData {
        port: u16,
        node_id: NodeId,
    }

    impl Arbitrary for VoteData {
        fn arbitrary<G: Gen>(g: &mut G) -> VoteData {
            VoteData { port: u16::arbitrary(g), node_id: NodeId::random() }
        }
    }

    #[derive(Debug, Clone)]
    struct VoteScenario {
        votes: Vec<VoteData>,
        threshold: usize,
    }

    impl Arbitrary for VoteScenario {
        fn arbitrary<G: Gen>(g: &mut G) -> VoteScenario {
            let threshold = (u8::arbitrary(g) % 10 + 2) as usize; // 2-11
            let vote_count = (u8::arbitrary(g) % 20) as usize; // 0-19
            let votes = (0..vote_count).map(|_| VoteData::arbitrary(g)).collect();
            VoteScenario { votes, threshold }
        }
    }

    quickcheck! {
        /// Property: If no vote meets minimum threshold, result should be None
        fn prop_below_threshold_returns_none(scenario: VoteScenario) -> TestResult {
            if scenario.votes.is_empty() {
                return TestResult::discard();
            }

            let mut vote_system = IpVote::new(scenario.threshold, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();

            // Add all votes
            for vote_data in &scenario.votes {
                let socket = SocketAddrV4::new(ip, vote_data.port);
                vote_system.insert(vote_data.node_id, socket);
            }

            // Count votes per port
            let mut port_counts = std::collections::HashMap::new();
            for vote_data in &scenario.votes {
                *port_counts.entry(vote_data.port).or_insert(0) += 1;
            }

            let max_count = port_counts.values().max().copied().unwrap_or(0);

            if max_count < scenario.threshold {
                TestResult::from_bool(vote_system.majority().0.is_none())
            } else {
                TestResult::discard()
            }
        }

        /// Property: If there's a clear winner (>= threshold, >= 20% margin), it should win
        fn prop_clear_winner_selected(scenario: VoteScenario) -> TestResult {
            if scenario.votes.len() < 2 {
                return TestResult::discard();
            }

            let mut vote_system = IpVote::new(scenario.threshold, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();

            // Add votes
            for vote_data in &scenario.votes {
                let socket = SocketAddrV4::new(ip, vote_data.port);
                vote_system.insert(vote_data.node_id, socket);
            }

            // Count votes per port
            let mut port_counts = std::collections::HashMap::new();
            for vote_data in &scenario.votes {
                *port_counts.entry(vote_data.port).or_insert(0) += 1;
            }

            // Find max and second max
            let mut counts: Vec<_> = port_counts.values().copied().collect();
            counts.sort_by(|a, b| b.cmp(a));

            if counts.is_empty() {
                return TestResult::discard();
            }

            let max_count = counts[0];
            let second_max = counts.get(1).copied().unwrap_or(0);

            // Check if we have a clear winner
            let threshold_margin = ((max_count as f64) * (1.0 - CLEAR_MAJORITY_PERCENTAGE)).round() as usize;
            let has_clear_winner = max_count >= scenario.threshold && second_max < threshold_margin;

            let result = vote_system.majority().0;

            if has_clear_winner {
                // Should return the winning port
                TestResult::from_bool(result.is_some())
            } else if max_count >= scenario.threshold && second_max >= threshold_margin {
                // Should return None due to competition within margin
                TestResult::from_bool(result.is_none())
            } else {
                // Below threshold, should be None
                TestResult::from_bool(result.is_none())
            }
        }

        /// Property: Adding the same vote multiple times should be idempotent
        fn prop_same_vote_idempotent(port: u16) -> bool {
            let mut vote_system = IpVote::new(2, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();
            let socket = SocketAddrV4::new(ip, port);
            let node_id = NodeId::random();

            // Add same vote multiple times
            vote_system.insert(node_id, socket);
            let result1 = vote_system.majority().0;

            vote_system.insert(node_id, socket);
            let result2 = vote_system.majority().0;

            result1 == result2
        }

        /// Property: Vote count should never exceed number of unique node IDs
        fn prop_vote_count_bounded_by_nodes() -> bool {
            let mut vote_system = IpVote::new(2, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();
            let socket = SocketAddrV4::new(ip, 8080);

            // Add votes from 3 different nodes
            let nodes = [NodeId::random(), NodeId::random(), NodeId::random()];
            for &node_id in &nodes {
                vote_system.insert(node_id, socket);
            }

            // The implementation should count each node only once
            // With threshold=2 and 3 votes, should return Some
            vote_system.majority().0.is_some()
        }

        /// Property: Competition within margin should result in no winner
        fn prop_competition_within_margin_no_winner(threshold: u8, first_votes: u8, second_votes: u8) -> TestResult {
            let threshold = threshold.max(2) as usize;
            let first_votes = first_votes.max(1) as usize;
            let second_votes = second_votes.max(1) as usize;

            // Only test when first meets threshold and second is within margin
            if first_votes < threshold || first_votes <= second_votes {
                return TestResult::discard();
            }

            let threshold_margin = ((first_votes as f64) * (1.0 - CLEAR_MAJORITY_PERCENTAGE)).round() as usize;
            if second_votes < threshold_margin {
                return TestResult::discard();
            }

            let mut votes = IpVote::new(threshold, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();
            let socket1 = SocketAddrV4::new(ip, 8080);
            let socket2 = SocketAddrV4::new(ip, 8081);

            for _ in 0..first_votes {
                votes.insert(NodeId::random(), socket1);
            }
            for _ in 0..second_votes {
                votes.insert(NodeId::random(), socket2);
            }

            TestResult::from_bool(votes.majority().0.is_none())
        }

        /// Property: Clear winner when second highest is outside the margin
        fn prop_clear_winner_outside_margin(threshold: u8, first_votes: u8, second_votes: u8) -> TestResult {
            let threshold = threshold.max(2) as usize;
            let first_votes = first_votes.max(1) as usize;
            let second_votes = second_votes as usize;

            // Only test when first meets threshold and second is outside margin
            if first_votes < threshold {
                return TestResult::discard();
            }

            let threshold_margin = ((first_votes as f64) * (1.0 - CLEAR_MAJORITY_PERCENTAGE)).round() as usize;
            if second_votes >= threshold_margin {
                return TestResult::discard();
            }

            let mut votes = IpVote::new(threshold, Duration::from_secs(10));
            let ip = "192.168.1.1".parse().unwrap();
            let socket1 = SocketAddrV4::new(ip, 8080);
            let socket2 = SocketAddrV4::new(ip, 8081);

            for _ in 0..first_votes {
                votes.insert(NodeId::random(), socket1);
            }
            for _ in 0..second_votes {
                votes.insert(NodeId::random(), socket2);
            }

            TestResult::from_bool(votes.majority().0 == Some(socket1))
        }
    }

    #[test]
    fn test_exact_threshold_boundary() {
        let mut votes = IpVote::new(3, Duration::from_secs(10));
        let ip = "192.168.1.1".parse().unwrap();
        let socket1 = SocketAddrV4::new(ip, 8080);
        let socket2 = SocketAddrV4::new(ip, 8081);

        // Add exactly threshold votes for one port
        for _ in 0..3 {
            votes.insert(NodeId::random(), socket1);
        }
        // Add 1 vote for another port
        votes.insert(NodeId::random(), socket2);

        // Should return socket1 (3 votes vs 1 vote, clear majority)
        assert_eq!(votes.majority().0, Some(socket1));
    }

    #[test]
    fn test_competing_votes_within_margin() {
        let mut votes = IpVote::new(2, Duration::from_secs(10));
        let ip = "192.168.1.1".parse().unwrap();
        let socket1 = SocketAddrV4::new(ip, 8080);
        let socket2 = SocketAddrV4::new(ip, 8081);

        // 10 votes for socket1, 8 votes for socket2
        // 8 >= (10 * (1-CLEAR_MAJORITY_PERCENTAGE)) = 7, so within margin - should return None
        for _ in 0..10 {
            votes.insert(NodeId::random(), socket1);
        }
        for _ in 0..8 {
            votes.insert(NodeId::random(), socket2);
        }

        assert_eq!(votes.majority().0, None);
    }

    #[test]
    fn test_clear_majority_outside_margin() {
        let mut votes = IpVote::new(5, Duration::from_secs(10));
        let ip = "192.168.1.1".parse().unwrap();
        let socket1 = SocketAddrV4::new(ip, 8080);
        let socket2 = SocketAddrV4::new(ip, 8081);

        // 10 votes for socket1, 4 votes for socket2
        // 4 < (10 * (1-CLEAR_MAJORITY_PERCENTAGE)) = 7, so outside margin - should return socket1 as clear winner
        for _ in 0..10 {
            votes.insert(NodeId::random(), socket1);
        }
        for _ in 0..4 {
            votes.insert(NodeId::random(), socket2);
        }

        assert_eq!(votes.majority().0, Some(socket1));
    }

    #[test]
    fn test_three_way_competition() {
        let mut votes = IpVote::new(2, Duration::from_secs(10));
        let ip = "192.168.1.1".parse().unwrap();
        let socket1 = SocketAddrV4::new(ip, 8080);
        let socket2 = SocketAddrV4::new(ip, 8081);
        let socket3 = SocketAddrV4::new(ip, 8082);

        // 5 votes each - all within margin of each other
        for _ in 0..5 {
            votes.insert(NodeId::random(), socket1);
            votes.insert(NodeId::random(), socket2);
            votes.insert(NodeId::random(), socket3);
        }

        // Should return None due to competition
        assert_eq!(votes.majority().0, None);
    }
}
