//! Base Payload attributes that reference the parent L2 block.

use base_common_consensus::OpTxType;
use base_common_rpc_types_engine::BasePayloadAttributes;

use crate::{BlockInfo, L2BlockInfo};

/// Base Payload Attributes with parent block reference and the L1 origin block.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AttributesWithParent {
    /// The payload attributes.
    pub attributes: BasePayloadAttributes,
    /// The parent block reference.
    pub parent: L2BlockInfo,
    /// The L1 block that the attributes were derived from.
    pub derived_from: Option<BlockInfo>,
    /// Whether the current batch is the last in its span.
    pub is_last_in_span: bool,
}

impl AttributesWithParent {
    /// Create a new [`AttributesWithParent`] instance.
    pub const fn new(
        attributes: BasePayloadAttributes,
        parent: L2BlockInfo,
        derived_from: Option<BlockInfo>,
        is_last_in_span: bool,
    ) -> Self {
        Self { attributes, parent, derived_from, is_last_in_span }
    }

    /// Returns the L2 block number for the payload attributes if made canonical.
    /// Derived as the parent block height plus one.
    pub const fn block_number(&self) -> u64 {
        self.parent.block_info.number.saturating_add(1)
    }

    /// Consumes `self` and returns the inner [`BasePayloadAttributes`].
    pub fn take_inner(self) -> BasePayloadAttributes {
        self.attributes
    }

    /// Returns the payload attributes.
    pub const fn attributes(&self) -> &BasePayloadAttributes {
        &self.attributes
    }

    /// Returns the parent block reference.
    pub const fn parent(&self) -> &L2BlockInfo {
        &self.parent
    }

    /// Returns the L1 origin block reference.
    pub const fn derived_from(&self) -> Option<&BlockInfo> {
        self.derived_from.as_ref()
    }

    /// Returns whether the current batch is the last in its span.
    pub const fn is_last_in_span(&self) -> bool {
        self.is_last_in_span
    }

    /// Returns `true` if all transactions in the payload are deposits.
    pub fn is_deposits_only(&self) -> bool {
        self.attributes
            .transactions
            .iter()
            .all(|tx| tx.first().is_some_and(|tx| tx[0] == OpTxType::Deposit as u8))
    }

    /// Converts the [`AttributesWithParent`] into a deposits-only payload.
    pub fn as_deposits_only(&self) -> Self {
        let mut attributes = self.attributes.clone();

        attributes
            .transactions
            .iter_mut()
            .for_each(|txs| txs.retain(|tx| tx.first().copied() == Some(OpTxType::Deposit as u8)));

        Self {
            attributes,
            parent: self.parent,
            derived_from: self.derived_from,
            is_last_in_span: self.is_last_in_span,
        }
    }

    /// Returns the number of transactions in the attributes.
    pub fn count_transactions(&self) -> u64 {
        self.attributes().decoded_transactions().count().try_into().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn test_op_attributes_with_parent() {
        let attributes = BasePayloadAttributes::default();
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes.clone(), parent, None, is_last_in_span);

        assert_eq!(op_attributes_with_parent.attributes(), &attributes);
        assert_eq!(op_attributes_with_parent.parent(), &parent);
        assert_eq!(op_attributes_with_parent.is_last_in_span(), is_last_in_span);
        assert_eq!(op_attributes_with_parent.derived_from(), None);
    }

    /// Test that the [`AttributesWithParent::as_deposits_only`] method strips out all
    /// transactions that are not deposits.
    #[test]
    fn test_op_attributes_with_parent_as_deposits_only() {
        let attributes = BasePayloadAttributes {
            transactions: Some(vec![
                vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into(),
                vec![OpTxType::Legacy as u8, 0x0, 0x11, 0x21].into(),
                vec![OpTxType::Eip2930 as u8, 0x0, 0x12, 0x22].into(),
                vec![OpTxType::Eip1559 as u8, 0x0, 0x13, 0x23].into(),
                vec![OpTxType::Eip7702 as u8, 0x0, 0x14, 0x24].into(),
                vec![].into(),
            ]),
            ..BasePayloadAttributes::default()
        };
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes, parent, None, is_last_in_span);
        let deposits_only_attributes = op_attributes_with_parent.as_deposits_only();

