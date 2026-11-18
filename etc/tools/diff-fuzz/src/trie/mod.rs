//! Differential trie fuzzing (`TRIE_PROTOCOL.md`): block sequences of hashed-key writes and
//! proof reads applied by every client's state root code over its own database, compared
//! block by block, guided by edge coverage of the trie code.

mod protocol;
pub use protocol::{
    ProofAccount, ProofResult, ProofTarget, StorageProofResult, TrieBlock, TrieBlockResult,
    TrieInput, TrieOp, TrieRequest, TrieResponse,
};

mod model;
pub use model::{BuiltTrie, ModelAccount, TrieModel};

mod case;
pub use case::{BuiltCase, CaseBlock, Preimages, TrieCase};

mod r#gen;
pub use r#gen::TrieGen;

mod mutate;
pub use mutate::TrieMutator;

mod compare;
pub use compare::{TrieCompare, TrieDifference, TrieVerdict};

mod campaign;
pub use campaign::{
    TrieFinding, TrieFuzzConfig, TrieFuzzStats, TrieFuzzer, TrieResults, TrieSample,
};
