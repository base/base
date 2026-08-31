//! Immutable DA artifacts built from streaming channel output.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use base_protocol::{BLOB_DERIVATION_PREFIX_SIZE, BLOB_MAX_DATA_SIZE, ChannelId, Frame};

use crate::{
    BatchSubmission, BlobPayload, DaType, SubmissionId,
    artifact::{ArtifactId, DaArtifactPayload, DaArtifacts},
    record::Channel,
};

/// Stateful DA egress over an immutable artifact ledger.
#[derive(Debug, Default)]
pub struct DaEgress {
    /// Immutable artifacts and their submission state.
    artifacts: DaArtifacts,
    /// In-flight submissions and their immutable artifacts.
    pending: HashMap<SubmissionId, Vec<ArtifactId>>,
}

impl DaEgress {
    /// Number of frame bytes available in one blob after its version prefix.
    pub const BLOB_CAPACITY: usize = BLOB_MAX_DATA_SIZE - BLOB_DERIVATION_PREFIX_SIZE;

    /// Creates an empty DA egress.
    pub fn new() -> Self {
        Self { artifacts: DaArtifacts::new(), pending: HashMap::new() }
    }

    /// Returns the immutable artifact ledger.
    pub const fn artifacts(&self) -> &DaArtifacts {
        &self.artifacts
    }

    /// Plans one full or deadline-released blob without mutating channels.
    fn plan_blob(
        channels: &VecDeque<Channel>,
        l1_head: u64,
    ) -> Option<Vec<(ChannelId, usize, bool)>> {
        let mut frames = Vec::new();
        let mut remaining = Self::BLOB_CAPACITY;
        let mut release_due = false;

        for channel in channels {
            if channel.framing_complete() {
                continue;
            }

            let first_frame = frames.len();
            let channel_complete = Self::plan_channel_frames(channel, &mut remaining, &mut frames);

            if frames.len() > first_frame && !channel.is_open() {
                release_due |= channel.deadline_due(l1_head);
            }

            // Open output, or a closed tail that does not fit, stops the walk.
            if !channel_complete {
                break;
            }
        }

        if frames.is_empty() {
            return None;
        }

        let saturated = remaining < Frame::ENCODED_OVERHEAD + 1;
        if saturated || release_due { Some(frames) } else { None }
    }

    /// Append frames from one channel that fit in the current blob.
    ///
    /// `true` after the terminal frame, so the planner may continue to the next channel.
    fn plan_channel_frames(
        channel: &Channel,
        remaining: &mut usize,
        frames: &mut Vec<(ChannelId, usize, bool)>,
    ) -> bool {
        let mut available = channel.available_output();
        let mut terminal_pending = channel.terminal_pending();

        // Every emitted data frame consumes a stable compressed prefix.
        while available > 0 && *remaining > Frame::ENCODED_OVERHEAD {
            let data_capacity =
                (*remaining - Frame::ENCODED_OVERHEAD).min(channel.max_frame_data());
            let data_len = available.min(data_capacity);
            let is_last = data_len == available && terminal_pending;
            frames.push((channel.id(), data_len, is_last));

            available -= data_len;
            *remaining -= Frame::ENCODED_OVERHEAD + data_len;

            if is_last {
                terminal_pending = false;
                break;
            }
        }

        // An empty compressed stream still needs one terminal frame.
        if available == 0 && terminal_pending && *remaining >= Frame::ENCODED_OVERHEAD {
            frames.push((channel.id(), 0, true));

            *remaining -= Frame::ENCODED_OVERHEAD;
            terminal_pending = false;
        }

        !channel.is_open() && available == 0 && !terminal_pending
    }

    /// Returns whether an immutable ready artifact or buildable payload exists.
    pub fn has_ready_submission(
        &self,
        channels: &VecDeque<Channel>,
        da_type: DaType,
        l1_head: u64,
    ) -> bool {
        if self.artifacts.has_ready() {
            return true;
        }

        match da_type {
            DaType::Blob => Self::plan_blob(channels, l1_head).is_some(),
            DaType::Calldata => Self::plan_calldata(channels).is_some(),
        }
    }

