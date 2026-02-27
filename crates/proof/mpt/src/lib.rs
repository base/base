#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod errors;
pub use errors::{OrderedListWalkerError, OrderedListWalkerResult, TrieNodeError, TrieNodeResult};

mod traits;
pub use traits::{TrieHinter, TrieProvider};

mod node;
pub use node::TrieNode;

mod list_walker;
pub use list_walker::OrderedListWalker;

mod noop;
pub use noop::{NoopTrieHinter, NoopTrieProvider};

mod util;
// Re-export [alloy_trie::Nibbles].
pub use alloy_trie::Nibbles;
pub use util::ordered_trie_with_encoder;

#[cfg(test)]
mod test_util;
