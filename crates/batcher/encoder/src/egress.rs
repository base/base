//! Immutable DA artifacts built from streaming channel output.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use base_protocol::{BLOB_DERIVATION_PREFIX_SIZE, BLOB_MAX_DATA_SIZE, ChannelId, Frame};

use crate::{
    BatchSubmission, BlobPayload, Channel, DaType, SubmissionId,
    artifact::{ArtifactId, DaArtifactPayload, DaArtifacts},
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
    use crate::{ChannelAddOutcome, EncoderConfig, SubmissionPayload};

    const FIRST: ChannelId = [1; 16];
    const SECOND: ChannelId = [2; 16];

    fn channel(id: ChannelId, max_channel_duration: u64) -> Channel {
        let config = EncoderConfig { max_channel_duration, ..EncoderConfig::default() };
        Channel::new(id, Arc::new(RollupConfig::default()), &config, 0, 0).unwrap()
    }

    /// Adds a batch of `transaction_len` random bytes, which Brotli cannot shrink.
    fn add_incompressible_batch(channel: &mut Channel, transaction_len: usize) {
        let mut state = channel.blocks_added() as u64 + 1;
        let transaction = (0..transaction_len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        let batch = SingleBatch {
            parent_hash: B256::ZERO,
            epoch_num: 0,
            epoch_hash: B256::ZERO,
            timestamp: 0,
            transactions: vec![Bytes::from(transaction)],
        };
        let outcome = channel.add_batch(&batch, transaction_len as u64).unwrap();
        assert_eq!(outcome, ChannelAddOutcome::Accepted);
    }

    /// An open channel holding at least `min_output` bytes of stable output.
    fn open_channel_with_output(id: ChannelId, min_output: usize) -> Channel {
        let mut channel = channel(id, 10);
        let mut transaction_len = 4_096;
        while channel.available_output() < min_output {
            add_incompressible_batch(&mut channel, transaction_len);
            transaction_len = (transaction_len * 2).min(200_000);
        }
        channel
    }

    /// A closed channel whose tail is shorter than a blob and than a calldata frame.
    fn closed_channel_with_a_short_tail(id: ChannelId, max_channel_duration: u64) -> Channel {
        let mut channel = channel(id, max_channel_duration);
        add_incompressible_batch(&mut channel, 50_000);
        channel.close().unwrap();
        assert!(channel.available_output() < channel.max_frame_data());
        channel
    }

    /// Every frame `submission` carries, in order.
    fn all_frames(submission: &BatchSubmission) -> Vec<Arc<Frame>> {
        match submission.payload() {
            SubmissionPayload::Blobs(blobs) => {
                blobs.iter().flat_map(BlobPayload::frames).cloned().collect()
            }
            SubmissionPayload::Calldata(frame) => vec![Arc::clone(frame)],
        }
    }

    /// The channel and last-frame flag of every frame `submission` carries.
    fn frames(submission: &BatchSubmission) -> Vec<(ChannelId, bool)> {
        all_frames(submission).iter().map(|frame| (frame.id, frame.is_last)).collect()
    }

    /// Partial output of an open channel stays buffered, as blob or calldata, even past the
    /// channel deadline.
    #[test]
    fn partial_output_of_an_open_channel_stays_buffered() {
        let mut open = channel(FIRST, 1);
        add_incompressible_batch(&mut open, 50_000);
        let buffered = open.available_output();
        let mut channels = VecDeque::from([open]);
        let mut egress = DaEgress::new();

        assert!(
            egress.next_submission(&mut channels, DaType::Blob, 5, 6, SubmissionId(0)).is_none()
        );
        assert!(
            egress
                .next_submission(&mut channels, DaType::Calldata, 5, 6, SubmissionId(1))
                .is_none()
        );
        assert_eq!(channels[0].available_output(), buffered);
    }

    /// A closed tail too short to fill a blob goes out once its channel deadline is due.
    #[test]
    fn a_short_blob_tail_waits_for_its_channel_deadline() {
        let mut channels = VecDeque::from([closed_channel_with_a_short_tail(FIRST, 2)]);
        let mut egress = DaEgress::new();

        assert!(
            egress.next_submission(&mut channels, DaType::Blob, 1, 6, SubmissionId(0)).is_none()
        );
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 2, 6, SubmissionId(1))
            .expect("the tail at the deadline");

        assert_eq!(frames(&submission), [(FIRST, true)]);
    }

    /// A blob carries the tail of a closed channel and fills up with the next channel.
    #[test]
    fn a_blob_packs_a_closed_tail_with_the_next_channel() {
        let first = closed_channel_with_a_short_tail(FIRST, 10);
        let second = open_channel_with_output(SECOND, DaEgress::BLOB_CAPACITY);
        let mut channels = VecDeque::from([first, second]);

        let submission = DaEgress::new()
            .next_submission(&mut channels, DaType::Blob, 0, 1, SubmissionId(0))
            .expect("a shared blob");

        assert_eq!(submission.blob_count(), 1);
        assert_eq!(frames(&submission), [(FIRST, true), (SECOND, false)]);
    }

    /// Confirming a submission reports the channels it carried and ends its lease, so a second
    /// confirmation reports nothing.
    #[test]
    fn confirm_reports_the_submitted_channels_once() {
        let channel = open_channel_with_output(FIRST, DaEgress::BLOB_CAPACITY);
        let mut channels = VecDeque::from([channel]);
        let mut egress = DaEgress::new();
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 6, SubmissionId(0))
            .expect("blob submission");
        assert_eq!(egress.pending_submission_count(), 1);

        assert_eq!(egress.confirm(submission.id), Some(vec![FIRST]));
        assert_eq!(egress.pending_submission_count(), 0);
        assert!(egress.confirm(submission.id).is_none());
    }

    /// Requeuing a submission returns its frame count once, and the retry goes out alone with
    /// the same frames, before the channel's newer output.
    #[test]
    fn a_requeued_submission_is_leased_again_before_new_output() {
        let channel = open_channel_with_output(FIRST, 2 * DaEgress::BLOB_CAPACITY);
        let mut channels = VecDeque::from([channel]);
        let mut egress = DaEgress::new();
        let first = egress
            .next_submission(&mut channels, DaType::Blob, 0, 1, SubmissionId(0))
            .expect("blob submission");

        assert_eq!(egress.requeue(first.id), Some(first.frame_count()));
        assert!(egress.requeue(first.id).is_none());
        let retry = egress
            .next_submission(&mut channels, DaType::Blob, 0, 2, SubmissionId(1))
            .expect("the retry");

        assert_eq!(retry.blob_count(), 1);
        assert_eq!(all_frames(&retry), all_frames(&first));
    }

    /// A reset drops every artifact and pending submission, so a confirmation for a submission
    /// issued before it finds nothing.
    #[test]
    fn reset_drops_every_artifact_and_submission() {
        let channel = open_channel_with_output(FIRST, DaEgress::BLOB_CAPACITY);
        let mut channels = VecDeque::from([channel]);
        let mut egress = DaEgress::new();
        let submission = egress
            .next_submission(&mut channels, DaType::Blob, 0, 6, SubmissionId(0))
            .expect("blob submission");

        egress.reset();

        assert!(egress.artifacts().is_empty());
        assert_eq!(egress.pending_submission_count(), 0);
        assert!(egress.confirm(submission.id).is_none());
    }
}
