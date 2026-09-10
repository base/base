use crate::{BodiesClient, HeadersClient};

/// Helper trait that unifies network behaviour needed for fetching entire blocks.
pub trait BlockClient: HeadersClient + BodiesClient + Unpin + Clone {}
