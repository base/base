use alloy_eips::eip4844::{c_kzg, env_settings::EnvKzgSettings};
use alloy_primitives::{Address, B256, Bloom, Bytes, FixedBytes, U256};
use serde::{Deserialize, Serialize};
use ssz::{Decode, DecodeError, SszDecoderBuilder};

use crate::{LightClient, Ssz};

/// A BLS public key.
pub type BlsPublicKey = FixedBytes<48>;

/// A BLS signature.
pub type BlsSignature = FixedBytes<96>;

/// A beacon block header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ssz_derive::Decode)]
pub struct BeaconHeader {
    /// Slot of the block.
    pub slot: u64,
    /// Index of the proposer.
    pub proposer_index: u64,
    /// Root of the parent block.
    pub parent_root: B256,
    /// Root of the post-state.
    pub state_root: B256,
    /// Root of the block body.
    pub body_root: B256,
}

impl BeaconHeader {
    /// Returns the SSZ hash tree root, which is the beacon block root.
    pub fn tree_root(&self) -> B256 {
        Ssz::merkleize(
            &[
                Ssz::uint64(self.slot),
                Ssz::uint64(self.proposer_index),
                self.parent_root,
                self.state_root,
                self.body_root,
            ],
            5,
        )
    }
}

/// An execution payload header in the Deneb (and later) layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ssz_derive::Decode)]
pub struct ExecutionHeader {
    /// Parent execution block hash.
    pub parent_hash: B256,
    /// Fee recipient.
    pub fee_recipient: Address,
    /// Execution state root.
    pub state_root: B256,
    /// Receipts root.
    pub receipts_root: B256,
    /// Logs bloom.
    pub logs_bloom: Bloom,
    /// RANDAO mix.
    pub prev_randao: B256,
    /// Execution block number.
    pub block_number: u64,
    /// Gas limit.
    pub gas_limit: u64,
    /// Gas used.
    pub gas_used: u64,
    /// Block timestamp.
    pub timestamp: u64,
    /// Extra data.
    pub extra_data: Bytes,
    /// Base fee per gas.
    pub base_fee_per_gas: U256,
    /// Execution block hash.
    pub block_hash: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Withdrawals root.
    pub withdrawals_root: B256,
    /// Blob gas used.
    pub blob_gas_used: u64,
    /// Excess blob gas.
    pub excess_blob_gas: u64,
}

impl ExecutionHeader {
    /// Returns the SSZ hash tree root.
    pub fn tree_root(&self) -> B256 {
        let mut fee_recipient = B256::ZERO;
        fee_recipient[..20].copy_from_slice(self.fee_recipient.as_slice());
        Ssz::merkleize(
            &[
                self.parent_hash,
                fee_recipient,
                self.state_root,
                self.receipts_root,
                Ssz::merkleize(&Ssz::pack(self.logs_bloom.as_slice()), 8),
                self.prev_randao,
                Ssz::uint64(self.block_number),
                Ssz::uint64(self.gas_limit),
                Ssz::uint64(self.gas_used),
                Ssz::uint64(self.timestamp),
                Ssz::byte_list(&self.extra_data, 32),
                B256::from(self.base_fee_per_gas.to_le_bytes::<32>()),
                self.block_hash,
                self.transactions_root,
                self.withdrawals_root,
                Ssz::uint64(self.blob_gas_used),
                Ssz::uint64(self.excess_blob_gas),
            ],
            17,
        )
    }
}

/// A light-client header: a beacon header plus the execution block it commits to.
///
/// Servers send the full execution header for pre-Gloas blocks and only the execution block hash
/// from Gloas on; exactly one of the two is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LightClientHeader {
    /// The beacon block header.
    pub beacon: BeaconHeader,
    /// The execution payload header (pre-Gloas format).
    pub execution: Option<ExecutionHeader>,
    /// The execution block hash (Gloas format).
    pub execution_block_hash: Option<B256>,
    /// Merkle branch of the execution data in the beacon block body.
    pub execution_branch: Vec<B256>,
}

impl LightClientHeader {
    /// Generalized index of `execution_payload` in `BeaconBlockBody` (Capella to Fulu).
    pub const EXECUTION_PAYLOAD_GINDEX: u64 = 25;
    /// Generalized index of `signed_execution_payload_bid.message.parent_block_hash` in
    /// `BeaconBlockBody` (Gloas).
    pub const EXECUTION_BLOCK_HASH_GINDEX_GLOAS: u64 = 2856;
    /// SSZ length of a Gloas header, which is fixed-size.
    pub const GLOAS_SSZ_LEN: usize =
        112 + 32 + Ssz::branch_len(Self::EXECUTION_BLOCK_HASH_GINDEX_GLOAS);

