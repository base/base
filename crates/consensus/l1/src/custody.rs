use std::{collections::BTreeMap, fs, io, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::{B256, U256};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use ssz::Encode;
use tracing::{debug, warn};

use crate::{
    BeaconPeers, BeaconPeersError, DataColumnSidecar, DataColumnsByRange, DataColumnsByRoot,
    L1Node, LightClient,
};

/// Our share of data-column custody: the columns our node ID selects, fetched from supernodes for
/// every verified L1 block with blobs, verified down to that block, and kept on disk for the
/// retention window to serve to peers.
///
/// Each block is a directory named `<slot>_<root>` holding one raw SSZ sidecar per column.
///
/// ponytail: no backfill. Custody starts with the blocks seen since the store was first created.
#[derive(Debug, Clone)]
pub struct ColumnCustody {
    /// Directory of the stored blocks.
    pub dir: PathBuf,
    /// Columns we custody, sorted.
    pub columns: Vec<u64>,
    /// Root of every stored block, by slot.
    pub blocks: Arc<Mutex<BTreeMap<u64, B256>>>,
    /// First slot we advertise as served.
    pub earliest_slot: u64,
}

impl ColumnCustody {
    /// `CUSTODY_REQUIREMENT`: custody groups of a node without validators.
    pub const GROUPS: u64 = 4;
    /// `NUMBER_OF_CUSTODY_GROUPS`, one column each.
    pub const NUMBER_OF_GROUPS: u64 = 128;
    /// Slots kept, `MIN_EPOCHS_FOR_DATA_COLUMN_SIDECARS_REQUESTS`.
    pub const RETENTION_SLOTS: u64 = 4096 * LightClient::SLOTS_PER_EPOCH;
    /// Blocks served per request, `MAX_REQUEST_BLOCKS_DENEB`.
    pub const MAX_REQUEST_BLOCKS: u64 = 128;
    /// Recent L1 blocks checked for missing columns every slot.
    pub const WINDOW: usize = 64;

    /// Opens the store in `dir` for the columns of `node_id`, advertising `current_slot` onwards.
    pub fn open(dir: PathBuf, node_id: [u8; 32], current_slot: u64) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut blocks: BTreeMap<u64, B256> = BTreeMap::new();
        for entry in fs::read_dir(&dir)? {
            let name = entry?.file_name();
            if let Some((slot, root)) = name.to_str().and_then(|name| name.split_once('_'))
                && let (Ok(slot), Ok(root)) = (slot.parse(), root.parse())
            {
                blocks.insert(slot, root);
            }
        }
        let earliest_slot =
            blocks.first_key_value().map_or(current_slot, |(&slot, _)| slot.min(current_slot));
        Ok(Self {
            dir,
            columns: Self::columns_of(node_id, Self::GROUPS),
            blocks: Arc::new(Mutex::new(blocks)),
            earliest_slot,
        })
    }

    /// Returns the sorted custody columns of `node_id` for `groups` custody groups:
    /// `get_custody_groups` of the Fulu spec, with one column per group.
    pub fn columns_of(node_id: [u8; 32], groups: u64) -> Vec<u64> {
        let mut id = U256::from_be_bytes(node_id);
        let mut columns = Vec::new();
        while (columns.len() as u64) < groups.min(Self::NUMBER_OF_GROUPS) {
            let hash = Sha256::digest(id.to_le_bytes::<32>());
            let prefix = hash[..8].try_into().unwrap_or_default();
            let column = u64::from_le_bytes(prefix) % Self::NUMBER_OF_GROUPS;
            if !columns.contains(&column) {
                columns.push(column);
            }
            id = id.wrapping_add(U256::from(1));
        }
        columns.sort_unstable();
        columns
    }

    /// Stores the raw SSZ `sidecars` of the block `root` at `slot`, by column, replacing a block
    /// that was reorged out of the slot.
    pub fn insert(&self, slot: u64, root: B256, sidecars: &[(u64, Vec<u8>)]) -> io::Result<()> {
        let staging = self.dir.join(format!("{slot}.tmp"));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir(&staging)?;
        for (column, ssz) in sidecars {
            fs::write(staging.join(column.to_string()), ssz)?;
        }
        let replaced = self.blocks.lock().get(&slot).copied();
        if let Some(replaced) = replaced {
            fs::remove_dir_all(self.block_dir(slot, replaced))?;
        }
        fs::rename(&staging, self.block_dir(slot, root))?;
        self.blocks.lock().insert(slot, root);
        Ok(())
    }

    /// Deletes the blocks that fell out of the retention window at `current_slot`.
    pub fn prune(&self, current_slot: u64) -> io::Result<()> {
        let cutoff = current_slot.saturating_sub(Self::RETENTION_SLOTS);
        let expired = {
            let mut blocks = self.blocks.lock();
            let kept = blocks.split_off(&cutoff);
            std::mem::replace(&mut *blocks, kept)
        };
        for (slot, root) in expired {
            fs::remove_dir_all(self.block_dir(slot, root))?;
        }
        Ok(())
    }

    /// Returns the stored sidecars a `data_column_sidecars_by_range` request asks for, with their
    /// slots, ordered by slot and column.
    pub fn by_range(&self, request: &DataColumnsByRange) -> Vec<(u64, Vec<u8>)> {
        let end = request.start_slot.saturating_add(request.count.min(Self::MAX_REQUEST_BLOCKS));
        let blocks: Vec<_> = self
            .blocks
            .lock()
            .range(request.start_slot..end)
            .map(|(slot, root)| (*slot, *root))
            .collect();
        blocks
            .into_iter()
            .flat_map(|(slot, root)| self.read(slot, root, &request.columns))
            .collect()
    }

    /// Returns the stored sidecars a `data_column_sidecars_by_root` request asks for, with their
    /// slots.
    ///
    /// ponytail: finds each root by scanning stored blocks from the newest, which is quick for
    /// the recent blocks peers ask for; index roots if old lookups get common.
    pub fn by_root(&self, request: &[DataColumnsByRoot]) -> Vec<(u64, Vec<u8>)> {
        request
            .iter()
            .take(Self::MAX_REQUEST_BLOCKS as usize)
            .filter_map(|id| {
                let blocks = self.blocks.lock();
                let (slot, root) = blocks.iter().rev().find(|(_, root)| **root == id.block_root)?;
                Some(self.read(*slot, *root, &id.columns))
            })
            .flatten()
            .collect()
    }

    /// Custodies the columns of every verified L1 block with blobs, every `slot`, forever.
    ///
    /// A block's beacon root is the `parent_beacon_block_root` of its child, so each block waits
    /// for its child.
    ///
    /// ponytail: Fulu blocks only. Gloas sidecars carry no commitments, so checking them needs
    /// the block's bid; mainnet has no Gloas epoch yet, and the light client cannot follow one.
    pub async fn sync(self, peers: BeaconPeers, node: Arc<L1Node>, slot: Duration) {
        let schedule = &peers.network.schedule;
        loop {
            let pending: Vec<(u64, B256)> = {
                let chain = node.chain.read();
                let recent: Vec<_> =
                    chain.by_number.values().rev().take(Self::WINDOW + 1).collect();
                let blocks = self.blocks.lock();
                recent
                    .windows(2)
                    .filter_map(|pair| {
                        let (child, block) = (pair[0], pair[1]);
                        let root = child.parent_beacon_block_root?;
                        let slot = block.timestamp.checked_sub(schedule.genesis_time)?
                            / schedule.seconds_per_slot;
                        (block.blob_gas_used.unwrap_or_default() > 0
                            && child.parent_hash == block.hash()
                            && slot >= self.earliest_slot
                            && !schedule.is_gloas(slot)
                            && blocks.get(&slot) != Some(&root))
                        .then_some((slot, root))
                    })
                    .collect()
            };
            for (slot, root) in pending {
                match self.fetch(&peers, slot, root).await {
                    Ok(sidecars) => match self.insert(slot, root, &sidecars) {
                        Ok(()) => debug!(target: "l1", slot, "custodied data columns"),
                        Err(error) => {
                            warn!(target: "l1", error = %error, slot, "failed to store data columns")
                        }
                    },
                    Err(BeaconPeersError::NoPeers) => break,
                    Err(error) => {
                        debug!(target: "l1", error = %error, slot, "failed to fetch custody columns")
                    }
                }
            }
            if let Err(error) = self.prune(peers.network.current_slot()) {
                warn!(target: "l1", error = %error, "failed to prune data columns");
            }
            tokio::time::sleep(slot).await;
        }
    }

    /// Fetches our columns of the block `root` at `slot` from supernodes, keeping only sidecars
    /// that verify against `root`.
    async fn fetch(
        &self,
        peers: &BeaconPeers,
        slot: u64,
        root: B256,
    ) -> Result<Vec<(u64, Vec<u8>)>, BeaconPeersError> {
        let request =
            DataColumnsByRange { start_slot: slot, count: 1, columns: self.columns.clone() };
        let request = request.as_ssz_bytes();
        peers
            .request(&peers.supernodes, BeaconPeers::COLUMNS_BY_RANGE, Some(&request), |chunks| {
                let mut sidecars: Vec<(u64, Vec<u8>)> = chunks
                    .into_iter()
                    .filter_map(|(_, ssz)| {
                        let sidecar = DataColumnSidecar::from_ssz_bytes(&ssz, false).ok()?;
                        (self.columns.contains(&sidecar.index)
                            && sidecar.slot == slot
                            && sidecar.is_valid(root))
                        .then_some((sidecar.index, ssz))
                    })
                    .collect();
                sidecars.sort_by_key(|(column, _)| *column);
                sidecars.dedup_by_key(|(column, _)| *column);
                if sidecars.len() == self.columns.len() {
                    Ok(sidecars)
                } else {
                    Err(BeaconPeersError::Columns(format!(
                        "only {} of our {} columns of slot {slot} verified",
                        sidecars.len(),
                        self.columns.len()
                    )))
                }
            })
            .await
    }

    /// Reads the stored sidecars of `columns` we custody in the block `root` at `slot`.
    fn read(&self, slot: u64, root: B256, columns: &[u64]) -> Vec<(u64, Vec<u8>)> {
        let dir = self.block_dir(slot, root);
        self.columns
            .iter()
            .filter(|column| columns.contains(column))
            .filter_map(|column| fs::read(dir.join(column.to_string())).ok())
            .map(|ssz| (slot, ssz))
            .collect()
    }

    /// Returns the directory of the block `root` at `slot`.
    fn block_dir(&self, slot: u64, root: B256) -> PathBuf {
        self.dir.join(format!("{slot}_{root}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custody_columns_follow_the_node_id() {
        let columns = ColumnCustody::columns_of([7; 32], ColumnCustody::GROUPS);
        assert_eq!(columns.len(), 4);
        assert!(columns.is_sorted() && columns.iter().all(|column| *column < 128));
        assert_ne!(columns, ColumnCustody::columns_of([8; 32], ColumnCustody::GROUPS));
        assert_eq!(ColumnCustody::columns_of([0xff; 32], 128), (0..128).collect::<Vec<_>>());
    }

    #[test]
    fn stores_serves_and_prunes_blocks() {
        let dir = std::env::temp_dir().join(format!("custody-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let custody = ColumnCustody::open(dir.clone(), [7; 32], 10).unwrap();
        let [a, b, ..] = custody.columns[..] else { unreachable!() };
        let sidecars = |tag: u8| vec![(a, vec![tag, 0]), (b, vec![tag, 1])];
        let (old, reorged, kept) =
            (B256::repeat_byte(1), B256::repeat_byte(2), B256::repeat_byte(3));

        custody.insert(5, old, &sidecars(1)).unwrap();
        custody.insert(5, reorged, &sidecars(2)).unwrap();
        custody.insert(9, kept, &sidecars(3)).unwrap();
        let range = DataColumnsByRange { start_slot: 0, count: 10, columns: vec![b, a, 200] };
        assert_eq!(
            custody.by_range(&range),
            vec![(5, vec![2, 0]), (5, vec![2, 1]), (9, vec![3, 0]), (9, vec![3, 1])]
        );
        let by_root =
            |root| custody.by_root(&[DataColumnsByRoot { block_root: root, columns: vec![b] }]);
        assert!(by_root(old).is_empty());
        assert_eq!(by_root(reorged), vec![(5, vec![2, 1])]);

        let reopened = ColumnCustody::open(dir.clone(), [7; 32], 20).unwrap();
        assert_eq!(reopened.earliest_slot, 5);
        assert_eq!(*reopened.blocks.lock(), BTreeMap::from([(5, reorged), (9, kept)]));
        reopened.prune(ColumnCustody::RETENTION_SLOTS + 6).unwrap();
        assert_eq!(*reopened.blocks.lock(), BTreeMap::from([(9, kept)]));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
