//! Contains peer scoring types.

use std::{collections::HashMap, time::Duration};

use derive_more::{Display, FromStr};
use libp2p::gossipsub::{PeerScoreParams, PeerScoreThresholds, TopicHash, TopicScoreParams};

/// The peer scoring level is used to determine
/// how peers are scored based on their behavior.
#[derive(Debug, FromStr, Display, Default, Clone, Copy, PartialEq, Eq)]
pub enum PeerScoreLevel {
    /// No peer scoring is applied.
    #[default]
    Off,
    /// Light peer scoring is applied.
    Light,
}

impl PeerScoreLevel {
    /// Decay to zero is the decay factor for a peer's score to zero.
    pub const DECAY_TO_ZERO: f64 = 0.01;

    /// Mesh weight is the weight of the mesh delivery topic.
    pub const MESH_WEIGHT: f64 = -0.7;

    /// Max in mesh score is the maximum score for being in the mesh.
    pub const MAX_IN_MESH_SCORE: f64 = 10.0;

    /// Decay epoch is the number of epochs to decay the score over.
    pub const DECAY_EPOCH: f64 = 5.0;

    /// Helper function to calculate the decay factor for a given duration.
    /// The decay factor is calculated using the formula:
    /// `decay_factor = (1 - decay_to_zero) ^ (duration / slot)`.
    pub fn score_decay(duration: Duration, slot: Duration) -> f64 {
        let num_of_times = duration.as_secs_f64() / slot.as_secs_f64();
        (1.0 - Self::DECAY_TO_ZERO).powf(1.0 / num_of_times)
    }

    /// Default peer score thresholds.
    pub const DEFAULT_PEER_SCORE_THRESHOLDS: PeerScoreThresholds = PeerScoreThresholds {
        gossip_threshold: -10.0,
        publish_threshold: -40.0,
        graylist_threshold: -40.0,
        accept_px_threshold: 20.0,
        opportunistic_graft_threshold: 0.05,
    };

    /// Returns a cap on the in mesh score.
    /// The cap is calculated based on the slot duration.
    /// The formula used is:
    /// `cap = (3600 * time.Second) / slot`.
    pub fn in_mesh_cap(slot: Duration) -> f64 {
        (3600 * Duration::from_secs(1)).as_secs_f64() / slot.as_secs_f64()
    }

    /// Returns the topic score parameters given the slot duration.
    pub fn topic_score_params_for_slot(slot: Duration) -> TopicScoreParams {
        let epoch = slot * 6;
        let invalid_decay_period = 50 * epoch;
        let decay_epoch = epoch.mul_f64(Self::DECAY_EPOCH);
        let epoch_slots = epoch.as_secs_f64() / slot.as_secs_f64();
        TopicScoreParams {
            topic_weight: 0.8,
            time_in_mesh_weight: Self::MAX_IN_MESH_SCORE / Self::in_mesh_cap(slot),
            time_in_mesh_quantum: slot,
            time_in_mesh_cap: Self::in_mesh_cap(slot),
            first_message_deliveries_weight: 1.0,
            first_message_deliveries_decay: Self::score_decay(20 * epoch, slot),
            first_message_deliveries_cap: 23.0,
            mesh_message_deliveries_weight: Self::MESH_WEIGHT,
            mesh_message_deliveries_decay: Self::score_decay(decay_epoch, slot),
            mesh_message_deliveries_cap: epoch_slots * Self::DECAY_EPOCH,
            mesh_message_deliveries_threshold: epoch_slots * Self::DECAY_EPOCH / 10.0,
            mesh_message_deliveries_window: Duration::from_secs(2),
            mesh_message_deliveries_activation: epoch * 4,
            mesh_failure_penalty_weight: Self::MESH_WEIGHT,
            mesh_failure_penalty_decay: Self::score_decay(decay_epoch, slot),
            invalid_message_deliveries_weight: -140.4475,
            invalid_message_deliveries_decay: Self::score_decay(invalid_decay_period, slot),
        }
    }

    /// Returns the topic score parameters given the block time.
    pub fn topic_score_params(block_time: u64) -> TopicScoreParams {
        Self::topic_score_params_for_slot(Duration::from_secs(block_time))
    }

