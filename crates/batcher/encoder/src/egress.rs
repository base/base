//! Immutable DA artifacts built from streaming channel output.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use base_protocol::{BLOB_DERIVATION_PREFIX_SIZE, BLOB_MAX_DATA_SIZE, ChannelId, Frame};

use crate::{BatchSubmission, BlobPayload, DaType, SubmissionId, record::Channel};

/// Stable identifier for one immutable DA artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactId(u64);

/// Submission state of one immutable DA artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactState {
    /// Available for a new L1 submission.
    Ready,
    /// Leased to one in-flight submission.
    Pending,
    /// Confirmed on L1.
    Confirmed,
}

/// Immutable payload carried by one DA artifact.
#[derive(Debug)]
pub enum DaArtifactPayload {
    /// One complete EIP-4844 blob payload.
    Blob(BlobPayload),
    /// One derivation frame carried by calldata.
    Calldata(Arc<Frame>),
}

/// One immutable artifact retained through submission retries and confirmation.
#[derive(Debug)]
pub struct DaArtifact {
    /// Stable artifact identifier.
    id: ArtifactId,
    /// Artifact payload.
    payload: DaArtifactPayload,
    /// Channels contributing frames to this artifact.
    channel_ids: Vec<ChannelId>,
    /// Current submission state.
    state: ArtifactState,
}

impl DaArtifact {
    /// Returns the artifact identifier.
    pub const fn id(&self) -> ArtifactId {
        self.id
    }

    /// Returns contributing channel identifiers.
    pub fn channel_ids(&self) -> &[ChannelId] {
        &self.channel_ids
    }

    /// Returns the current submission state.
    pub const fn state(&self) -> ArtifactState {
        self.state
    }

    /// Returns the number of frames carried by this artifact.
    pub fn frame_count(&self) -> usize {
        match &self.payload {
            DaArtifactPayload::Blob(payload) => payload.frames().len(),
            DaArtifactPayload::Calldata(_) => 1,
        }
    }
}

/// Stateful DA egress and immutable artifact ledger.
#[derive(Debug, Default)]
pub struct DaEgress {
    /// Artifacts in creation order.
    artifacts: VecDeque<DaArtifact>,
    /// In-flight submissions and their immutable artifacts.
    pending: HashMap<SubmissionId, Vec<ArtifactId>>,
    /// Next stable artifact identifier.
    next_artifact_id: u64,
}

impl DaEgress {
    /// Number of frame bytes available in one blob after its version prefix.
    pub const BLOB_CAPACITY: usize = BLOB_MAX_DATA_SIZE - BLOB_DERIVATION_PREFIX_SIZE;

    /// Creates an empty DA egress.
    pub fn new() -> Self {
        Self { artifacts: VecDeque::new(), pending: HashMap::new(), next_artifact_id: 0 }
    }

