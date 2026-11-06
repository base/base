use std::path::Path;

use alloy_primitives::B256;
use blst::{
    BLST_ERROR,
    min_pk::{PublicKey, Signature},
};

use crate::{
    BlsPublicKey, Fork, LightClientBootstrap, LightClientHeader, LightClientUpdate, Ssz,
    SyncAggregate, SyncCommittee,
};

/// Errors raised while verifying light-client data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LightClientError {
    /// The bootstrap header does not hash to the trusted checkpoint.
    #[error("bootstrap header root {actual} does not match checkpoint {expected}")]
    CheckpointMismatch {
        /// The trusted checkpoint root.
        expected: B256,
        /// The root of the received header.
        actual: B256,
    },
    /// An execution header is not committed to by its beacon block.
    #[error("invalid execution payload branch")]
    InvalidExecutionBranch,
    /// A sync committee is not committed to by the beacon state.
    #[error("invalid sync committee branch")]
    InvalidSyncCommitteeBranch,
    /// The finalized header is not committed to by the attested state.
    #[error("invalid finality branch")]
    InvalidFinalityBranch,
    /// A sync committee does not contain 512 valid public keys.
    #[error("invalid sync committee public keys")]
    InvalidSyncCommittee,
    /// Too few sync committee members signed.
    #[error("insufficient sync committee participation: {0}")]
    InsufficientParticipation(usize),
    /// The aggregate signature does not verify.
    #[error("invalid sync committee signature")]
    InvalidSignature,
    /// The update is signed by a committee the client does not know.
    #[error("no known sync committee for period {0}")]
    UnknownSyncCommittee(u64),
    /// The update's slots are inconsistent.
    #[error("inconsistent update slots")]
    InvalidSlots,
}

/// Fork versions, genesis and slot timing of a beacon chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSchedule {
    /// Root of the genesis validator set.
    pub genesis_validators_root: B256,
    /// Unix time of slot 0.
    pub genesis_time: u64,
    /// Slot duration in seconds.
    pub seconds_per_slot: u64,
    /// Fork versions with their activation epochs.
    pub forks: Vec<Fork>,
    /// First epoch of Gloas, `u64::MAX` while unscheduled.
    pub gloas_epoch: u64,
}

impl ForkSchedule {
    const DOMAIN_SYNC_COMMITTEE: [u8; 4] = [7, 0, 0, 0];

    /// Returns whether `slot` is in Gloas or later.
    pub const fn is_gloas(&self, slot: u64) -> bool {
        slot / LightClient::SLOTS_PER_EPOCH >= self.gloas_epoch
    }

    /// Returns `compute_fork_data_root` for the fork active at `epoch`.
    pub fn fork_data_root(&self, epoch: u64) -> B256 {
        let version = self
            .forks
            .iter()
            .filter(|fork| fork.epoch <= epoch)
            .max_by_key(|fork| fork.epoch)
            .map(|fork| fork.current_version)
            .unwrap_or_default();
        let mut version_chunk = B256::ZERO;
        version_chunk[..4].copy_from_slice(version.as_slice());
        Ssz::hash_pair(&version_chunk, &self.genesis_validators_root)
    }

    /// Returns the sync committee signing domain for a signature included at `signature_slot`.
    pub fn sync_committee_domain(&self, signature_slot: u64) -> B256 {
        let epoch = signature_slot.max(1).saturating_sub(1) / LightClient::SLOTS_PER_EPOCH;
        let fork_data_root = self.fork_data_root(epoch);

        let mut domain = B256::ZERO;
        domain[..4].copy_from_slice(&Self::DOMAIN_SYNC_COMMITTEE);
        domain[4..].copy_from_slice(&fork_data_root[..28]);
        domain
    }
}

/// Decompressed and validated sync committee public keys.
#[derive(Debug, Clone)]
pub struct SyncCommitteeKeys {
    /// Member public keys in committee order.
    pub keys: Vec<PublicKey>,
}

