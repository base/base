//! Base `eth_subscribe` stream customization.

use base_common_consensus::BasePrimitives;
use base_common_rpc_types::{BaseHeaderResponse, BaseRpcTypes};
use base_protocol::BaseTimeUpdateTx;
use futures::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_rpc_eth_api::{RpcConvert, RpcNodeCore, helpers::EthSubscriptions};
use tracing::error;

use super::BaseEthApi;
use crate::BaseEthApiError;

impl<N, Rpc> EthSubscriptions for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore<Primitives = BasePrimitives>,
    Rpc: RpcConvert<Primitives = BasePrimitives, Network = BaseRpcTypes, Error = BaseEthApiError>,
{
    fn header_stream(&self) -> impl futures::Stream<Item = BaseHeaderResponse> + Send + Unpin {
        let converter = self.eth_api().converter();
        self.provider().canonical_state_stream().flat_map(move |new_chain| {
            let headers = new_chain
                .committed()
                .blocks_iter()
                .filter_map(|block| {
                    let mut header = converter
                        .convert_header(block.clone_sealed_header(), block.rlp_length())
                        .inspect_err(|err| {
                            error!(error = %err, "failed to convert canonical header");
                        })
                        .ok()?;
                    header.timestamp_ms = BaseTimeUpdateTx::extract_timestamp_ms(
                        &block.body().transactions,
                        block.number,
                        block.timestamp,
                    )
                    .ok();
                    Some(header)
                })
                .collect::<Vec<_>>();
            futures::stream::iter(headers)
        })
    }
}
