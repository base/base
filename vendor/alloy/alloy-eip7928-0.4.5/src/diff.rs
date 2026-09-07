//! Helpers for finding the first divergence between block access lists.

use crate::AccountChanges;
use alloy_primitives::Address;
use core::{cmp::Ordering, fmt};

/// Compact summary of the first difference between two block access lists.
///
/// The diff only reports the first account position that differs. This keeps diagnostics cheap for
/// callers that only need enough context to explain why two BALs do not hash to the same value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalDiff {
    /// Number of account entries in the left-hand BAL.
    pub left_accounts: usize,
    /// Number of account entries in the right-hand BAL.
    pub right_accounts: usize,
    /// First account-level difference, if any.
    pub first_diff: Option<AccountDiff>,
}

impl BalDiff {
    /// Returns the first divergence between two block access lists.
    pub fn between(left: &[AccountChanges], right: &[AccountChanges]) -> Self {
        let mut index = 0;
        let first_diff = loop {
            match (left.get(index), right.get(index)) {
                (Some(left_account), Some(right_account)) => {
                    match left_account.address.cmp(&right_account.address) {
                        Ordering::Less | Ordering::Greater => {
                            break Some(AccountDiff::address_mismatch(
                                index,
                                left_account,
                                right_account,
                            ));
                        }
                        Ordering::Equal => {
                            let fields_differ = AccountFieldDiff::new(left_account, right_account);
                            if fields_differ.is_divergent() {
                                break Some(AccountDiff {
                                    index,
                                    left: Some(AccountSummary::from_account(left_account)),
                                    right: Some(AccountSummary::from_account(right_account)),
                                    fields_differ,
                                });
                            }
                            index += 1;
                        }
                    }
                }
                (Some(account), None) => break Some(AccountDiff::left_only(index, account)),
                (None, Some(account)) => break Some(AccountDiff::right_only(index, account)),
                (None, None) => break None,
            }
        };

        Self { left_accounts: left.len(), right_accounts: right.len(), first_diff }
    }

    /// Returns `true` if the compared BALs do not diverge.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.first_diff.is_none()
    }
}

impl fmt::Display for BalDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.first_diff {
            Some(diff) => write!(
                f,
                "accounts left={}, right={}; {}",
                self.left_accounts, self.right_accounts, diff
            ),
            None => write!(
                f,
                "no BAL divergence (accounts left={}, right={})",
                self.left_accounts, self.right_accounts
            ),
        }
    }
}

/// Account-level details for the first differing BAL entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDiff {
    /// Account index in the BAL where the first difference was found.
    pub index: usize,
    /// Summary of the left-hand account entry at [`Self::index`], if present.
    pub left: Option<AccountSummary>,
    /// Summary of the right-hand account entry at [`Self::index`], if present.
    pub right: Option<AccountSummary>,
    /// Per-field differences when both entries have the same address.
    pub fields_differ: AccountFieldDiff,
}

impl AccountDiff {
    fn left_only(index: usize, account: &AccountChanges) -> Self {
        Self {
            index,
            left: Some(AccountSummary::from_account(account)),
            right: None,
            fields_differ: AccountFieldDiff::default(),
        }
    }

    fn right_only(index: usize, account: &AccountChanges) -> Self {
        Self {
            index,
            left: None,
            right: Some(AccountSummary::from_account(account)),
            fields_differ: AccountFieldDiff::default(),
        }
    }

    fn address_mismatch(index: usize, left: &AccountChanges, right: &AccountChanges) -> Self {
        Self {
            index,
            left: Some(AccountSummary::from_account(left)),
            right: Some(AccountSummary::from_account(right)),
            fields_differ: AccountFieldDiff::default(),
        }
    }
}

impl fmt::Display for AccountDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "first difference at account index {}", self.index)?;
        match (&self.left, &self.right) {
            (Some(left), Some(right)) if left.address != right.address => {
                write!(f, ": address mismatch, left={left}, right={right}")
            }
            (Some(left), Some(right)) => {
                write!(f, ": fields [{}] differ, left={left}, right={right}", self.fields_differ)
            }
            (Some(left), None) => write!(f, ": account only in left BAL, left={left}"),
            (None, Some(right)) => write!(f, ": account only in right BAL, right={right}"),
            (None, None) => f.write_str(": missing account summaries"),
        }
    }
}

