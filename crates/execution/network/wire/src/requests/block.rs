use crate::{BodiesClient, HeadersClient};
use reth_primitives_traits::Block;

/// Helper trait that unifies network behaviour needed for fetching entire blocks.
pub trait BlockClient:
    HeadersClient + BodiesClient<Body = base_common_types_chain::BaseBlockBody> + Unpin + Clone
{
    /// The Block type that this client fetches.
    type Block: Block;
}
