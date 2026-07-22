//! Base `eth_subscribe` stream customization.

use base_common_consensus::{BasePrimitives, BaseTransaction};
use base_common_network::Base;
use base_common_rpc_types::BaseHeaderResponse;
use base_protocol::BaseTimeUpdateTx;
use futures::StreamExt;
use reth_chain_state::CanonStateSubscriptions;
use reth_rpc_eth_api::{RpcConvert, RpcHeader, RpcNodeCore, helpers::EthSubscriptions};
use tracing::error;

use super::BaseEthApi;
use crate::BaseEthApiError;

fn apply_timestamp_ms<T: BaseTransaction>(
    header: &mut BaseHeaderResponse,
    transactions: &[T],
    block_number: u64,
    block_timestamp: u64,
) {
    header.timestamp_ms =
        BaseTimeUpdateTx::extract_timestamp_ms(transactions, block_number, block_timestamp).ok();
}

impl<N, Rpc> EthSubscriptions for BaseEthApi<N, Rpc>
where
    N: RpcNodeCore<Primitives = BasePrimitives>,
    Rpc: RpcConvert<Primitives = BasePrimitives, Network = Base, Error = BaseEthApiError>,
{
    fn header_stream(&self) -> impl futures::Stream<Item = RpcHeader<Base>> + Send + Unpin {
        let converter = self.eth_api().converter();
        self.provider().canonical_state_stream().flat_map(move |new_chain| {
            let headers = new_chain
                .committed()
                .blocks_iter()
                .filter_map(|block| {
                    match converter.convert_header(block.clone_sealed_header(), block.rlp_length())
                    {
                        Ok(mut header) => {
                            apply_timestamp_ms(
                                &mut header,
                                &block.body().transactions,
                                block.number,
                                block.timestamp,
                            );
                            Some(header)
                        }
                        Err(err) => {
                            error!(error = %err, "failed to convert canonical header");
                            None
                        }
                    }
                })
                .collect::<Vec<_>>();
            futures::stream::iter(headers)
        })
    }
}

#[cfg(test)]
mod tests {
    use base_common_consensus::{BaseTransactionSigned, TxDeposit};
    use base_common_rpc_types::BaseHeaderResponse;
    use base_protocol::BaseTimeUpdateTx;

    use super::apply_timestamp_ms;

    #[test]
    fn adds_timestamp_ms_from_base_time_metadata() {
        let transactions: Vec<BaseTransactionSigned> = vec![
            TxDeposit::default().into(),
            BaseTimeUpdateTx::new(600).unwrap().into_deposit_tx(9).into(),
        ];
        let mut header = BaseHeaderResponse::default();

        apply_timestamp_ms(&mut header, &transactions, 9, 42);

        assert_eq!(header.timestamp_ms, Some(42_600));
    }

    #[test]
    fn omits_timestamp_ms_without_valid_base_time_metadata() {
        let transactions: Vec<BaseTransactionSigned> = vec![TxDeposit::default().into()];
        let mut header = BaseHeaderResponse::default();

        apply_timestamp_ms(&mut header, &transactions, 9, 42);

        assert_eq!(header.timestamp_ms, None);
    }
}
