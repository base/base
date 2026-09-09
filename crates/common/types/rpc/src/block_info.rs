//! Block-reference conversions for RPC responses.

use crate::Block as RpcBlock;
use base_common_types_chain::BlockInfo;

impl<T> From<RpcBlock<T>> for BlockInfo {
    fn from(block: RpcBlock<T>) -> Self {
        Self {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        }
    }
}

impl<T> From<&RpcBlock<T>> for BlockInfo {
    fn from(block: &RpcBlock<T>) -> Self {
        Self {
            hash: block.header.hash_slow(),
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.timestamp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;
    use base_common_types_chain::BaseTxEnvelope;

    #[test]
    fn test_rpc_block_into_info() {
        let block: crate::Block<BaseTxEnvelope> = crate::Block {
            header: crate::Header {
                hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
                inner: base_common_types_chain::Header {
                    number: 1,
                    parent_hash: b256!(
                        "0202020202020202020202020202020202020202020202020202020202020202"
                    ),
                    timestamp: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let expected = BlockInfo {
            hash: b256!("04d6fefc87466405ba0e5672dcf5c75325b33e5437da2a42423080aab8be889b"),
            number: 1,
            parent_hash: b256!("0202020202020202020202020202020202020202020202020202020202020202"),
            timestamp: 1,
        };
        assert_eq!(BlockInfo::from(&block), expected);
        assert_eq!(BlockInfo::from(block), expected);
    }
}
