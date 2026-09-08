use alloc::{boxed::Box, fmt, vec::Vec};

use alloy_primitives::B256;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_trie_common::updates::TrieUpdates;

/// An invalid block hook.
pub trait InvalidBlockHook: Send + Sync {
    /// Invoked when an invalid block is encountered.
    fn on_invalid_block(
        &self,
        parent_header: &SealedHeader,
        block: &RecoveredBlock,
        output: &BlockExecutionOutput,
        trie_updates: Option<(&TrieUpdates, B256)>,
    );
}

impl<F> InvalidBlockHook for F
where
    F: Fn(&SealedHeader, &RecoveredBlock, &BlockExecutionOutput, Option<(&TrieUpdates, B256)>)
        + Send
        + Sync,
{
    fn on_invalid_block(
        &self,
        parent_header: &SealedHeader,
        block: &RecoveredBlock,
        output: &BlockExecutionOutput,
        trie_updates: Option<(&TrieUpdates, B256)>,
    ) {
        self(parent_header, block, output, trie_updates)
    }
}

/// A no-op [`InvalidBlockHook`] that does nothing.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct NoopInvalidBlockHook;

impl InvalidBlockHook for NoopInvalidBlockHook {
    fn on_invalid_block(
        &self,
        _parent_header: &SealedHeader,
        _block: &RecoveredBlock,
        _output: &BlockExecutionOutput,
        _trie_updates: Option<(&TrieUpdates, B256)>,
    ) {
    }
}

/// Multiple [`InvalidBlockHook`]s that are executed in order.
pub struct InvalidBlockHooks(pub Vec<Box<dyn InvalidBlockHook>>);

impl fmt::Debug for InvalidBlockHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvalidBlockHooks").field("len", &self.0.len()).finish()
    }
}

impl InvalidBlockHook for InvalidBlockHooks {
    fn on_invalid_block(
        &self,
        parent_header: &SealedHeader,
        block: &RecoveredBlock,
        output: &BlockExecutionOutput,
        trie_updates: Option<(&TrieUpdates, B256)>,
    ) {
        for hook in &self.0 {
            hook.on_invalid_block(parent_header, block, output, trie_updates);
        }
    }
}