    /// Decodes an SSZ header in the Electra (also used by Fulu) or Gloas layout.
    pub fn from_ssz_bytes(bytes: &[u8], gloas: bool) -> Result<Self, DecodeError> {
        let mut builder = SszDecoderBuilder::new(bytes);
        builder.register_type::<BeaconHeader>()?;
        if gloas {
            builder.register_type::<B256>()?;
            builder.register_type_parameterized(
                true,
                Ssz::branch_len(Self::EXECUTION_BLOCK_HASH_GINDEX_GLOAS),
            )?;
        } else {
            builder.register_type::<ExecutionHeader>()?;
            builder.register_type_parameterized(
                true,
                Ssz::branch_len(Self::EXECUTION_PAYLOAD_GINDEX),
            )?;
        }
        let mut decoder = builder.build()?;
        let beacon = decoder.decode_next()?;
        let (execution, execution_block_hash) = if gloas {
            (None, Some(decoder.decode_next()?))
        } else {
            (Some(decoder.decode_next()?), None)
        };
        let execution_branch = decoder.decode_next_with(|bytes| Ok(Ssz::chunks(bytes)))?;
        Ok(Self { beacon, execution, execution_block_hash, execution_branch })
    }

    /// Registers a header field with an enclosing container's decoder.
    fn register(builder: &mut SszDecoderBuilder<'_>, gloas: bool) -> Result<(), DecodeError> {
        if gloas {
            builder.register_type_parameterized(true, Self::GLOAS_SSZ_LEN)
        } else {
            builder.register_anonymous_variable_length_item()
        }
    }

    /// Returns the execution block hash this header commits to.
    ///
    /// From Gloas this is the parent of the block's own payload, so it trails the beacon slot by
    /// one block.
    pub fn block_hash(&self) -> B256 {
        self.execution_block_hash
            .or_else(|| self.execution.as_ref().map(|execution| execution.block_hash))
            .unwrap_or_default()
    }

    /// Checks that the execution data is committed to by the beacon block body.
    pub fn is_valid(&self, gloas: bool) -> bool {
        let body = self.beacon.body_root;
        match (&self.execution, self.execution_block_hash) {
            (Some(execution), None) if !gloas => Ssz::is_valid_branch(
                execution.tree_root(),
                &self.execution_branch,
                Self::EXECUTION_PAYLOAD_GINDEX,
                body,
            ),
            (None, Some(hash)) if gloas => Ssz::is_valid_branch(
                hash,
                &self.execution_branch,
                Self::EXECUTION_BLOCK_HASH_GINDEX_GLOAS,
                body,
            ),
            _ => false,
        }
    }
}

/// A sync committee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCommittee {
    /// Member public keys.
    pub pubkeys: Vec<BlsPublicKey>,
    /// Aggregate public key of all members.
    pub aggregate_pubkey: BlsPublicKey,
}

impl SyncCommittee {
    /// Number of members in a sync committee.
    pub const SIZE: usize = 512;
    /// SSZ length of a sync committee.
    pub const SSZ_LEN: usize = (Self::SIZE + 1) * 48;

    /// Decodes an SSZ sync committee: 512 member keys followed by the aggregate key.
    pub fn from_ssz_bytes(bytes: &[u8]) -> Self {
        let mut pubkeys: Vec<BlsPublicKey> =
            bytes.chunks_exact(48).map(BlsPublicKey::from_slice).collect();
        let aggregate_pubkey = pubkeys.pop().unwrap_or_default();
        Self { pubkeys, aggregate_pubkey }
    }

    /// Returns the SSZ hash tree root.
    pub fn tree_root(&self) -> B256 {
        let pubkeys: Vec<B256> = self.pubkeys.iter().map(Self::pubkey_root).collect();
        Ssz::merkleize(
            &[Ssz::merkleize(&pubkeys, Self::SIZE), Self::pubkey_root(&self.aggregate_pubkey)],
            2,
        )
    }

    fn pubkey_root(key: &BlsPublicKey) -> B256 {
        Ssz::merkleize(&Ssz::pack(key.as_slice()), 2)
    }
}

/// Sync committee participation bits and aggregate signature.
#[derive(Debug, Clone, PartialEq, Eq, ssz_derive::Decode)]
pub struct SyncAggregate {
    /// Participation bitvector.
    pub sync_committee_bits: FixedBytes<64>,
    /// Aggregate BLS signature of the participants.
    pub sync_committee_signature: BlsSignature,
}

