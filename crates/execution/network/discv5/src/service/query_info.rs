use enr::{NodeId, k256::sha2::digest::generic_array::GenericArray};
use smallvec::SmallVec;
use tokio::sync::oneshot;

use crate::{Enr, kbucket::Key, rpc::RequestBody};

/// Information about a query.
#[derive(Debug)]
pub struct QueryInfo {
    /// What we are querying and why.
    pub query_type: QueryType,

    /// Temporary ENRs used when trying to reach nodes.
    pub untrusted_enrs: SmallVec<[Enr; 16]>,

    /// A callback channel for the service that requested the query.
    pub callback: oneshot::Sender<Vec<Enr>>,

    /// The number of distances we request for each peer.
    /// NOTE: This must not be larger than 127.
    pub distances_to_request: usize,
}

/// Additional information about the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryType {
    /// The user requested a `FIND_NODE` query to be performed. It should be reported when finished.
    FindNode(NodeId),
}

impl QueryInfo {
    /// Builds an RPC Request, given the QueryInfo
    pub(crate) fn rpc_request(&self, peer: NodeId) -> RequestBody {
        match self.query_type {
            QueryType::FindNode(node_id) => {
                let distances = findnode_log2distance(node_id, peer, self.distances_to_request)
                    .unwrap_or_else(|| vec![0]);
                RequestBody::FindNode { distances }
            }
        }
    }
}

impl crate::query_pool::TargetKey<NodeId> for QueryInfo {
    fn key(&self) -> Key<NodeId> {
        match self.query_type {
            QueryType::FindNode(ref node_id) => {
                Key::new_raw(*node_id, *GenericArray::from_slice(&node_id.raw()))
            }
        }
    }
}

/// Calculates the log2 distance for a destination peer given a target and the size (number of
/// distances to request).
///
/// As the iteration increases, FINDNODE requests adjacent distances from the exact peer distance.
///
/// As an example, if the target has a distance of 12 from the remote peer, the sequence of distances that are sent for increasing iterations would be [12, 13, 11, 14, 10, .. ].
fn findnode_log2distance(target: NodeId, peer: NodeId, size: usize) -> Option<Vec<u64>> {
    if size > 127 {
        // invoke and endless loop - coding error
        panic!("Iterations cannot be greater than 127");
    }

    let dst_key: Key<NodeId> = peer.into();
    let distance = dst_key.log2_distance(&target.into())?;

    let mut result_list = vec![distance];
    let mut difference = 1;
    while result_list.len() < size {
        if distance + difference <= 256 {
            result_list.push(distance + difference);
        }
        if result_list.len() < size {
            if let Some(d) = distance.checked_sub(difference) {
                result_list.push(d);
            }
        }
        difference += 1;
    }
    Some(result_list[..size].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log2distance() {
        let target = NodeId::new(&[0u8; 32]);
        let mut destination = [0u8; 32];
        destination[10] = 1; // gives a log2 distance of 169
        let destination = NodeId::new(&destination);

        let expected_distances = vec![169, 170, 168, 171, 167, 172, 166, 173, 165];

        assert_eq!(
            findnode_log2distance(target, destination, expected_distances.len()).unwrap(),
            expected_distances
        );
    }

    #[test]
    fn test_log2distance_lower() {
        let target = NodeId::new(&[0u8; 32]);
        let mut destination = [0u8; 32];
        destination[31] = 8; // gives a log2 distance of 5
        let destination = NodeId::new(&destination);

        let expected_distances = vec![4, 5, 3, 6, 2, 7, 1, 8, 0, 9, 10];

        assert_eq!(
            findnode_log2distance(target, destination, expected_distances.len()).unwrap(),
            expected_distances
        );
    }

    #[test]
    fn test_log2distance_upper() {
        let target = NodeId::new(&[0u8; 32]);
        let mut destination = [0u8; 32];
        destination[0] = 8; // gives a log2 distance of 252
        let destination = NodeId::new(&destination);

        let expected_distances = vec![252, 253, 251, 254, 250, 255, 249, 256, 248, 247, 246];

        assert_eq!(
            findnode_log2distance(target, destination, expected_distances.len()).unwrap(),
            expected_distances
        );
    }
}