    /// Builds and leases one transaction-sized submission.
    pub fn next_submission(
        &mut self,
        channels: &mut VecDeque<Channel>,
        da_type: DaType,
        l1_head: u64,
        max_blobs_per_tx: usize,
        id: SubmissionId,
    ) -> Option<BatchSubmission> {
        if !self.artifacts.has_ready() {
            self.build_ready_artifacts(channels, da_type, l1_head, max_blobs_per_tx);
        }

        let (submission, artifact_ids) = self.artifacts.lease(id, max_blobs_per_tx)?;
        self.pending.insert(id, artifact_ids);

        Some(submission)
    }

    /// Materializes ready artifacts only when no retry is already waiting.
    fn build_ready_artifacts(
        &mut self,
        channels: &mut VecDeque<Channel>,
        da_type: DaType,
        l1_head: u64,
        max_blobs: usize,
    ) {
        match da_type {
            DaType::Blob => {
                while self.artifacts.ready_blob_count() < max_blobs {
                    let Some(plan) = Self::plan_blob(channels, l1_head) else {
                        break;
                    };
                    self.commit_blob(channels, plan);
                }
            }
            DaType::Calldata => {
                if let Some(plan) = Self::plan_calldata(channels) {
                    self.commit_calldata(channels, plan);
                }
            }
        }
    }

    /// Confirms every artifact leased to `submission_id`.
    pub fn confirm(&mut self, submission_id: SubmissionId) -> Option<Vec<ChannelId>> {
        let artifact_ids = self.pending.remove(&submission_id)?;
        if !self.artifacts.all_pending(&artifact_ids) {
            return None;
        }

        Some(self.artifacts.confirm(&artifact_ids))
    }

    /// Returns every artifact leased to `submission_id` to ready state.
    pub fn requeue(&mut self, submission_id: SubmissionId) -> Option<usize> {
        let artifact_ids = self.pending.remove(&submission_id)?;
        if !self.artifacts.all_pending(&artifact_ids) {
            return None;
        }

        Some(self.artifacts.requeue(&artifact_ids))
    }

    /// Returns whether all artifacts from a fully framed channel are confirmed.
    pub fn channel_fully_confirmed(&self, channel: &Channel) -> bool {
        channel.framing_complete() && self.artifacts.all_confirmed_for(channel.id())
    }

    /// Removes safe channel references and artifacts no longer tracking any channel.
    pub fn prune_channels(&mut self, channel_ids: &[ChannelId]) {
        self.artifacts.prune_channels(channel_ids);
        self.retain_existing_pending_artifacts();
    }

    /// Removes artifacts invalidated by deterministic replay closure.
    pub fn invalidate_artifacts(&mut self, artifact_ids: &[ArtifactId]) {
        self.artifacts.invalidate(artifact_ids);
        self.pending.retain(|_, pending| !pending.iter().any(|id| artifact_ids.contains(id)));
    }

    /// Adds every artifact sharing an in-flight submission with `affected`.
    pub fn extend_with_submission_artifacts(&self, affected: &mut Vec<ArtifactId>) {
        for artifact_ids in self.pending.values() {
            if artifact_ids.iter().any(|id| affected.contains(id)) {
                for id in artifact_ids {
                    if !affected.contains(id) {
                        affected.push(*id);
                    }
                }
            }
        }
    }

    /// Clears every artifact while preserving monotonic artifact identifiers.
    pub fn reset(&mut self) {
        self.artifacts.clear();
        self.pending.clear();
    }

    /// Plans one calldata frame without mutating channel output.
    fn plan_calldata(channels: &VecDeque<Channel>) -> Option<(ChannelId, usize, bool)> {
        // Preserve FIFO ordering: only the first unfinished channel may emit.
        for channel in channels {
            if channel.framing_complete() {
                continue;
            }

            let available = channel.available_output();

            // Full frames may stream before the channel closes.
            if available >= channel.max_frame_data() {
                let data_len = channel.max_frame_data();
                let is_last = data_len == available && channel.terminal_pending();
                return Some((channel.id(), data_len, is_last));
            }

            // A closed channel releases its final, possibly empty, frame.
            if channel.terminal_pending() {
                return Some((channel.id(), available, true));
            }

            // Partial output from an open channel remains buffered.
            return None;
        }

        None
    }

