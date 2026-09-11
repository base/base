//! Utility functions for node startup and shutdown, for example path parsing and retrieving single
//! blocks from the network.

use std::path::PathBuf;

use alloy_eips::BlockHashOrNumber;
use base_common_types_chain::{BlockHeader, SealedHeader};
use base_execution_network_wire::{HeadersClient, Priority};
use eyre::Result;

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
