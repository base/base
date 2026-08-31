//! Immutable DA artifacts and the ledger tracking their submission state.

use std::{
    collections::{VecDeque, vec_deque},
    sync::Arc,
};

use base_protocol::{ChannelId, Frame};

use crate::{BatchSubmission, BlobPayload, SubmissionId};

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

impl DaArtifactPayload {
    /// Returns the number of derivation frames carried by this payload.
    pub fn frame_count(&self) -> usize {
        match self {
            Self::Blob(payload) => payload.frames().len(),
            // A calldata transaction carries exactly one frame by construction.
            Self::Calldata(_) => 1,
        }
    }
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

    /// Returns the artifact payload.
    pub const fn payload(&self) -> &DaArtifactPayload {
        &self.payload
    }

    /// Returns contributing channel identifiers.
    pub fn channel_ids(&self) -> &[ChannelId] {
        &self.channel_ids
    }

    /// Returns the current submission state.
    pub const fn state(&self) -> ArtifactState {
        self.state
    }
}

/// Ledger of immutable DA artifacts, in creation order.
///
/// Artifacts are appended [`ArtifactState::Ready`], leased to one in-flight
/// submission at a time, and retained until the channels they carry are safe.
#[derive(Debug, Default)]
pub struct DaArtifacts {
    /// Artifacts in creation order.
    artifacts: VecDeque<DaArtifact>,
    /// Next stable artifact identifier.
    next_id: u64,
}

impl DaArtifacts {
    /// Creates an empty ledger.
    pub const fn new() -> Self {
        Self { artifacts: VecDeque::new(), next_id: 0 }
    }

    /// Returns the number of retained artifacts.
    pub fn len(&self) -> usize {
        self.artifacts.len()
    }

    /// Returns whether no artifact is retained.
    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty()
    }

    /// Returns every retained artifact in creation order.
    pub fn iter(&self) -> vec_deque::Iter<'_, DaArtifact> {
        self.artifacts.iter()
    }

    /// Appends one immutable ready artifact.
    pub fn push(&mut self, payload: DaArtifactPayload, channel_ids: Vec<ChannelId>) -> ArtifactId {
        let id = ArtifactId(self.next_id);
        self.next_id += 1;
        self.artifacts.push_back(DaArtifact {
            id,
            payload,
            channel_ids,
            state: ArtifactState::Ready,
        });
        id
    }

    /// Returns whether any artifact is available for a new submission.
    pub fn has_ready(&self) -> bool {
        self.artifacts.iter().any(|artifact| artifact.state == ArtifactState::Ready)
    }

    /// Returns the number of ready blob artifacts.
    pub fn ready_blob_count(&self) -> usize {
        self.ready()
            .filter(|artifact| matches!(artifact.payload, DaArtifactPayload::Blob(_)))
            .count()
    }

    /// Returns the number of frames currently available for submission.
    pub fn ready_frame_count(&self) -> usize {
        self.ready().map(|artifact| artifact.payload.frame_count()).sum()
    }

    /// Leases the next ready artifacts as one transaction-sized submission.
    ///
    /// Consecutive ready blobs are packed into one transaction, up to
    /// `max_blobs`. A calldata artifact is always leased on its own.
    pub fn lease(
        &mut self,
        submission_id: SubmissionId,
        max_blobs: usize,
    ) -> Option<(BatchSubmission, Vec<ArtifactId>)> {
        let first_ready =
            self.artifacts.iter().position(|artifact| artifact.state == ArtifactState::Ready)?;

        Some(match self.artifacts[first_ready].payload {
            DaArtifactPayload::Blob(_) => self.lease_blobs(first_ready, max_blobs, submission_id),
            DaArtifactPayload::Calldata(_) => self.lease_calldata(first_ready, submission_id),
        })
    }

    /// Returns whether every listed artifact is still leased to a submission.
    pub fn all_pending(&self, artifact_ids: &[ArtifactId]) -> bool {
        artifact_ids.iter().all(|id| {
            self.artifacts
                .iter()
                .any(|artifact| artifact.id == *id && artifact.state == ArtifactState::Pending)
        })
    }

    /// Confirms the listed artifacts and returns their contributing channels.
    pub fn confirm(&mut self, artifact_ids: &[ArtifactId]) -> Vec<ChannelId> {
        let mut channel_ids = Vec::new();

        for artifact in self.select_mut(artifact_ids) {
            artifact.state = ArtifactState::Confirmed;
            for channel_id in &artifact.channel_ids {
                if !channel_ids.contains(channel_id) {
                    channel_ids.push(*channel_id);
                }
            }
        }

        channel_ids
    }

    /// Returns the listed artifacts to ready state, and their total frame count.
    pub fn requeue(&mut self, artifact_ids: &[ArtifactId]) -> usize {
        let mut frame_count = 0;

        for artifact in self.select_mut(artifact_ids) {
            frame_count += artifact.payload.frame_count();
            artifact.state = ArtifactState::Ready;
        }

        frame_count
    }

    /// Returns whether `channel_id` has artifacts and all of them are confirmed.
    pub fn all_confirmed_for(&self, channel_id: ChannelId) -> bool {
        let mut artifacts =
            self.artifacts.iter().filter(|artifact| artifact.channel_ids.contains(&channel_id));

        artifacts.next().is_some_and(|first| {
            first.state == ArtifactState::Confirmed
                && artifacts.all(|artifact| artifact.state == ArtifactState::Confirmed)
        })
    }

    /// Drops safe channel references, and artifacts left without any channel.
    pub fn prune_channels(&mut self, channel_ids: &[ChannelId]) {
        for artifact in &mut self.artifacts {
            artifact.channel_ids.retain(|id| !channel_ids.contains(id));
        }
        self.artifacts.retain(|artifact| !artifact.channel_ids.is_empty());
    }

    /// Removes the listed artifacts.
    pub fn invalidate(&mut self, artifact_ids: &[ArtifactId]) {
        self.artifacts.retain(|artifact| !artifact_ids.contains(&artifact.id));
    }

    /// Returns whether `artifact_id` is still retained.
    pub fn contains(&self, artifact_id: ArtifactId) -> bool {
        self.artifacts.iter().any(|artifact| artifact.id == artifact_id)
    }

    /// Clears every artifact while preserving monotonic artifact identifiers.
    pub fn clear(&mut self) {
        self.artifacts.clear();
    }

    /// Returns every artifact available for a new submission.
    fn ready(&self) -> impl Iterator<Item = &DaArtifact> {
        self.artifacts.iter().filter(|artifact| artifact.state == ArtifactState::Ready)
    }

    /// Returns the listed artifacts, in creation order.
    fn select_mut(&mut self, artifact_ids: &[ArtifactId]) -> impl Iterator<Item = &mut DaArtifact> {
        self.artifacts.iter_mut().filter(|artifact| artifact_ids.contains(&artifact.id))
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
}

impl<'a> IntoIterator for &'a DaArtifacts {
    type Item = &'a DaArtifact;
    type IntoIter = vec_deque::Iter<'a, DaArtifact>;

    fn into_iter(self) -> Self::IntoIter {
        self.artifacts.iter()
    }
}