impl SyncAggregate {
    /// Returns whether committee member `index` signed.
    pub fn participated(&self, index: usize) -> bool {
        (self.sync_committee_bits[index / 8] >> (index % 8)) & 1 == 1
    }

    /// Returns the number of participants.
    pub fn participants(&self) -> usize {
        self.sync_committee_bits.iter().map(|byte| byte.count_ones() as usize).sum()
    }
}

/// Light-client bootstrap for a trusted block root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightClientBootstrap {
    /// Header of the trusted block.
    pub header: LightClientHeader,
    /// Sync committee for the block's period.
    pub current_sync_committee: SyncCommittee,
    /// Merkle branch of the current sync committee in the block's state.
    pub current_sync_committee_branch: Vec<B256>,
}

impl LightClientBootstrap {
    /// Decodes an SSZ bootstrap in the Electra or Gloas layout.
    pub fn from_ssz_bytes(bytes: &[u8], gloas: bool) -> Result<Self, DecodeError> {
        let gindex = LightClient::CURRENT_SYNC_COMMITTEE_GINDEX[usize::from(gloas)];
        let mut builder = SszDecoderBuilder::new(bytes);
        LightClientHeader::register(&mut builder, gloas)?;
        builder.register_type_parameterized(true, SyncCommittee::SSZ_LEN)?;
        builder.register_type_parameterized(true, Ssz::branch_len(gindex))?;
        let mut decoder = builder.build()?;
        Ok(Self {
            header: decoder
                .decode_next_with(|bytes| LightClientHeader::from_ssz_bytes(bytes, gloas))?,
            current_sync_committee: decoder
                .decode_next_with(|bytes| Ok(SyncCommittee::from_ssz_bytes(bytes)))?,
            current_sync_committee_branch: decoder
                .decode_next_with(|bytes| Ok(Ssz::chunks(bytes)))?,
        })
    }
}

/// Which light-client update container a payload holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightClientUpdateKind {
    /// `LightClientUpdate`, with the next sync committee and finality proof.
    Full,
    /// `LightClientFinalityUpdate`, with the finality proof only.
    Finality,
    /// `LightClientOptimisticUpdate`, with just the signed attested header.
    Optimistic,
}

/// A light-client update. Finality and optimistic updates are updates with the optional fields
/// absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightClientUpdate {
    /// Header the sync committee signed.
    pub attested_header: LightClientHeader,
    /// Next sync committee, proven against the attested state.
    pub next_sync_committee: Option<SyncCommittee>,
    /// Merkle branch of the next sync committee.
    pub next_sync_committee_branch: Vec<B256>,
    /// Finalized header, proven against the attested state.
    pub finalized_header: Option<LightClientHeader>,
    /// Merkle branch of the finalized checkpoint root.
    pub finality_branch: Vec<B256>,
    /// Signature over the attested header.
    pub sync_aggregate: SyncAggregate,
    /// Slot at which the signature was included.
    pub signature_slot: u64,
}

impl LightClientUpdate {
    /// Decodes an SSZ update of `kind` in the Electra or Gloas layout.
    pub fn from_ssz_bytes(
        bytes: &[u8],
        gloas: bool,
        kind: LightClientUpdateKind,
    ) -> Result<Self, DecodeError> {
        let fork = usize::from(gloas);
        let committee = kind == LightClientUpdateKind::Full;
        let finality = kind != LightClientUpdateKind::Optimistic;
        let mut builder = SszDecoderBuilder::new(bytes);
        LightClientHeader::register(&mut builder, gloas)?;
        if committee {
            builder.register_type_parameterized(true, SyncCommittee::SSZ_LEN)?;
            builder.register_type_parameterized(
                true,
                Ssz::branch_len(LightClient::NEXT_SYNC_COMMITTEE_GINDEX[fork]),
            )?;
        }
        if finality {
            LightClientHeader::register(&mut builder, gloas)?;
            builder.register_type_parameterized(
                true,
                Ssz::branch_len(LightClient::FINALIZED_ROOT_GINDEX[fork]),
            )?;
        }
        builder.register_type::<SyncAggregate>()?;
        builder.register_type::<u64>()?;

        let mut decoder = builder.build()?;
        let header = |bytes: &[u8]| LightClientHeader::from_ssz_bytes(bytes, gloas);
        let branch = |bytes: &[u8]| Ok(Ssz::chunks(bytes));
        let attested_header = decoder.decode_next_with(header)?;
        let (next_sync_committee, next_sync_committee_branch) = if committee {
            (
                Some(decoder.decode_next_with(|bytes| Ok(SyncCommittee::from_ssz_bytes(bytes)))?),
                decoder.decode_next_with(branch)?,
            )
        } else {
            (None, Vec::new())
        };
        let (finalized_header, finality_branch) = if finality {
            (Some(decoder.decode_next_with(header)?), decoder.decode_next_with(branch)?)
        } else {
            (None, Vec::new())
        };
        Ok(Self {
            attested_header,
            next_sync_committee,
            next_sync_committee_branch,
            finalized_header,
            finality_branch,
            sync_aggregate: decoder.decode_next()?,
            signature_slot: decoder.decode_next()?,
        })
    }
}