impl SyncCommitteeKeys {
    /// BLS ciphersuite used by the beacon chain.
    const DST: &'static [u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

    /// Validates every public key of `pubkeys`.
    pub fn new(pubkeys: &[BlsPublicKey]) -> Result<Self, LightClientError> {
        if pubkeys.len() != SyncCommittee::SIZE {
            return Err(LightClientError::InvalidSyncCommittee);
        }
        let keys = pubkeys
            .iter()
            .map(|key| PublicKey::key_validate(key.as_slice()))
            .collect::<Result<_, _>>()
            .map_err(|_| LightClientError::InvalidSyncCommittee)?;
        Ok(Self { keys })
    }

    /// Verifies that a supermajority of the committee signed `signing_root`.
    pub fn verify(
        &self,
        aggregate: &SyncAggregate,
        signing_root: B256,
    ) -> Result<(), LightClientError> {
        let participants = aggregate.participants();
        if participants * 3 < SyncCommittee::SIZE * 2 {
            return Err(LightClientError::InsufficientParticipation(participants));
        }
        let signers: Vec<&PublicKey> = self
            .keys
            .iter()
            .enumerate()
            .filter(|(index, _)| aggregate.participated(*index))
            .map(|(_, key)| key)
            .collect();
        let signature = Signature::from_bytes(aggregate.sync_committee_signature.as_slice())
            .map_err(|_| LightClientError::InvalidSignature)?;
        match signature.fast_aggregate_verify(true, signing_root.as_slice(), Self::DST, &signers) {
            BLST_ERROR::BLST_SUCCESS => Ok(()),
            _ => Err(LightClientError::InvalidSignature),
        }
    }
}

/// A beacon light client following the sync committee from a trusted checkpoint.
///
/// ponytail: no `force_update` path; if the chain stops finalizing for a whole sync committee
/// period the client stalls instead of accepting the best unfinalized update.
#[derive(Debug, Clone)]
pub struct LightClient {
    /// Fork schedule used to compute signing domains.
    pub schedule: ForkSchedule,
    /// Latest finalized header.
    pub finalized: LightClientHeader,
    /// Latest supermajority-signed header.
    pub optimistic: LightClientHeader,
    /// Committee for the finalized header's period.
    pub current_committee: SyncCommitteeKeys,
    /// Committee for the following period, once known.
    pub next_committee: Option<SyncCommitteeKeys>,
}

impl LightClient {
    /// Slots per epoch.
    pub const SLOTS_PER_EPOCH: u64 = 32;
    /// Slots per sync committee period.
    pub const SLOTS_PER_PERIOD: u64 = Self::SLOTS_PER_EPOCH * 256;

    // ponytail: pre-Electra states are not supported, so checkpoints must be Electra or later.
    /// Generalized index of the finalized root in `BeaconState`, for Electra/Fulu and Gloas
    /// (which reshuffles the state, EIP-7688).
    pub const FINALIZED_ROOT_GINDEX: [u64; 2] = [169, 735];
    /// Generalized index of the current sync committee, for Electra/Fulu and Gloas.
    pub const CURRENT_SYNC_COMMITTEE_GINDEX: [u64; 2] = [86, 2945];
    /// Generalized index of the next sync committee, for Electra/Fulu and Gloas.
    pub const NEXT_SYNC_COMMITTEE_GINDEX: [u64; 2] = [87, 2946];

    fn gindex(schedule: &ForkSchedule, slot: u64, gindices: [u64; 2]) -> u64 {
        gindices[usize::from(schedule.is_gloas(slot))]
    }