        assert_eq!(
            deposits_only_attributes.attributes().transactions,
            Some(vec![vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into()])
        );
    }

    #[test]
    fn test_op_attributes_with_parent_as_deposits_multi_deposits() {
        let attributes = BasePayloadAttributes {
            transactions: Some(vec![
                vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into(),
                vec![OpTxType::Legacy as u8, 0x0, 0x11, 0x21].into(),
                vec![OpTxType::Eip2930 as u8, 0x0, 0x12, 0x22].into(),
                vec![OpTxType::Deposit as u8, 0x98, 0x21, 0x31].into(),
                vec![OpTxType::Eip1559 as u8, 0x0, 0x13, 0x23].into(),
                vec![OpTxType::Eip7702 as u8, 0x0, 0x14, 0x24].into(),
                vec![OpTxType::Deposit as u8, 0x56, 0x31, 0x41].into(),
                vec![].into(),
            ]),
            ..BasePayloadAttributes::default()
        };
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes, parent, None, is_last_in_span);
        let deposits_only_attributes = op_attributes_with_parent.as_deposits_only();

        assert_eq!(
            deposits_only_attributes.attributes().transactions,
            Some(vec![
                vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into(),
                vec![OpTxType::Deposit as u8, 0x98, 0x21, 0x31].into(),
                vec![OpTxType::Deposit as u8, 0x56, 0x31, 0x41].into(),
            ])
        );
    }

    /// Test that the [`AttributesWithParent::as_deposits_only`] method strips out all
    /// transactions that are not deposits.
    #[test]
    fn test_op_attributes_with_parent_as_deposits_no_deposits() {
        let attributes = BasePayloadAttributes {
            transactions: Some(vec![
                vec![OpTxType::Legacy as u8, 0x0, 0x11, 0x21].into(),
                vec![OpTxType::Eip2930 as u8, 0x0, 0x12, 0x22].into(),
                vec![OpTxType::Eip1559 as u8, 0x0, 0x13, 0x23].into(),
                vec![OpTxType::Eip7702 as u8, 0x0, 0x14, 0x24].into(),
                vec![].into(),
            ]),
            ..BasePayloadAttributes::default()
        };
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes, parent, None, is_last_in_span);
        let deposits_only_attributes = op_attributes_with_parent.as_deposits_only();

        assert_eq!(deposits_only_attributes.attributes().transactions, Some(vec![]));
    }

    #[test]
    fn test_op_attributes_with_parent_as_deposits_only_deposits() {
        let attributes = BasePayloadAttributes {
            transactions: Some(vec![
                vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into(),
                vec![OpTxType::Deposit as u8, 0x98, 0x21, 0x31].into(),
                vec![OpTxType::Deposit as u8, 0x56, 0x31, 0x41].into(),
                vec![].into(),
            ]),
            ..BasePayloadAttributes::default()
        };
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes, parent, None, is_last_in_span);
        let deposits_only_attributes = op_attributes_with_parent.as_deposits_only();

        assert_eq!(
            deposits_only_attributes.attributes().transactions,
            Some(vec![
                vec![OpTxType::Deposit as u8, 0x0, 0x10, 0x20].into(),
                vec![OpTxType::Deposit as u8, 0x98, 0x21, 0x31].into(),
                vec![OpTxType::Deposit as u8, 0x56, 0x31, 0x41].into(),
            ])
        );
    }

    #[test]
    fn test_op_attributes_with_parent_as_deposits_no_txs() {
        let attributes =
            BasePayloadAttributes { transactions: None, ..BasePayloadAttributes::default() };
        let parent = L2BlockInfo::default();
        let is_last_in_span = true;
        let op_attributes_with_parent =
            AttributesWithParent::new(attributes, parent, None, is_last_in_span);
        let deposits_only_attributes = op_attributes_with_parent.as_deposits_only();

        assert_eq!(deposits_only_attributes.attributes().transactions, None);
    }
}