/// A `DataColumnSidecar`: one cell of every blob in a block, with its proofs and, before Gloas,
/// the block header and the blobs' KZG commitments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataColumnSidecar {
    /// Column index.
    pub index: u64,
    /// The column's cells, one per blob, concatenated.
    pub cells: Vec<u8>,
    /// KZG commitment of each blob. Empty from Gloas, which moved them into the block's bid.
    pub kzg_commitments: Vec<FixedBytes<48>>,
    /// KZG proof of each cell.
    pub kzg_proofs: Vec<FixedBytes<48>>,
    /// Slot of the block.
    pub slot: u64,
    /// Header of the block, before Gloas. Its signature is not kept.
    pub header: Option<BeaconHeader>,
    /// Branch from the commitments to the header's body root, before Gloas.
    pub inclusion_proof: Vec<B256>,
}

impl DataColumnSidecar {
    /// Length of one cell.
    pub const CELL_LEN: usize = 2048;
    /// `MAX_BLOB_COMMITMENTS_PER_BLOCK` from Deneb.
    pub const MAX_BLOB_COMMITMENTS: usize = 4096;
    /// Generalized index of `blob_kzg_commitments` in a Deneb to Fulu `BeaconBlockBody`.
    pub const COMMITMENTS_GINDEX: u64 = 27;
    /// SSZ length of a `BeaconBlockHeader`.
    const HEADER_LEN: usize = 112;
    /// SSZ length of a `SignedBeaconBlockHeader`.
    const SIGNED_HEADER_LEN: usize = Self::HEADER_LEN + 96;

    /// Decodes an SSZ sidecar in the Fulu or the `gloas` layout.
    pub fn from_ssz_bytes(bytes: &[u8], gloas: bool) -> Result<Self, DecodeError> {
        let mut builder = SszDecoderBuilder::new(bytes);
        builder.register_type::<u64>()?;
        builder.register_anonymous_variable_length_item()?;
        builder.register_anonymous_variable_length_item()?;
        if gloas {
            builder.register_type::<u64>()?;
            builder.register_type::<B256>()?;
        } else {
            builder.register_anonymous_variable_length_item()?;
            builder.register_type_parameterized(true, Self::SIGNED_HEADER_LEN)?;
            builder.register_type_parameterized(true, 4 * 32)?;
        }
        let mut decoder = builder.build()?;
        let raw = |bytes: &[u8]| Ok(bytes.to_vec());
        let index = decoder.decode_next()?;
        let cells: Vec<u8> = decoder.decode_next_with(raw)?;
        let (commitments, proofs, slot, header, inclusion_proof): (Vec<u8>, Vec<u8>, _, _, _) =
            if gloas {
                let proofs = decoder.decode_next_with(raw)?;
                (Vec::new(), proofs, decoder.decode_next()?, None, Vec::new())
            } else {
                let commitments = decoder.decode_next_with(raw)?;
                let proofs = decoder.decode_next_with(raw)?;
                let header: BeaconHeader = decoder
                    .decode_next_with(|bytes| Decode::from_ssz_bytes(&bytes[..Self::HEADER_LEN]))?;
                let inclusion_proof = decoder.decode_next_with(|bytes| Ok(Ssz::chunks(bytes)))?;
                (commitments, proofs, header.slot, Some(header), inclusion_proof)
            };
        let blobs = cells.len() / Self::CELL_LEN;
        if blobs == 0
            || blobs > Self::MAX_BLOB_COMMITMENTS
            || index >= 128
            || !cells.len().is_multiple_of(Self::CELL_LEN)
            || proofs.len() != blobs * 48
            || (!gloas && commitments.len() != blobs * 48)
        {
            return Err(DecodeError::BytesInvalid("one cell per blob".into()));
        }
        let entries = |bytes: Vec<u8>| bytes.chunks_exact(48).map(FixedBytes::from_slice).collect();
        Ok(Self {
            index,
            cells,
            kzg_commitments: entries(commitments),
            kzg_proofs: entries(proofs),
            slot,
            header,
            inclusion_proof,
        })
    }