    /// Starts a light client from a bootstrap for the trusted `checkpoint` block root.
    pub fn bootstrap(
        schedule: ForkSchedule,
        checkpoint: B256,
        bootstrap: &LightClientBootstrap,
    ) -> Result<Self, LightClientError> {
        let actual = bootstrap.header.beacon.tree_root();
        if actual != checkpoint {
            return Err(LightClientError::CheckpointMismatch { expected: checkpoint, actual });
        }
        let slot = bootstrap.header.beacon.slot;
        if !bootstrap.header.is_valid(schedule.is_gloas(slot)) {
            return Err(LightClientError::InvalidExecutionBranch);
        }
        if !Ssz::is_valid_branch(
            bootstrap.current_sync_committee.tree_root(),
            &bootstrap.current_sync_committee_branch,
            Self::gindex(&schedule, slot, Self::CURRENT_SYNC_COMMITTEE_GINDEX),
            bootstrap.header.beacon.state_root,
        ) {
            return Err(LightClientError::InvalidSyncCommitteeBranch);
        }

        Ok(Self {
            schedule,
            finalized: bootstrap.header.clone(),
            optimistic: bootstrap.header.clone(),
            current_committee: SyncCommitteeKeys::new(&bootstrap.current_sync_committee.pubkeys)?,
            next_committee: None,
        })
    }

