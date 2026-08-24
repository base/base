use base_shadow_indexer_db::{ShadowBlockRow, ShadowCanonicalRef};

/// A unit of work sent from the `ExEx` to the writer.
#[derive(Clone, Debug)]
pub enum ShadowWrite {
    /// A reorged-out or reverted block to persist. Boxed: a row carries a full block and receipts.
    Reorged(Box<ShadowBlockRow>),
    /// A canonical block that resolves rows stored at its height.
    Canonical(ShadowCanonicalRef),
}
