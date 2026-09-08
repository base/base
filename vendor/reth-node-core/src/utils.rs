//! Utility functions for node startup and shutdown, for example path parsing and retrieving single
//! blocks from the network.

use std::path::PathBuf;

use alloy_eips::BlockHashOrNumber;
use base_common_consensus::BlockHeader;
use base_execution_consensus::BaseBeaconConsensus;
use eyre::Result;
use reth_network_p2p::{
    bodies::client::BodiesClient, headers::client::HeadersClient, priority::Priority,
};
use reth_primitives_traits::{SealedBlock, SealedHeader};

/// Parses a user-specified path into a [`PathBuf`].
pub fn parse_path(value: &str) -> PathBuf {
    PathBuf::from(value)
}

/// Get a single header from the network
pub async fn get_single_header<Client>(
    client: Client,
    id: BlockHashOrNumber,
) -> Result<SealedHeader>
where
    Client: HeadersClient,
{
    let (peer_id, response) = client.get_header_with_priority(id, Priority::High).await?.split();

    let Some(header) = response else {
        client.report_bad_message(peer_id);
        eyre::bail!("Invalid number of headers received. Expected: 1. Received: 0");
    };

    let header = SealedHeader::seal_slow(header);

    let valid = match id {
        BlockHashOrNumber::Hash(hash) => header.hash() == hash,
        BlockHashOrNumber::Number(number) => header.number() == number,
    };

    if !valid {
        client.report_bad_message(peer_id);
        eyre::bail!(
            "Received invalid header. Received: {:?}. Expected: {:?}",
            header.num_hash(),
            id
        );
    }

    Ok(header)
}

/// Get a body from the network based on header
pub async fn get_single_body<Client>(
    client: Client,
    header: SealedHeader,
    consensus: BaseBeaconConsensus,
) -> Result<SealedBlock>
where
    Client: BodiesClient<Body = base_common_consensus::BaseBlockBody>,
{
    let (peer_id, response) = client.get_block_body(header.hash()).await?.split();

    let Some(body) = response else {
        client.report_bad_message(peer_id);
        eyre::bail!("Invalid number of bodies received. Expected: 1. Received: 0");
    };

    let block = SealedBlock::from_sealed_parts(header, body);
    consensus.validate_block_pre_execution(&block)?;

    Ok(block)
}
