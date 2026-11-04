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
    /// A calldata artifact is always leased on its own. Consecutive ready blobs
    /// are packed into one transaction up to and including the first one that
    /// holds a channel's last frame, and at most `max_blobs`. The safe head
    /// moves only once a channel's last frame is on L1, so sending its earlier
    /// blobs ahead in their own transactions gains nothing. Until one of these
    /// bounds is reached, or a calldata artifact in any state ends the run,
    /// nothing is leased. A calldata artifact stays in the ledger until its
    /// channel is safe, which needs the blobs before it, so a requeued run keeps
    /// the same cut.
    pub fn lease(
        &mut self,
        submission_id: SubmissionId,
        max_blobs: usize,
    ) -> Option<(BatchSubmission, Vec<ArtifactId>)> {
        let first_ready =
            self.artifacts.iter().position(|artifact| artifact.state == ArtifactState::Ready)?;

        match self.artifacts[first_ready].payload {
            DaArtifactPayload::Blob(_) => self.lease_blobs(first_ready, max_blobs, submission_id),
            DaArtifactPayload::Calldata(_) => Some(self.lease_calldata(first_ready, submission_id)),
        }
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

    /// Leases consecutive ready blobs as one transaction, once the run is complete.
    fn lease_blobs(
        &mut self,
        first_ready: usize,
        maximum: usize,
        submission_id: SubmissionId,
    ) -> Option<(BatchSubmission, Vec<ArtifactId>)> {
        let mut artifacts = self.artifacts.iter().enumerate().skip(first_ready);
        let mut indexes = Vec::new();
        let complete = loop {
            let Some((index, artifact)) = artifacts.next() else {
                break false;
            };
            // No later blob may join the run ahead of a calldata frame, whatever its state, so
            // a requeued run keeps the same cut.
            let DaArtifactPayload::Blob(payload) = &artifact.payload else {
                break true;
            };
            if artifact.state != ArtifactState::Ready {
                continue;
            }
            indexes.push(index);
            if indexes.len() == maximum || payload.frames().iter().any(|frame| frame.is_last) {
                break true;
            }
        };
        if !complete {
            return None;
        }

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

        Some((BatchSubmission::blobs(submission_id, payloads), artifact_ids))
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: ChannelId = [1; 16];
    const SECOND: ChannelId = [2; 16];

    const MAX_BLOBS: usize = 6;

    fn frame(id: ChannelId, is_last: bool) -> Arc<Frame> {
        Arc::new(Frame { id, number: 0, data: Vec::new(), is_last })
    }

    /// A blob carrying one frame of channel `id`.
    fn blob(id: ChannelId, is_last: bool) -> DaArtifactPayload {
        DaArtifactPayload::Blob(BlobPayload::new(vec![frame(id, is_last)]))
    }

    /// The artifacts the next lease takes, if any.
    fn lease(artifacts: &mut DaArtifacts) -> Option<Vec<ArtifactId>> {
        artifacts.lease(SubmissionId(0), MAX_BLOBS).map(|(_, artifact_ids)| artifact_ids)
    }

    /// A transaction ends at the first blob holding a channel's last frame, so the blobs after
    /// it, from a channel still open, wait.
    #[test]
    fn a_transaction_ends_at_a_channel_end() {
        let mut artifacts = DaArtifacts::new();
        let first = artifacts.push(blob(FIRST, false), vec![FIRST]);
        let end = artifacts.push(blob(FIRST, true), vec![FIRST, SECOND]);
        artifacts.push(blob(SECOND, false), vec![SECOND]);

        assert_eq!(lease(&mut artifacts), Some(vec![first, end]));
        assert_eq!(lease(&mut artifacts), None);
    }

    /// Blobs without a channel end are leased only once they fill a transaction.
    #[test]
    fn blobs_without_a_channel_end_wait_for_a_full_transaction() {
        let mut artifacts = DaArtifacts::new();
        let mut ids: Vec<_> =
            (1..MAX_BLOBS).map(|_| artifacts.push(blob(FIRST, false), vec![FIRST])).collect();
        assert_eq!(lease(&mut artifacts), None);

        ids.push(artifacts.push(blob(FIRST, false), vec![FIRST]));

        assert_eq!(lease(&mut artifacts), Some(ids));
    }

    /// A requeued transaction is leased again as it was, without the newer blobs after it.
    #[test]
    fn a_requeued_transaction_follows_the_same_rule() {
        let mut artifacts = DaArtifacts::new();
        artifacts.push(blob(FIRST, false), vec![FIRST]);
        artifacts.push(blob(FIRST, true), vec![FIRST]);
        let leased = lease(&mut artifacts).expect("the channel's blobs");
        artifacts.push(blob(SECOND, false), vec![SECOND]);

        artifacts.requeue(&leased);

        assert_eq!(lease(&mut artifacts), Some(leased));
        assert_eq!(lease(&mut artifacts), None);
    }

    /// A calldata frame ends the blob run before it, since no later blob may land ahead of it, so
    /// those blobs go out without a channel end.
    #[test]
    fn calldata_ends_the_blob_run_before_it() {
        let mut artifacts = DaArtifacts::new();
        let blob = artifacts.push(blob(FIRST, false), vec![FIRST]);
        let calldata = artifacts.push(DaArtifactPayload::Calldata(frame(FIRST, true)), vec![FIRST]);

        assert_eq!(lease(&mut artifacts), Some(vec![blob]));
        assert_eq!(lease(&mut artifacts), Some(vec![calldata]));
    }

    /// A blob run cut by calldata keeps its cut once the calldata is leased or confirmed, so the
    /// requeued blobs go out again as the same transaction instead of waiting for more output.
    #[test]
    fn a_run_cut_by_calldata_keeps_its_cut_when_requeued() {
        for confirm_calldata in [false, true] {
            let mut artifacts = DaArtifacts::new();
            artifacts.push(blob(FIRST, false), vec![FIRST]);
            artifacts.push(DaArtifactPayload::Calldata(frame(FIRST, true)), vec![FIRST]);
            let blobs = lease(&mut artifacts).expect("the blobs before the calldata");
            let calldata = lease(&mut artifacts).expect("the calldata");
            artifacts.push(blob(SECOND, false), vec![SECOND]);
            if confirm_calldata {
                artifacts.confirm(&calldata);
            }

            artifacts.requeue(&blobs);

            assert_eq!(lease(&mut artifacts), Some(blobs));
            assert_eq!(lease(&mut artifacts), None);
        }
    }

    /// Pruning a channel removes it from every artifact and drops the artifacts left with no
    /// channel, so the ledger does not grow forever. An artifact another channel still shares
    /// stays, listing only that channel.
    #[test]
    fn prune_channels_drops_artifacts_left_without_a_channel() {
        let mut artifacts = DaArtifacts::new();
        artifacts.push(blob(FIRST, true), vec![FIRST, SECOND]);
        artifacts.push(blob(FIRST, true), vec![FIRST]);

        artifacts.prune_channels(&[FIRST]);

        let retained: Vec<_> = artifacts.into_iter().map(DaArtifact::channel_ids).collect();
        assert_eq!(retained, [[SECOND]]);
    }
}
