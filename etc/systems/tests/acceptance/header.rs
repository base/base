//! Authentication of observed execution headers, including the Amsterdam boundary.

use alloy_consensus::Header;
use alloy_primitives::B256;
use eyre::{Result, ensure};
use serde_json::Value;

/// A JSON-RPC header whose reported hash has been recomputed from its consensus fields.
#[derive(Clone, Debug)]
pub struct AuthenticatedHeader {
    /// Original JSON, retained so omitted fields remain observable.
    pub raw: Value,
    /// Authenticated block hash.
    pub hash: B256,
    /// Consensus header used in the hash calculation.
    pub header: Header,
}

impl AuthenticatedHeader {
    /// Decodes all known consensus fields and rejects a forged or truncated header.
    pub fn parse(raw: Value) -> Result<Self> {
        let hash: B256 = serde_json::from_value(raw["hash"].clone())?;
        let header: Header = serde_json::from_value(raw.clone())?;
        ensure!(header.hash_slow() == hash, "header hash mismatch at block {}", header.number);
        Ok(Self { raw, hash, header })
    }

    /// Checks absence both on the wire and in the consensus header.
    pub fn without_amsterdam(&self) -> Result<()> {
        ensure!(
            self.raw["blockAccessListHash"].is_null()
                && self.raw["slotNumber"].is_null()
                && self.header.block_access_list_hash.is_none()
                && self.header.slot_number.is_none(),
            "unexpected Amsterdam fields at block {} ({})",
            self.header.number,
            self.hash
        );
        Ok(())
    }

    /// Authenticates adjacent blocks straddling the configured EL/CL fork instant.
    pub fn check_boundary(
        before: &Self,
        after: &Self,
        activation: u64,
        genesis: u64,
        seconds_per_slot: u64,
    ) -> Result<()> {
        ensure!(seconds_per_slot > 0, "slot duration must be nonzero");
        before.without_amsterdam()?;
        ensure!(before.header.timestamp < activation, "boundary predecessor is not pre-fork");
        ensure!(after.header.timestamp >= activation, "boundary successor is not post-fork");
        ensure!(
            before.header.number.checked_add(1) == Some(after.header.number)
                && after.header.parent_hash == before.hash,
            "fork boundary headers are not adjacent and linked"
        );
        ensure!(
            after.header.block_access_list_hash.is_some(),
            "post-fork blockAccessListHash missing"
        );
        let elapsed = after
            .header
            .timestamp
            .checked_sub(genesis)
            .ok_or_else(|| eyre::eyre!("post-fork header predates consensus genesis"))?;
        ensure!(elapsed % seconds_per_slot == 0, "post-fork timestamp is not slot-aligned");
        ensure!(
            after.header.slot_number == Some(elapsed / seconds_per_slot),
            "post-fork slotNumber does not match CL genesis and slot duration"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use serde_json::json;

    use super::AuthenticatedHeader;

    fn rpc_header(header: Header) -> serde_json::Value {
        let mut value = serde_json::to_value(&header).unwrap();
        value["hash"] = json!(header.hash_slow());
        value
    }

    fn boundary() -> (AuthenticatedHeader, AuthenticatedHeader) {
        let before = AuthenticatedHeader::parse(rpc_header(Header {
            number: 31,
            timestamp: 162,
            ..Default::default()
        }))
        .unwrap();
        let after = AuthenticatedHeader::parse(rpc_header(Header {
            number: 32,
            timestamp: 164,
            parent_hash: before.hash,
            block_access_list_hash: Some(B256::repeat_byte(1)),
            slot_number: Some(32),
            ..Default::default()
        }))
        .unwrap();
        (before, after)
    }

    #[test]
    fn authenticates_a_scheduled_boundary() {
        let (before, after) = boundary();
        AuthenticatedHeader::check_boundary(&before, &after, 164, 100, 2).unwrap();
    }

    #[test]
    fn rejects_forged_header_hash() {
        let mut header = rpc_header(Header::default());
        header["hash"] = json!(B256::repeat_byte(9));
        assert!(AuthenticatedHeader::parse(header).is_err());
    }

    #[test]
    fn rejects_dropped_amsterdam_field_even_if_reported_hash_is_unchanged() {
        let (_, after) = boundary();
        let mut raw = after.raw;
        raw.as_object_mut().unwrap().remove("slotNumber");
        assert!(AuthenticatedHeader::parse(raw).is_err());
    }

    #[test]
    fn rejects_unlinked_boundary() {
        let (before, after) = boundary();
        let unrelated = AuthenticatedHeader::parse(rpc_header(Header {
            parent_hash: B256::repeat_byte(7),
            ..after.header
        }))
        .unwrap();
        assert!(AuthenticatedHeader::check_boundary(&before, &unrelated, 164, 100, 2).is_err());
    }

    #[test]
    fn rejects_wrong_slot_and_postfork_only_observations() {
        let (before, after) = boundary();
        assert!(AuthenticatedHeader::check_boundary(&before, &after, 162, 100, 2).is_err());
        assert!(AuthenticatedHeader::check_boundary(&before, &after, 164, 102, 2).is_err());
        assert!(after.without_amsterdam().is_err());
    }
}