/// Compact account summary included in [`AccountDiff`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountSummary {
    /// Account address for this BAL entry.
    pub address: Address,
    /// Number of changed storage slots.
    pub storage_changes: usize,
    /// Number of read storage slots.
    pub storage_reads: usize,
    /// Number of balance changes.
    pub balance_changes: usize,
    /// Number of nonce changes.
    pub nonce_changes: usize,
    /// Number of code changes.
    pub code_changes: usize,
}

impl AccountSummary {
    /// Creates a summary from a BAL account entry.
    #[inline]
    pub const fn from_account(account: &AccountChanges) -> Self {
        Self {
            address: account.address,
            storage_changes: account.storage_changes.len(),
            storage_reads: account.storage_reads.len(),
            balance_changes: account.balance_changes.len(),
            nonce_changes: account.nonce_changes.len(),
            code_changes: account.code_changes.len(),
        }
    }
}

impl fmt::Display for AccountSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (storage_changes={}, storage_reads={}, balance_changes={}, nonce_changes={}, code_changes={})",
            self.address,
            self.storage_changes,
            self.storage_reads,
            self.balance_changes,
            self.nonce_changes,
            self.code_changes
        )
    }
}

/// Per-field difference flags for account entries with the same address.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountFieldDiff {
    /// `true` if `storage_changes` differ.
    pub storage_changes: bool,
    /// `true` if `storage_reads` differ.
    pub storage_reads: bool,
    /// `true` if `balance_changes` differ.
    pub balance_changes: bool,
    /// `true` if `nonce_changes` differ.
    pub nonce_changes: bool,
    /// `true` if `code_changes` differ.
    pub code_changes: bool,
}

impl AccountFieldDiff {
    /// Creates field-level diff flags for account entries with the same address.
    pub fn new(left: &AccountChanges, right: &AccountChanges) -> Self {
        Self {
            storage_changes: left.storage_changes != right.storage_changes,
            storage_reads: left.storage_reads != right.storage_reads,
            balance_changes: left.balance_changes != right.balance_changes,
            nonce_changes: left.nonce_changes != right.nonce_changes,
            code_changes: left.code_changes != right.code_changes,
        }
    }

    /// Returns `true` if any account field differs.
    #[inline]
    pub const fn is_divergent(&self) -> bool {
        self.storage_changes
            || self.storage_reads
            || self.balance_changes
            || self.nonce_changes
            || self.code_changes
    }

    fn fmt_flag(
        f: &mut fmt::Formatter<'_>,
        wrote_field: &mut bool,
        differs: bool,
        name: &str,
    ) -> fmt::Result {
        if differs {
            if *wrote_field {
                f.write_str(", ")?;
            }
            f.write_str(name)?;
            *wrote_field = true;
        }
        Ok(())
    }
}

