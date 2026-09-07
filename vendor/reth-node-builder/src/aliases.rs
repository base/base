use base_common_consensus::{BaseBlock, BaseReceipt, BaseTxEnvelope};
use reth_provider::BlockReader;

/// Block reader bound for the concrete Base wire types.
pub trait BlockReaderFor:
    BlockReader<
        Block = BaseBlock,
        Header = alloy_consensus::Header,
        Transaction = BaseTxEnvelope,
        Receipt = BaseReceipt,
    >
{
}

impl<T> BlockReaderFor for T where
    T: BlockReader<
            Block = BaseBlock,
            Header = alloy_consensus::Header,
            Transaction = BaseTxEnvelope,
            Receipt = BaseReceipt,
        >
{
}
