//! Test helper impls for generating bodies

#![allow(dead_code)]

use alloy_consensus::BlockHeader;
use alloy_primitives::map::B256Map;
use base_common_consensus::BaseBlockBody as BlockBody;
use reth_network_p2p::bodies::response::BlockResponse;
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_provider::{
    ProviderFactory, StaticFileProviderFactory, StaticFileSegment, StaticFileWriter,
    test_utils::MockNodeDatabase,
};

pub(crate) fn zip_blocks<'a>(
    headers: impl Iterator<Item = &'a SealedHeader>,
    bodies: &mut B256Map<base_common_consensus::BaseBlockBody>,
) -> Vec<BlockResponse> {
    headers
        .into_iter()
        .map(|header| {
            let body = bodies.remove(&header.hash()).expect("body exists");
            if header.is_empty() {
                BlockResponse::Empty(header.clone())
            } else {
                BlockResponse::Full(SealedBlock::from_sealed_parts(header.clone(), body))
            }
        })
        .collect()
}

pub(crate) fn create_raw_bodies(
    headers: impl IntoIterator<Item = SealedHeader>,
    bodies: &mut B256Map<BlockBody>,
) -> Vec<base_common_consensus::BaseBlock> {
    headers
        .into_iter()
        .map(|header| {
            let body = bodies.remove(&header.hash()).expect("body exists");
            body.into_block(header.unseal())
        })
        .collect()
}

#[inline]
pub(crate) fn insert_headers(
    factory: &ProviderFactory<MockNodeDatabase>,
    headers: &[SealedHeader],
) {
    let provider_rw = factory.provider_rw().expect("failed to create provider");
    let static_file_provider = provider_rw.static_file_provider();
    let mut writer = static_file_provider
        .latest_writer(StaticFileSegment::Headers)
        .expect("failed to create writer");

    for header in headers {
        writer.append_header(header.header(), &header.hash()).expect("failed to append header");
    }
    drop(writer);
    provider_rw.commit().expect("failed to commit");
}