    /// Constructs topic scores for the given topics.
    pub fn topic_scores(
        topics: Vec<TopicHash>,
        block_time: u64,
    ) -> HashMap<TopicHash, TopicScoreParams> {
        Self::topic_scores_for_slot(topics, Duration::from_secs(block_time))
    }

    /// Constructs topic scores for the given topics and slot duration.
    pub fn topic_scores_for_slot(
        topics: Vec<TopicHash>,
        slot: Duration,
    ) -> HashMap<TopicHash, TopicScoreParams> {
        let mut topic_scores = HashMap::with_capacity(topics.len());
        for topic in topics {
            debug!(target: "scoring", topic = %topic, "Topic scoring enabled");
            topic_scores.insert(topic, Self::topic_score_params_for_slot(slot));
        }
        topic_scores
    }

    /// Returns the [`PeerScoreParams`] for the given peer scoring level.
    ///
    /// # Arguments
    /// * `block_time` - The block time in seconds.
    pub fn to_params(
        &self,
        topics: Vec<TopicHash>,
        topic_scoring: bool,
        block_time: u64,
    ) -> Option<PeerScoreParams> {
        self.to_params_for_slot(topics, topic_scoring, Duration::from_secs(block_time))
    }

    /// Returns the [`PeerScoreParams`] for the given peer scoring level and slot duration.
    pub fn to_params_for_slot(
        &self,
        topics: Vec<TopicHash>,
        topic_scoring: bool,
        slot: Duration,
    ) -> Option<PeerScoreParams> {
        debug!(target: "scoring", slot = ?slot, "Slot duration");
        let epoch = slot * 6;
        let ten_epochs = epoch * 10;
        let one_hundred_epochs = epoch * 100;
        let penalty_decay = Self::score_decay(ten_epochs, slot);
        let topics = if topic_scoring {
            Self::topic_scores_for_slot(topics, slot)
        } else {
            Default::default()
        };
        match self {
            Self::Off => None,
            Self::Light => Some(PeerScoreParams {
                topics,
                topic_score_cap: 34.0,
                app_specific_weight: 1.0,
                ip_colocation_factor_weight: -35.0,
                ip_colocation_factor_threshold: 10.0,
                ip_colocation_factor_whitelist: Default::default(),
                behaviour_penalty_weight: -16.0,
                behaviour_penalty_threshold: 6.0,
                behaviour_penalty_decay: penalty_decay,
                decay_interval: slot,
                decay_to_zero: Self::DECAY_TO_ZERO,
                retain_score: one_hundred_epochs,
                slow_peer_weight: -0.2,   // default
                slow_peer_threshold: 0.0, // default
                slow_peer_decay: 0.2,
            }),
        }
    }

    /// Returns the [`PeerScoreThresholds`].
    pub const fn thresholds() -> PeerScoreThresholds {
        Self::DEFAULT_PEER_SCORE_THRESHOLDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_decay_supports_subsecond_slots() {
        let decay = PeerScoreLevel::score_decay(Duration::from_secs(1), Duration::from_millis(200));

        assert!(decay.is_finite());
        assert!(decay > 0.0);
        assert!(decay < 1.0);
    }

    #[test]
    fn topic_score_params_support_subsecond_slots() {
        let params = PeerScoreLevel::topic_score_params_for_slot(Duration::from_millis(200));

        assert_eq!(params.time_in_mesh_quantum, Duration::from_millis(200));
        assert_eq!(params.mesh_message_deliveries_activation, Duration::from_millis(4_800));
        assert!((params.mesh_message_deliveries_cap - 30.0).abs() < 1e-12);
        assert!((params.mesh_message_deliveries_threshold - 3.0).abs() < 1e-12);
    }

    #[test]
    fn peer_score_params_support_subsecond_slots() {
        let params = PeerScoreLevel::Light
            .to_params_for_slot(Vec::new(), false, Duration::from_millis(200))
            .expect("light scoring should produce params");

        assert_eq!(params.decay_interval, Duration::from_millis(200));
        assert_eq!(params.retain_score, Duration::from_secs(120));
    }
}