impl fmt::Display for AccountFieldDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut wrote_field = false;
        Self::fmt_flag(f, &mut wrote_field, self.storage_changes, "storage_changes")?;
        Self::fmt_flag(f, &mut wrote_field, self.storage_reads, "storage_reads")?;
        Self::fmt_flag(f, &mut wrote_field, self.balance_changes, "balance_changes")?;
        Self::fmt_flag(f, &mut wrote_field, self.nonce_changes, "nonce_changes")?;
        Self::fmt_flag(f, &mut wrote_field, self.code_changes, "code_changes")?;
        if !wrote_field {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// Returns a compact diff describing the first divergence between two BALs.
pub fn first_bal_divergence(left: &[AccountChanges], right: &[AccountChanges]) -> BalDiff {
    BalDiff::between(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges, StorageChange,
        bal::Bal,
    };
    use alloc::format;
    use alloy_primitives::{Address, Bytes, U256};

    fn diagnostic_addr(byte: u8) -> Address {
        let mut address = [0; 20];
        address[19] = byte;
        Address::from(address)
    }

    fn diagnostic_account(address: Address, balance: u64) -> AccountChanges {
        AccountChanges {
            address,
            balance_changes: vec![BalanceChange::new(
                BlockAccessIndex::new(1),
                U256::from(balance),
            )],
            ..Default::default()
        }
    }

    fn first_diff(left: &[AccountChanges], right: &[AccountChanges]) -> AccountDiff {
        first_bal_divergence(left, right).first_diff.expect("expected BAL divergence")
    }

    fn assert_summary_address(summary: &Option<AccountSummary>, address: Address) {
        assert_eq!(summary.as_ref().map(|summary| summary.address), Some(address));
    }

    #[test]
    fn none_for_equal_bals() {
        let account = diagnostic_account(diagnostic_addr(1), 1);
        let right = vec![account.clone()];

        let diff = first_bal_divergence(core::slice::from_ref(&account), &right);

        assert_eq!(diff, BalDiff { left_accounts: 1, right_accounts: 1, first_diff: None });
        assert!(diff.is_empty());
    }

    #[test]
    fn reports_account_only_in_left() {
        let left = vec![diagnostic_account(diagnostic_addr(1), 1)];

        let diff = first_diff(&left, &[]);

        assert_eq!(diff.index, 0);
        assert_eq!(diff.left, Some(AccountSummary::from_account(&left[0])));
        assert_eq!(diff.right, None);
    }

    #[test]
    fn reports_tail_account_only_in_left_after_matching_prefix() {
        let shared = diagnostic_account(diagnostic_addr(1), 1);
        let left = vec![shared.clone(), diagnostic_account(diagnostic_addr(2), 2)];
        let right = vec![shared];

        let diff = first_diff(&left, &right);

        assert_eq!(diff.index, 1);
        assert_summary_address(&diff.left, diagnostic_addr(2));
        assert_eq!(diff.right, None);
    }

    #[test]
    fn reports_account_only_in_right() {
        let right = vec![diagnostic_account(diagnostic_addr(1), 1)];

        let diff = first_diff(&[], &right);

        assert_eq!(diff.index, 0);
        assert_eq!(diff.left, None);
        assert_eq!(diff.right, Some(AccountSummary::from_account(&right[0])));
    }

    #[test]
    fn reports_tail_account_only_in_right_after_matching_prefix() {
        let shared = diagnostic_account(diagnostic_addr(1), 1);
        let left = vec![shared.clone()];
        let right = vec![shared, diagnostic_account(diagnostic_addr(2), 2)];

        let diff = first_diff(&left, &right);

        assert_eq!(diff.index, 1);
        assert_eq!(diff.left, None);
        assert_summary_address(&diff.right, diagnostic_addr(2));
    }

    #[test]
    fn reports_changed_balance_field() {
        let left = vec![diagnostic_account(diagnostic_addr(1), 1)];
        let right = vec![diagnostic_account(diagnostic_addr(1), 2)];

        let diff = first_diff(&left, &right);

        assert_eq!(diff.index, 0);
        assert_eq!(
            diff.fields_differ,
            AccountFieldDiff { balance_changes: true, ..Default::default() }
        );
        assert!(diff.left.is_some());
        assert!(diff.right.is_some());
        assert!(!first_bal_divergence(&left, &right).is_empty());
    }

    #[test]
    fn reports_each_changed_account_field() {
        let left = vec![AccountChanges {
            address: diagnostic_addr(1),
            storage_changes: vec![SlotChanges::new(
                U256::from(1),
                vec![StorageChange::new(BlockAccessIndex::new(1), U256::from(1))],
            )],
            storage_reads: vec![U256::from(2)],
            balance_changes: vec![BalanceChange::new(BlockAccessIndex::new(3), U256::from(3))],
            nonce_changes: vec![NonceChange::new(BlockAccessIndex::new(4), 4)],
            code_changes: vec![CodeChange::new(BlockAccessIndex::new(5), Bytes::from_static(&[5]))],
        }];
        let right = vec![AccountChanges {
            address: diagnostic_addr(1),
            storage_changes: vec![SlotChanges::new(
                U256::from(1),
                vec![StorageChange::new(BlockAccessIndex::new(1), U256::from(10))],
            )],
            storage_reads: vec![U256::from(20)],
            balance_changes: vec![BalanceChange::new(BlockAccessIndex::new(3), U256::from(30))],
            nonce_changes: vec![NonceChange::new(BlockAccessIndex::new(4), 40)],
            code_changes: vec![CodeChange::new(
                BlockAccessIndex::new(5),
                Bytes::from_static(&[50]),
            )],
        }];

        let diff = first_diff(&left, &right);

        assert_eq!(
            diff.fields_differ,
            AccountFieldDiff {
                storage_changes: true,
                storage_reads: true,
                balance_changes: true,
                nonce_changes: true,
                code_changes: true,
            }
        );
        assert_eq!(
            diff.left,
            Some(AccountSummary {
                address: diagnostic_addr(1),
                storage_changes: 1,
                storage_reads: 1,
                balance_changes: 1,
                nonce_changes: 1,
                code_changes: 1,
            })
        );
    }

    #[test]
    fn reports_both_addresses_for_address_mismatch() {
        let left = vec![
            diagnostic_account(diagnostic_addr(1), 1),
            diagnostic_account(diagnostic_addr(3), 1),
        ];
        let right = vec![
            diagnostic_account(diagnostic_addr(2), 1),
            diagnostic_account(diagnostic_addr(3), 2),
        ];

        let diff = first_diff(&left, &right);

        assert_eq!(diff.index, 0);
        assert_summary_address(&diff.left, diagnostic_addr(1));
        assert_summary_address(&diff.right, diagnostic_addr(2));
        assert_eq!(diff.fields_differ, AccountFieldDiff::default());
    }

    #[test]
    fn stops_at_first_mismatch_after_matching_prefix() {
        let shared = diagnostic_account(diagnostic_addr(1), 1);
        let left = vec![
            shared.clone(),
            diagnostic_account(diagnostic_addr(2), 2),
            diagnostic_account(diagnostic_addr(4), 4),
        ];
        let right = vec![
            shared,
            diagnostic_account(diagnostic_addr(3), 3),
            diagnostic_account(diagnostic_addr(4), 5),
        ];

        let diff = first_diff(&left, &right);

        assert_eq!(diff.index, 1);
        assert_summary_address(&diff.left, diagnostic_addr(2));
        assert_summary_address(&diff.right, diagnostic_addr(3));
        assert_eq!(diff.fields_differ, AccountFieldDiff::default());
    }

    #[test]
    fn bal_methods_compare_against_slices() {
        let left = Bal::new(vec![diagnostic_account(diagnostic_addr(1), 1)]);
        let right = vec![diagnostic_account(diagnostic_addr(1), 2)];

        assert_eq!(left.diff(&right), BalDiff::between(left.as_slice(), &right));
    }

    #[test]
    fn displays_equal_bals_without_diff() {
        let account = diagnostic_account(diagnostic_addr(1), 1);
        let right = vec![account.clone()];

        assert_eq!(
            format!("{}", first_bal_divergence(core::slice::from_ref(&account), &right)),
            "no BAL divergence (accounts left=1, right=1)"
        );
    }

    #[test]
    fn displays_field_divergence() {
        let left = vec![diagnostic_account(diagnostic_addr(1), 1)];
        let right = vec![diagnostic_account(diagnostic_addr(1), 2)];

        assert_eq!(
            format!("{}", first_bal_divergence(&left, &right)),
            concat!(
                "accounts left=1, right=1; first difference at account index 0: ",
                "fields [balance_changes] differ, ",
                "left=0x0000000000000000000000000000000000000001 ",
                "(storage_changes=0, storage_reads=0, balance_changes=1, nonce_changes=0, ",
                "code_changes=0), ",
                "right=0x0000000000000000000000000000000000000001 ",
                "(storage_changes=0, storage_reads=0, balance_changes=1, nonce_changes=0, ",
                "code_changes=0)"
            )
        );
    }

    #[test]
    fn displays_address_mismatch() {
        let left = vec![diagnostic_account(diagnostic_addr(1), 1)];
        let right = vec![diagnostic_account(diagnostic_addr(2), 1)];

        assert_eq!(
            format!("{}", first_bal_divergence(&left, &right)),
            concat!(
                "accounts left=1, right=1; first difference at account index 0: ",
                "address mismatch, ",
                "left=0x0000000000000000000000000000000000000001 ",
                "(storage_changes=0, storage_reads=0, balance_changes=1, nonce_changes=0, ",
                "code_changes=0), ",
                "right=0x0000000000000000000000000000000000000002 ",
                "(storage_changes=0, storage_reads=0, balance_changes=1, nonce_changes=0, ",
                "code_changes=0)"
            )
        );
    }

    #[test]
    fn displays_missing_account_side() {
        let left = vec![diagnostic_account(diagnostic_addr(1), 1)];

        assert_eq!(
            format!("{}", first_bal_divergence(&left, &[])),
            concat!(
                "accounts left=1, right=0; first difference at account index 0: ",
                "account only in left BAL, ",
                "left=0x0000000000000000000000000000000000000001 ",
                "(storage_changes=0, storage_reads=0, balance_changes=1, nonce_changes=0, ",
                "code_changes=0)"
            )
        );
    }
}