    /// Commits a validated blob plan into one immutable ready artifact.
    fn commit_blob(
        &mut self,
        channels: &mut VecDeque<Channel>,
        plan: Vec<(ChannelId, usize, bool)>,
    ) -> ArtifactId {
        let mut frames = Vec::with_capacity(plan.len());
        let mut channel_ids = Vec::new();

        for (channel_id, data_len, is_last) in plan {
            let channel = channels
                .iter_mut()
                .find(|channel| channel.id() == channel_id)
                .expect("planned channel remains in the encoder FIFO");
            let frame = Arc::new(
                channel.take_frame(data_len, is_last).expect("blob plan satisfies channel limits"),
            );

            if !channel_ids.contains(&channel.id()) {
                channel_ids.push(channel.id());
            }
            frames.push(frame);
        }

        let payload = BlobPayload::new(frames);
        self.artifacts.push(DaArtifactPayload::Blob(payload), channel_ids)
    }

    /// Commits one calldata frame into an immutable ready artifact.
    fn commit_calldata(
        &mut self,
        channels: &mut VecDeque<Channel>,
        plan: (ChannelId, usize, bool),
    ) -> ArtifactId {
        let (channel_id, data_len, is_last) = plan;
        let channel = channels
            .iter_mut()
            .find(|channel| channel.id() == channel_id)
            .expect("planned channel remains in the encoder FIFO");
        let frame = Arc::new(
            channel.take_frame(data_len, is_last).expect("calldata plan satisfies channel limits"),
        );

        self.artifacts.push(DaArtifactPayload::Calldata(frame), vec![channel_id])
    }

    /// Removes pruned artifact identifiers from pending submissions.
    fn retain_existing_pending_artifacts(&mut self) {
        let artifacts = &self.artifacts;
        self.pending.retain(|_, pending| {
            pending.retain(|id| artifacts.contains(*id));
            !pending.is_empty()
        });
    }

    /// Returns artifacts leased to `submission_id`.
    pub fn pending_artifacts(&self, submission_id: SubmissionId) -> Option<&[ArtifactId]> {
        self.pending.get(&submission_id).map(Vec::as_slice)
    }