    /// Returns the number of blobs in the block.
    pub const fn blob_count(&self) -> usize {
        self.cells.len() / Self::CELL_LEN
    }

    /// Checks a Fulu sidecar against `block_root`: its header must hash to it, the inclusion
    /// proof must tie the commitments to the header's body, and every cell must match its blob's
    /// commitment through its KZG proof.
    ///
    /// ponytail: the header signature is not checked, as that needs the proposer's key from the
    /// beacon state. A supernode forging it only gets us penalized by the peers we serve.
    pub fn is_valid(&self, block_root: B256) -> bool {
        let Some(header) = &self.header else { return false };
        let leaves: Vec<B256> = self
            .kzg_commitments
            .iter()
            .map(|commitment| Ssz::merkleize(&Ssz::pack(commitment.as_slice()), 2))
            .collect();
        let commitments_root = Ssz::hash_pair(
            &Ssz::merkleize(&leaves, Self::MAX_BLOB_COMMITMENTS),
            &Ssz::uint64(leaves.len() as u64),
        );
        let bytes48 = |entries: &[FixedBytes<48>]| -> Vec<c_kzg::Bytes48> {
            entries.iter().map(|entry| c_kzg::Bytes48::new(entry.0)).collect()
        };
        let cells: Result<Vec<_>, _> =
            self.cells.chunks_exact(Self::CELL_LEN).map(c_kzg::Cell::from_bytes).collect();
        header.tree_root() == block_root
            && Ssz::is_valid_branch(
                commitments_root,
                &self.inclusion_proof,
                Self::COMMITMENTS_GINDEX,
                header.body_root,
            )
            && cells.is_ok_and(|cells| {
                EnvKzgSettings::Default
                    .get()
                    .verify_cell_kzg_proof_batch(
                        &bytes48(&self.kzg_commitments),
                        &vec![self.index; cells.len()],
                        &cells,
                        &bytes48(&self.kzg_proofs),
                    )
                    .unwrap_or(false)
            })
    }
}

/// A `DataColumnSidecarsByRangeRequest`.
#[derive(Debug, Clone, PartialEq, Eq, ssz_derive::Encode, ssz_derive::Decode)]
pub struct DataColumnsByRange {
    /// First slot.
    pub start_slot: u64,
    /// Number of slots.
    pub count: u64,
    /// Requested column indices.
    pub columns: Vec<u64>,
}

/// A `DataColumnsByRootIdentifier`, one entry of a `data_column_sidecars_by_root` request.
#[derive(Debug, Clone, PartialEq, Eq, ssz_derive::Encode, ssz_derive::Decode)]
pub struct DataColumnsByRoot {
    /// Block root.
    pub block_root: B256,
    /// Requested column indices.
    pub columns: Vec<u64>,
}

/// One entry of a beacon fork schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fork {
    /// Fork version active from `epoch`.
    pub current_version: FixedBytes<4>,
    /// Activation epoch.
    pub epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOTSTRAP: &[u8] = include_bytes!("../testdata/bootstrap.ssz");
    const CHECKPOINT: B256 = alloy_primitives::b256!(
        "0x32d4bea31eb4de8f8c0f6728a08ecddc600eb575b055ff04c057ad7821e257c1"
    );

    #[test]
    fn mainnet_header_roots_match() {
        let bootstrap = LightClientBootstrap::from_ssz_bytes(BOOTSTRAP, false).unwrap();
        let header = &bootstrap.header;

        assert_eq!(header.beacon.tree_root(), CHECKPOINT);
        assert!(header.is_valid(false));
        assert!(LightClientBootstrap::from_ssz_bytes(BOOTSTRAP, true).is_err());
        assert!(LightClientBootstrap::from_ssz_bytes(&BOOTSTRAP[1..], false).is_err());
    }

    #[test]
    fn mainnet_column_verifies_against_its_block() {
        let root = alloy_primitives::b256!(
            "0x9b939f59113f8176c1fb122847b0437816ab4eb04931056a742693242521076f"
        );
        let column =
            DataColumnSidecar::from_ssz_bytes(include_bytes!("../testdata/column.ssz"), false)
                .unwrap();
        assert_eq!((column.index, column.slot), (0, 15_525_001));
        assert!(column.blob_count() > 0);
        assert!(column.is_valid(root));
        assert!(!column.is_valid(B256::ZERO));

        let mut tampered = column.clone();
        tampered.cells[1] ^= 1;
        assert!(!tampered.is_valid(root));
        let mut tampered = column;
        tampered.inclusion_proof[0] = B256::ZERO;
        assert!(!tampered.is_valid(root));
    }
}