    /// Returns all retained artifacts in creation order.
    pub const fn artifacts(&self) -> &VecDeque<DaArtifact> {
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
        if self.artifacts.iter().any(|artifact| artifact.state == ArtifactState::Ready) {
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
        if !self.artifacts.iter().any(|artifact| artifact.state == ArtifactState::Ready) {
            self.build_ready_artifacts(channels, da_type, l1_head, max_blobs_per_tx);
        }

        let first_ready =
            self.artifacts.iter().position(|artifact| artifact.state == ArtifactState::Ready)?;

        let (submission, artifact_ids) = match &self.artifacts[first_ready].payload {
            DaArtifactPayload::Blob(_) => self.lease_blobs(first_ready, max_blobs_per_tx, id),
            DaArtifactPayload::Calldata(_) => self.lease_calldata(first_ready, id),
        };
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
                while self
                    .artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact.state == ArtifactState::Ready
                            && matches!(artifact.payload, DaArtifactPayload::Blob(_))
                    })
                    .count()
                    < max_blobs
                {
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

    /// Leases consecutive ready blobs as one transaction.
    fn lease_blobs(
        &mut self,
        first_ready: usize,
        maximum: usize,
        submission_id: SubmissionId,
    ) -> (BatchSubmission, Vec<ArtifactId>) {
        let indexes: Vec<_> = self
            .artifacts
            .iter()
            .enumerate()
            .skip(first_ready)
            .filter(|(_, artifact)| artifact.state == ArtifactState::Ready)
            .take_while(|(_, artifact)| matches!(artifact.payload, DaArtifactPayload::Blob(_)))
            .take(maximum)
            .map(|(index, _)| index)
            .collect();

        let payloads = indexes
            .iter()
            .map(|index| match &self.artifacts[*index].payload {
                DaArtifactPayload::Blob(payload) => payload.clone(),
                DaArtifactPayload::Calldata(_) => {
                    unreachable!("blob lease contains only blob artifacts")
                }
            })
            .collect();
        let artifact_ids = indexes.iter().map(|index| self.artifacts[*index].id).collect();

        for index in indexes {
            self.artifacts[index].state = ArtifactState::Pending;
        }

        (BatchSubmission::blobs(submission_id, payloads), artifact_ids)
    }

    /// Leases one ready calldata frame as one transaction.
    fn lease_calldata(
        &mut self,
        index: usize,
        submission_id: SubmissionId,
    ) -> (BatchSubmission, Vec<ArtifactId>) {
        let DaArtifactPayload::Calldata(frame) = &self.artifacts[index].payload else {
            unreachable!("calldata lease requires a calldata artifact");
        };
        let frame = Arc::clone(frame);
        let artifact_id = self.artifacts[index].id;

        self.artifacts[index].state = ArtifactState::Pending;

        (BatchSubmission::calldata(submission_id, frame), vec![artifact_id])
    }

    /// Confirms every artifact leased to `submission_id`.
    pub fn confirm(&mut self, submission_id: SubmissionId) -> Option<Vec<ChannelId>> {
        let artifact_ids = self.pending.remove(&submission_id)?;
        if !self.lease_matches(&artifact_ids) {
            return None;
        }

        let mut channel_ids = Vec::new();

        for artifact in &mut self.artifacts {
            if !artifact_ids.contains(&artifact.id) {
                continue;
            }

            artifact.state = ArtifactState::Confirmed;
            for channel_id in &artifact.channel_ids {
                if !channel_ids.contains(channel_id) {
                    channel_ids.push(*channel_id);
                }
            }
        }

        Some(channel_ids)
    }

    /// Returns every artifact leased to `submission_id` to ready state.
    pub fn requeue(&mut self, submission_id: SubmissionId) -> Option<usize> {
        let artifact_ids = self.pending.remove(&submission_id)?;
        if !self.lease_matches(&artifact_ids) {
            return None;
        }

        let mut frame_count = 0;
        for artifact in &mut self.artifacts {
            if artifact_ids.contains(&artifact.id) {
                frame_count += artifact.frame_count();
                artifact.state = ArtifactState::Ready;
            }
        }

        Some(frame_count)
    }

    /// Returns whether all artifacts from a fully framed channel are confirmed.
    pub fn channel_fully_confirmed(&self, channel: &Channel) -> bool {
        if !channel.framing_complete() {
            return false;
        }

        let mut found = false;
        for artifact in &self.artifacts {
            if !artifact.channel_ids.contains(&channel.id()) {
                continue;
            }
            found = true;
            if artifact.state != ArtifactState::Confirmed {
                return false;
            }
        }
        found
    }

    /// Removes safe channel references and artifacts no longer tracking any channel.
    pub fn prune_channels(&mut self, channel_ids: &[ChannelId]) {
        for artifact in &mut self.artifacts {
            artifact.channel_ids.retain(|id| !channel_ids.contains(id));
        }
        self.artifacts.retain(|artifact| !artifact.channel_ids.is_empty());
        self.retain_existing_pending_artifacts();
    }

    /// Removes artifacts invalidated by deterministic replay closure.
    pub fn invalidate_artifacts(&mut self, artifact_ids: &[ArtifactId]) {
        self.artifacts.retain(|artifact| !artifact_ids.contains(&artifact.id));
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

    /// Returns the number of frames currently available for submission.
    pub fn ready_frame_count(&self) -> usize {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.state == ArtifactState::Ready)
            .map(DaArtifact::frame_count)
            .sum()
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
        self.push_artifact(DaArtifactPayload::Blob(payload), channel_ids)
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

        self.push_artifact(DaArtifactPayload::Calldata(frame), vec![channel_id])
    }

    /// Appends one immutable ready artifact.
    fn push_artifact(
        &mut self,
        payload: DaArtifactPayload,
        channel_ids: Vec<ChannelId>,
    ) -> ArtifactId {
        let id = ArtifactId(self.next_artifact_id);
        self.next_artifact_id += 1;
        self.artifacts.push_back(DaArtifact {
            id,
            payload,
            channel_ids,
            state: ArtifactState::Ready,
        });
        id
    }

    /// Returns whether every artifact in a lease remains pending.
    fn lease_matches(&self, artifact_ids: &[ArtifactId]) -> bool {
        artifact_ids.iter().all(|id| {
            self.artifacts
                .iter()
                .any(|artifact| artifact.id == *id && artifact.state == ArtifactState::Pending)
        })
    }

    /// Removes pruned artifact identifiers from pending submissions.
    fn retain_existing_pending_artifacts(&mut self) {
        let artifacts = &self.artifacts;
        self.pending.retain(|_, pending| {
            pending.retain(|id| artifacts.iter().any(|artifact| artifact.id == *id));
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
        Channel::new(id, Arc::new(RollupConfig::default()), &config, 0, opened_l1_block)
            .unwrap()
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
}