    /// Writes the finalized header and sync committees to `path`, replacing it atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let pubkeys = |committee: &SyncCommitteeKeys| -> Vec<BlsPublicKey> {
            committee.keys.iter().map(|key| key.compress().into()).collect()
        };
        let state = (
            &self.finalized,
            pubkeys(&self.current_committee),
            self.next_committee.as_ref().map(pubkeys),
        );
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&state)?)?;
        std::fs::rename(tmp, path)
    }

    /// Restores a client written by [`Self::save`], following `schedule`.
    ///
    /// The file is trusted like the node key beside it, so only the committee keys are checked.
    pub fn load(schedule: ForkSchedule, path: &Path) -> std::io::Result<Self> {
        let (finalized, current, next): (
            LightClientHeader,
            Vec<BlsPublicKey>,
            Option<Vec<BlsPublicKey>>,
        ) = serde_json::from_slice(&std::fs::read(path)?)?;
        let keys = |pubkeys: &[BlsPublicKey]| {
            SyncCommitteeKeys::new(pubkeys)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        };
        Ok(Self {
            schedule,
            optimistic: finalized.clone(),
            finalized,
            current_committee: keys(&current)?,
            next_committee: next.as_deref().map(keys).transpose()?,
        })
    }

    /// Returns the sync committee period of `slot`.
    pub const fn period(slot: u64) -> u64 {
        slot / Self::SLOTS_PER_PERIOD
    }

    /// Returns the sync committee period of the finalized header.
    pub const fn finalized_period(&self) -> u64 {
        Self::period(self.finalized.beacon.slot)
    }

    /// Verifies and applies a light-client update, returning whether a head advanced.
    pub fn apply(&mut self, update: &LightClientUpdate) -> Result<bool, LightClientError> {
        let attested = &update.attested_header;
        if update.signature_slot <= attested.beacon.slot {
            return Err(LightClientError::InvalidSlots);
        }
        let attested_slot = attested.beacon.slot;
        if !attested.is_valid(self.schedule.is_gloas(attested_slot)) {
            return Err(LightClientError::InvalidExecutionBranch);
        }

        let store_period = self.finalized_period();
        let signature_period = Self::period(update.signature_slot);
        let committee = if signature_period == store_period {
            &self.current_committee
        } else if signature_period == store_period + 1 {
            self.next_committee
                .as_ref()
                .ok_or(LightClientError::UnknownSyncCommittee(signature_period))?
        } else {
            return Err(LightClientError::UnknownSyncCommittee(signature_period));
        };

        if let Some(finalized) = &update.finalized_header {
            if finalized.beacon.slot > attested.beacon.slot {
                return Err(LightClientError::InvalidSlots);
            }
            if !finalized.is_valid(self.schedule.is_gloas(finalized.beacon.slot)) {
                return Err(LightClientError::InvalidExecutionBranch);
            }
            if !Ssz::is_valid_branch(
                finalized.beacon.tree_root(),
                &update.finality_branch,
                Self::gindex(&self.schedule, attested_slot, Self::FINALIZED_ROOT_GINDEX),
                attested.beacon.state_root,
            ) {
                return Err(LightClientError::InvalidFinalityBranch);
            }
        }
        if let Some(next) = &update.next_sync_committee
            && !Ssz::is_valid_branch(
                next.tree_root(),
                &update.next_sync_committee_branch,
                Self::gindex(&self.schedule, attested_slot, Self::NEXT_SYNC_COMMITTEE_GINDEX),
                attested.beacon.state_root,
            )
        {
            return Err(LightClientError::InvalidSyncCommitteeBranch);
        }

        let domain = self.schedule.sync_committee_domain(update.signature_slot);
        committee.verify(
            &update.sync_aggregate,
            Ssz::hash_pair(&attested.beacon.tree_root(), &domain),
        )?;

        let next_committee = update
            .next_sync_committee
            .as_ref()
            .map(|committee| SyncCommitteeKeys::new(&committee.pubkeys))
            .transpose()?;

        let mut advanced = false;
        if let Some(finalized) = &update.finalized_header
            && finalized.beacon.slot > self.finalized.beacon.slot
        {
            self.finalized = finalized.clone();
            if self.finalized_period() == store_period + 1
                && let Some(next) = self.next_committee.take()
            {
                self.current_committee = next;
            }
            advanced = true;
        }
        if self.next_committee.is_none()
            && Self::period(attested.beacon.slot) == self.finalized_period()
        {
            self.next_committee = next_committee;
        }
        if attested.beacon.slot > self.optimistic.beacon.slot {
            self.optimistic = attested.clone();
            advanced = true;
        }
        if self.finalized.beacon.slot > self.optimistic.beacon.slot {
            self.optimistic = self.finalized.clone();
        }
        Ok(advanced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BeaconNetwork, LightClientUpdateKind};

    const BOOTSTRAP: &[u8] = include_bytes!("../testdata/bootstrap.ssz");
    const FINALITY_UPDATE: &[u8] = include_bytes!("../testdata/finality_update.ssz");
    const UPDATE: &[u8] = include_bytes!("../testdata/update.ssz");
    const CHECKPOINT: B256 = alloy_primitives::b256!(
        "0x32d4bea31eb4de8f8c0f6728a08ecddc600eb575b055ff04c057ad7821e257c1"
    );

    fn mainnet_client() -> LightClient {
        let bootstrap = LightClientBootstrap::from_ssz_bytes(BOOTSTRAP, false).unwrap();
        LightClient::bootstrap(BeaconNetwork::mainnet().schedule, CHECKPOINT, &bootstrap).unwrap()
    }

    fn full_update() -> LightClientUpdate {
        LightClientUpdate::from_ssz_bytes(UPDATE, false, LightClientUpdateKind::Full).unwrap()
    }

    fn finality_update() -> LightClientUpdate {
        LightClientUpdate::from_ssz_bytes(FINALITY_UPDATE, false, LightClientUpdateKind::Finality)
            .unwrap()
    }

    #[test]
    fn follows_mainnet_sync_committee() {
        let mut client = mainnet_client();
        let mut without_finalized = full_update();
        without_finalized.finalized_header = None;
        without_finalized.finality_branch.clear();
        client.apply(&without_finalized).unwrap();
        assert!(client.next_committee.is_some());

        let mut client = mainnet_client();
        client.apply(&full_update()).unwrap();
        assert!(client.next_committee.is_some());

        let update = finality_update();
        assert!(client.apply(&update).unwrap());
        assert_eq!(client.optimistic.block_hash(), update.attested_header.block_hash());
        assert_eq!(client.finalized.beacon.tree_root(), CHECKPOINT);
    }

    #[test]
    fn restored_client_keeps_following() {
        let mut client = mainnet_client();
        client.apply(&full_update()).unwrap();
        let path = std::env::temp_dir().join(format!("light-client-{}.json", std::process::id()));
        client.save(&path).unwrap();

        let mut restored = LightClient::load(client.schedule.clone(), &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(restored.finalized, client.finalized);
        assert!(restored.next_committee.is_some());
        let update = finality_update();
        assert!(restored.apply(&update).unwrap());
        assert_eq!(restored.optimistic.block_hash(), update.attested_header.block_hash());
    }

    #[test]
    fn rejects_wrong_checkpoint() {
        let client = mainnet_client();
        let bootstrap = LightClientBootstrap::from_ssz_bytes(BOOTSTRAP, false).unwrap();
        let err = LightClient::bootstrap(client.schedule, B256::ZERO, &bootstrap).unwrap_err();
        assert!(matches!(err, LightClientError::CheckpointMismatch { .. }));
    }

    #[test]
    fn rejects_tampered_execution_header() {
        let mut client = mainnet_client();
        let mut update = finality_update();
        update.attested_header.execution.as_mut().unwrap().block_hash = B256::repeat_byte(1);
        assert_eq!(client.apply(&update), Err(LightClientError::InvalidExecutionBranch));
    }

    #[test]
    fn rejects_forged_signature() {
        let mut client = mainnet_client();
        let mut update = finality_update();
        update.attested_header.beacon.proposer_index += 1;
        assert_eq!(client.apply(&update), Err(LightClientError::InvalidSignature));

        let mut update = finality_update();
        update.sync_aggregate.sync_committee_bits[0] ^= 1;
        assert_eq!(client.apply(&update), Err(LightClientError::InvalidSignature));
    }

    #[test]
    fn rejects_next_period_without_committee() {
        let mut client = mainnet_client();
        let mut update = finality_update();
        update.signature_slot += LightClient::SLOTS_PER_PERIOD;
        assert_eq!(client.apply(&update), Err(LightClientError::UnknownSyncCommittee(1895)));
    }

    #[test]
    fn follows_sepolia_gloas_sync_committee() {
        const SEPOLIA_CHECKPOINT: B256 = alloy_primitives::b256!(
            "0x6e35bee76abcdeeb7ce56c5883030cd4a86c0beaf0dba4856b996061a4b363f7"
        );
        let bootstrap = LightClientBootstrap::from_ssz_bytes(
            include_bytes!("../testdata/sepolia/bootstrap.ssz"),
            true,
        )
        .unwrap();
        let update = LightClientUpdate::from_ssz_bytes(
            include_bytes!("../testdata/sepolia/update.ssz"),
            true,
            LightClientUpdateKind::Full,
        )
        .unwrap();
        let finality = LightClientUpdate::from_ssz_bytes(
            include_bytes!("../testdata/sepolia/finality_update.ssz"),
            true,
            LightClientUpdateKind::Finality,
        )
        .unwrap();
        let schedule = BeaconNetwork::sepolia().schedule;

        let mut client =
            LightClient::bootstrap(schedule.clone(), SEPOLIA_CHECKPOINT, &bootstrap).unwrap();
        client.apply(&update).unwrap();
        assert!(client.next_committee.is_some());
        assert!(client.apply(&finality).unwrap());
        assert_eq!(client.optimistic.block_hash(), finality.attested_header.block_hash());

        let mut tampered = finality;
        tampered.attested_header.execution_block_hash = Some(B256::repeat_byte(1));
        assert_eq!(client.apply(&tampered), Err(LightClientError::InvalidExecutionBranch));

        let pre_gloas = ForkSchedule { gloas_epoch: u64::MAX, ..schedule };
        assert!(LightClient::bootstrap(pre_gloas, SEPOLIA_CHECKPOINT, &bootstrap).is_err());
    }
}