    /// Returns the number of in-flight submissions.
    pub fn pending_submission_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes};
    use base_common_genesis::RollupConfig;
    use base_protocol::SingleBatch;

    use super::*;
    use crate::{CompressionAlgo, EncoderConfig, record::ChannelAddOutcome};

    fn channel(id: ChannelId, opened_l1_block: u64, duration: u64) -> Channel {
        let config = EncoderConfig {
            compression_algo: CompressionAlgo::Zlib,
            max_channel_duration: duration,
            ..EncoderConfig::default()
        };
        Channel::new(id, Arc::new(RollupConfig::default()), &config, 0, opened_l1_block).unwrap()
    }

    fn incompressible_batch(transaction_len: usize) -> SingleBatch {
        let mut state = 1u64;
        let transaction = (0..transaction_len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        SingleBatch {
            parent_hash: B256::ZERO,
            epoch_num: 0,
            epoch_hash: B256::ZERO,
            timestamp: 0,
            transactions: vec![Bytes::from(transaction)],
        }
    }

    fn append_accepted(channel: &mut Channel, transaction_len: usize) {
        assert_eq!(
            channel
                .add_batch(incompressible_batch(transaction_len), transaction_len as u64)
                .unwrap(),
            ChannelAddOutcome::Accepted
        );
    }

    fn fill_open_channel(channel: &mut Channel, min_output: usize) {
        let mut transaction_len = 4_096;
        while channel.available_output() < min_output {
            append_accepted(channel, transaction_len);
            transaction_len = (transaction_len * 2).min(200_000);
        }
        assert!(channel.is_open());
    }

    #[test]
    fn full_blob_is_ready_from_open_channel_output() {
        let mut channel = channel([1; 16], 0, 10);
        fill_open_channel(&mut channel, DaEgress::BLOB_CAPACITY - Frame::ENCODED_OVERHEAD);
        let channels = VecDeque::from([channel]);

        let plan = DaEgress::plan_blob(&channels, 0).expect("full blob");
        let (_, _, is_last) = plan[0];

        assert_eq!(plan.len(), 1);
        assert!(!is_last);
    }

    #[test]
    fn open_channel_timeout_does_not_release_non_terminal_partial_data() {
        let mut channel = channel([1; 16], 0, 1);
        append_accepted(&mut channel, 50_000);
        assert!(channel.available_output() > 0);
        assert!(channel.available_output() < DaEgress::BLOB_CAPACITY - Frame::ENCODED_OVERHEAD);
        let channels = VecDeque::from([channel]);

        assert!(DaEgress::plan_blob(&channels, 1).is_none());
    }

    #[test]
    fn closed_channel_timeout_releases_partial_tail() {
        let mut channel = channel([1; 16], 0, 2);
        append_accepted(&mut channel, 50_000);
        channel.close().unwrap();
        assert!(channel.available_output() > 0);
        assert!(channel.available_output() < DaEgress::BLOB_CAPACITY - Frame::ENCODED_OVERHEAD);
        let channels = VecDeque::from([channel]);

        assert!(DaEgress::plan_blob(&channels, 1).is_none());
        let plan = DaEgress::plan_blob(&channels, 2).expect("timeout-released tail");
        assert!(plan.last().is_some_and(|(_, _, is_last)| *is_last));
    }

    #[test]
    fn calldata_plan_emits_a_full_frame_before_close() {
        let mut channel = channel([1; 16], 0, 10);
        let max_frame_data = channel.max_frame_data();
        fill_open_channel(&mut channel, max_frame_data);
        let channels = VecDeque::from([channel]);

        assert_eq!(DaEgress::plan_calldata(&channels), Some(([1; 16], max_frame_data, false)));
    }

    #[test]
    fn calldata_plan_buffers_partial_open_output() {
        let mut channel = channel([1; 16], 0, 10);
        append_accepted(&mut channel, 50_000);
        let buffered = channel.available_output();
        assert!(buffered > 0 && buffered < channel.max_frame_data());
        let channels = VecDeque::from([channel]);

        assert!(DaEgress::plan_calldata(&channels).is_none());
        assert_eq!(channels[0].available_output(), buffered);
    }

    #[test]
    fn calldata_plan_releases_the_closed_tail() {
        let mut channel = channel([1; 16], 0, 10);
        append_accepted(&mut channel, 50_000);
        channel.close().unwrap();
        let tail = channel.available_output();
        assert!(tail > 0 && tail < channel.max_frame_data());
        let channels = VecDeque::from([channel]);

        assert_eq!(DaEgress::plan_calldata(&channels), Some(([1; 16], tail, true)));
    }

    #[test]
    fn plan_crosses_closed_channel_boundary_without_mutation() {
        let mut first = channel([1; 16], 0, 10);
        append_accepted(&mut first, 50_000);
        first.close().unwrap();
        let first_bytes = first.available_output();
        assert!(first_bytes > 0);
        assert!(first_bytes < DaEgress::BLOB_CAPACITY - Frame::ENCODED_OVERHEAD);

        let mut second = channel([2; 16], 0, 10);
        fill_open_channel(&mut second, DaEgress::BLOB_CAPACITY);
        let second_bytes = second.available_output();
        let channels = VecDeque::from([first, second]);

        let plan = DaEgress::plan_blob(&channels, 0).expect("cross-channel blob");
        let (first_id, _, first_is_last) = plan[0];
        let (second_id, _, second_is_last) = plan[1];

        assert_eq!(first_id, [1; 16]);
        assert!(first_is_last);
        assert_eq!(second_id, [2; 16]);
        assert!(!second_is_last);
        assert_eq!(channels[0].available_output(), first_bytes);
        assert_eq!(channels[1].available_output(), second_bytes);
    }

    /// Returns an egress and one channel holding `blobs` worth of output.
    fn egress_with_output(blobs: usize) -> (DaEgress, VecDeque<Channel>) {
        let mut channel = channel([1; 16], 0, 10);
        fill_open_channel(&mut channel, DaEgress::BLOB_CAPACITY * blobs);

        (DaEgress::new(), VecDeque::from([channel]))
    }

    #[test]
    fn submission_confirms_the_contributing_channel() {
        let (mut egress, mut channels) = egress_with_output(1);

        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 6, SubmissionId(0))
            .expect("blob submission");

        assert_eq!(egress.pending_submission_count(), 1);
        assert!(!egress.channel_fully_confirmed(&channels[0]));

        assert_eq!(egress.confirm(submission.id), Some(vec![[1; 16]]));
        assert_eq!(egress.pending_submission_count(), 0);
        assert!(egress.confirm(submission.id).is_none());

        // The channel keeps producing frames, so confirmed artifacts are not enough.
        assert!(!egress.channel_fully_confirmed(&channels[0]));
    }

    #[test]
    fn a_requeued_submission_is_leased_again_before_new_output() {
        let (mut egress, mut channels) = egress_with_output(2);
        let first = egress
            .next_submission(&mut channels, DaType::Blob, 0, 1, SubmissionId(0))
            .expect("blob submission");
        let leased = egress.pending_artifacts(first.id).expect("in-flight submission").to_vec();

        assert_eq!(egress.requeue(first.id), Some(first.frame_count()));
        assert_eq!(egress.artifacts().ready_frame_count(), first.frame_count());
        assert_eq!(egress.pending_submission_count(), 0);
        assert!(egress.requeue(first.id).is_none());

        // The channel still holds output, but a retry takes priority over new blobs.
        let retry = egress
            .next_submission(&mut channels, DaType::Blob, 0, 2, SubmissionId(1))
            .expect("retry submission");

        assert_eq!(egress.pending_artifacts(retry.id), Some(leased.as_slice()));
        assert_eq!(egress.artifacts().len(), 1);
    }

    #[test]
    fn blob_submissions_are_capped_by_max_blobs_per_tx() {
        let (mut egress, mut channels) = egress_with_output(3);

        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 2, SubmissionId(0))
            .expect("blob submission");

        assert_eq!(submission.blob_count(), 2);
        assert_eq!(egress.artifacts().len(), 2);
    }

    #[test]
    fn co_submitted_artifacts_extend_the_replay_closure() {
        let (mut egress, mut channels) = egress_with_output(2);
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 2, SubmissionId(0))
            .expect("blob submission");
        let leased =
            egress.pending_artifacts(submission.id).expect("in-flight submission").to_vec();
        assert_eq!(leased.len(), 2);

        let mut affected = vec![leased[0]];
        egress.extend_with_submission_artifacts(&mut affected);

        assert_eq!(affected, leased);
    }

    #[test]
    fn invalidating_a_leased_artifact_drops_its_submission() {
        let (mut egress, mut channels) = egress_with_output(1);
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 6, SubmissionId(0))
            .expect("blob submission");
        let leased =
            egress.pending_artifacts(submission.id).expect("in-flight submission").to_vec();

        egress.invalidate_artifacts(&leased);

        assert!(egress.artifacts().is_empty());
        assert_eq!(egress.pending_submission_count(), 0);
        assert!(egress.pending_artifacts(submission.id).is_none());
    }

    #[test]
    fn pruning_a_safe_channel_drops_its_confirmed_artifacts() {
        let (mut egress, mut channels) = egress_with_output(1);
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 6, SubmissionId(0))
            .expect("blob submission");
        egress.confirm(submission.id).expect("confirmed submission");

        egress.prune_channels(&[[1; 16]]);

        assert!(egress.artifacts().is_empty());
    }

    #[test]
    fn reset_clears_artifacts_without_reusing_identifiers() {
        let (mut egress, mut channels) = egress_with_output(2);
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 1, SubmissionId(0))
            .expect("blob submission");
        let leased =
            egress.pending_artifacts(submission.id).expect("in-flight submission").to_vec();

        egress.reset();
        assert!(egress.artifacts().is_empty());
        assert_eq!(egress.pending_submission_count(), 0);

        let rebuilt = egress
            .next_submission(&mut channels, DaType::Blob, 0, 1, SubmissionId(1))
            .expect("submission after reset");

        assert_ne!(egress.pending_artifacts(rebuilt.id), Some(leased.as_slice()));
    }
}
