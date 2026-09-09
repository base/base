use base_common_types_chain::{BaseBlock, BaseTxEnvelope};
use base_execution_state_provider::BlockReader;

/// Block reader bound for the concrete Base wire types.
pub trait BlockReaderFor: BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope> {}

impl<T> BlockReaderFor for T where T: BlockReader<Block = BaseBlock, Transaction = BaseTxEnvelope> {}
