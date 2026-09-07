//! Contains the [`BlockAccessList`] type, which represents a simple list of account changes.

use crate::account_changes::AccountChanges;
use alloc::vec::Vec;

#[cfg(not(feature = "std"))]
use once_cell::race::OnceBox as OnceLock;
#[cfg(feature = "std")]
use std::sync::OnceLock;

/// This struct is used to store `account_changes` in a block.
pub type BlockAccessList = Vec<AccountChanges>;

/// Computes the hash of the given block access list.
#[cfg(feature = "rlp")]
pub fn compute_block_access_list_hash(bal: &[AccountChanges]) -> alloy_primitives::B256 {
    let mut buf = Vec::new();
    alloy_rlp::encode_list(bal, &mut buf);
    alloy_primitives::keccak256(&buf)
}

/// Computes the total number of items in the block access list, counting each account and storage
/// entry.
pub fn total_bal_items(bal: &[AccountChanges]) -> u64 {
    bal.iter()
        .map(|account| 1 + account.storage_changes().len() + account.storage_reads().len())
        .sum::<usize>() as u64
}

/// Block-Level Access List wrapper type with helper methods for metrics and validation.
pub mod bal {
    use super::OnceLock;
    use crate::{
        BlockAccessListGasError, BlockAccessListHashMismatch, account_changes::AccountChanges,
        diff::BalDiff,
    };
    use alloc::vec::{IntoIter, Vec};
    use alloy_primitives::{B256, Bytes};
    use core::{
        ops::{Deref, Index},
        slice::Iter,
    };

    /// A wrapper around [`Vec<AccountChanges>`] that provides helper methods for
    /// computing metrics and statistics about the block access list.
    ///
    /// This type implements `Deref` to `[AccountChanges]` for easy access to the
    /// underlying data while providing additional utility methods for BAL analysis.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(
        feature = "rlp",
        derive(alloy_rlp::RlpEncodableWrapper, alloy_rlp::RlpDecodableWrapper)
    )]
    #[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
    #[cfg_attr(feature = "borsh", derive(borsh::BorshSerialize, borsh::BorshDeserialize))]
    pub struct Bal(Vec<AccountChanges>);

    impl From<Bal> for Vec<AccountChanges> {
        #[inline]
        fn from(this: Bal) -> Self {
            this.0
        }
    }

    impl From<Vec<AccountChanges>> for Bal {
        #[inline]
        fn from(list: Vec<AccountChanges>) -> Self {
            Self(list)
        }
    }

    #[cfg(feature = "rlp")]
    impl alloy_primitives::Sealable for Bal {
        fn hash_slow(&self) -> alloy_primitives::B256 {
            self.compute_hash()
        }
    }

    impl Deref for Bal {
        type Target = [AccountChanges];

        #[inline]
        fn deref(&self) -> &Self::Target {
            self.as_slice()
        }
    }

    impl IntoIterator for Bal {
        type Item = AccountChanges;
        type IntoIter = IntoIter<AccountChanges>;

        #[inline]
        fn into_iter(self) -> Self::IntoIter {
            self.0.into_iter()
        }
    }

    impl<'a> IntoIterator for &'a Bal {
        type Item = &'a AccountChanges;
        type IntoIter = Iter<'a, AccountChanges>;

        #[inline]
        fn into_iter(self) -> Self::IntoIter {
            self.iter()
        }
    }

    impl FromIterator<AccountChanges> for Bal {
        fn from_iter<I: IntoIterator<Item = AccountChanges>>(iter: I) -> Self {
            Self(iter.into_iter().collect())
        }
    }

    impl<I> Index<I> for Bal
    where
        I: core::slice::SliceIndex<[AccountChanges]>,
    {
        type Output = I::Output;

        #[inline]
        fn index(&self, index: I) -> &Self::Output {
            &self.0[index]
        }
    }

    impl Bal {
        /// Creates a new [`Bal`] from the provided account changes.
        #[inline]
        pub const fn new(account_changes: Vec<AccountChanges>) -> Self {
            Self(account_changes)
        }

        /// Adds a new [`AccountChanges`] entry to the list.
        #[inline]
        pub fn push(&mut self, account_changes: AccountChanges) {
            self.0.push(account_changes)
        }

        /// Returns `true` if the list contains no elements.
        #[inline]
        pub const fn is_empty(&self) -> bool {
            self.0.is_empty()
        }

        /// Returns the number of account change entries contained in the list.
        #[inline]
        pub const fn len(&self) -> usize {
            self.0.len()
        }

        /// Returns an iterator over the [`AccountChanges`] entries.
        #[inline]
        pub fn iter(&self) -> Iter<'_, AccountChanges> {
            self.0.iter()
        }

        /// Returns a slice of the contained [`AccountChanges`].
        #[inline]
        pub const fn as_slice(&self) -> &[AccountChanges] {
            self.0.as_slice()
        }

        /// Returns the contained [`Vec<AccountChanges>`].
        #[inline]
        pub const fn as_vec(&self) -> &Vec<AccountChanges> {
            &self.0
        }

        /// Returns a compact diff describing where this BAL first diverges from `other`.
        pub fn diff(&self, other: &[AccountChanges]) -> BalDiff {
            BalDiff::between(self.as_slice(), other)
        }

        /// Returns a vector of [`AccountChanges`].
        #[inline]
        pub fn into_inner(self) -> Vec<AccountChanges> {
            self.0
        }

        /// Sorts this block access list in-place according to the canonical EIP-7928 ordering
        /// rules.
        ///
        /// This applies the ordering required by the "Ordering, Uniqueness and Determinism"
        /// section of EIP-7928:
        ///
        /// - accounts are sorted lexicographically by address
        /// - `storage_changes` are sorted lexicographically by storage key
        /// - each per-slot `StorageChange` list is sorted by block access index in ascending order
        /// - `storage_reads` are sorted lexicographically by storage key
        /// - `balance_changes`, `nonce_changes`, and `code_changes` are sorted by block access
        ///   index in ascending order
        ///
        /// The account-local ordering is delegated to [`AccountChanges::sort`], so callers may
        /// sort account internals independently when a parallel sort strategy is useful.
        ///
        /// This method only canonicalizes ordering. It does not enforce the EIP-7928 uniqueness
        /// constraints for accounts, storage keys, or block access indexes.
        pub fn sort(&mut self) {
            self.0.sort_unstable_by_key(|account| account.address);

            for account in &mut self.0 {
                account.sort();
            }
        }

        /// Returns the total number of accounts with changes in this BAL.
        #[inline]
        pub const fn account_count(&self) -> usize {
            self.0.len()
        }

        /// Returns the total number of storage changes across all accounts.
        pub fn total_storage_changes(&self) -> usize {
            self.0.iter().map(|a| a.storage_changes.len()).sum()
        }

        /// Returns the total number of storage reads across all accounts.
        pub fn total_storage_reads(&self) -> usize {
            self.0.iter().map(|a| a.storage_reads.len()).sum()
        }

        /// Returns the total number of storage slots (both changes and reads) across all accounts.
        pub fn total_slots(&self) -> usize {
            self.0.iter().map(|a| a.storage_changes.len() + a.storage_reads.len()).sum()
        }

        /// Returns the total number of balance changes across all accounts.
        pub fn total_balance_changes(&self) -> usize {
            self.0.iter().map(|a| a.balance_changes.len()).sum()
        }

        /// Returns the total number of nonce changes across all accounts.
        pub fn total_nonce_changes(&self) -> usize {
            self.0.iter().map(|a| a.nonce_changes.len()).sum()
        }

        /// Returns the total number of code changes across all accounts.
        pub fn total_code_changes(&self) -> usize {
            self.0.iter().map(|a| a.code_changes.len()).sum()
        }

        /// Returns a summary of all change counts for metrics reporting.
        pub fn change_counts(&self) -> BalChangeCounts {
            let mut counts = BalChangeCounts::default();
            for account in &self.0 {
                counts.accounts += 1;
                counts.storage += account.storage_changes.len();
                counts.balance += account.balance_changes.len();
                counts.nonce += account.nonce_changes.len();
                counts.code += account.code_changes.len();
            }
            counts
        }

        /// Computes the total number of items in this block access list, counting each account and
        /// unique storage slot.
        pub fn total_bal_items(&self) -> u64 {
            super::total_bal_items(&self.0)
        }

        /// Validates this block access list against the block gas limit.
        ///
        /// EIP-7928 specifies that the total cost of the block access list items must not exceed
        /// the gas limit. Each item costs [`crate::constants::ITEM_COST`] gas.
        pub fn validate_gas_limit(&self, gas_limit: u64) -> Result<(), BlockAccessListGasError> {
            let items = self.total_bal_items();
            if items > gas_limit / crate::constants::ITEM_COST as u64 {
                return Err(BlockAccessListGasError::new(items, gas_limit));
            }
            Ok(())
        }

        /// Computes the hash of this block access list.
        #[cfg(feature = "rlp")]
        pub fn compute_hash(&self) -> alloy_primitives::B256 {
            if self.0.is_empty() {
                return crate::constants::EMPTY_BLOCK_ACCESS_LIST_HASH;
            }
            super::compute_block_access_list_hash(&self.0)
        }
    }

    /// Summary of change counts in a BAL for metrics reporting.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct BalChangeCounts {
        /// Number of accounts with changes.
        pub accounts: usize,
        /// Total number of storage changes.
        pub storage: usize,
        /// Total number of balance changes.
        pub balance: usize,
        /// Total number of nonce changes.
        pub nonce: usize,
        /// Total number of code changes.
        pub code: usize,
    }

    /// Raw RLP bytes for a block access list with lazy hash computation.
    #[derive(Clone, Debug)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(transparent))]
    pub struct RawBal {
        /// The original raw RLP bytes.
        raw: Bytes,
        /// Lazily computed hash of the block access list.
        #[cfg_attr(feature = "serde", serde(skip, default))]
        hash: OnceLock<B256>,
    }

    impl PartialEq for RawBal {
        #[inline]
        fn eq(&self, other: &Self) -> bool {
            self.raw == other.raw
        }
    }

    impl Eq for RawBal {}

    impl From<Bytes> for RawBal {
        #[inline]
        fn from(raw: Bytes) -> Self {
            Self::new(raw)
        }
    }

    impl RawBal {
        /// Creates a new [`RawBal`] from raw RLP bytes.
        #[inline]
        pub const fn new(raw: Bytes) -> Self {
            Self { raw, hash: OnceLock::new() }
        }

        /// Creates a new [`RawBal`] from raw RLP bytes and a precomputed hash.
        ///
        /// The hash is not checked against the raw bytes. Callers must ensure `hash` is the
        /// keccak256 hash of `raw`.
        #[inline]
        pub fn new_unchecked(raw: Bytes, hash: B256) -> Self {
            let this = Self::new(raw);
            #[allow(clippy::useless_conversion)]
            let _ = this.hash.get_or_init(|| hash.into());
            this
        }

        /// Returns the original raw RLP bytes.
        #[inline]
        pub const fn as_raw(&self) -> &Bytes {
            &self.raw
        }

        /// Consumes this value and returns the raw RLP bytes.
        #[inline]
        pub fn into_raw(self) -> Bytes {
            self.raw
        }

        /// Consumes this value and returns the raw RLP bytes and hash.
        #[inline]
        pub fn into_parts(self) -> (Bytes, B256) {
            let hash = self.hash();
            (self.raw, hash)
        }

        /// Ensures the raw RLP hash matches the expected block access list hash.
        #[inline]
        pub fn ensure_hash(&self, expected: B256) -> Result<(), BlockAccessListHashMismatch> {
            let computed = self.hash();
            if computed == expected {
                Ok(())
            } else {
                Err(BlockAccessListHashMismatch::new(computed, expected))
            }
        }

        /// Returns the hash of the raw block access list bytes.
        ///
        /// The hash is computed lazily on first call and cached for subsequent calls.
        #[inline]
        pub fn hash(&self) -> B256 {
            #[allow(clippy::useless_conversion)]
            *self.hash.get_or_init(|| alloy_primitives::keccak256(self.raw.as_ref()).into())
        }
    }

    #[cfg(feature = "rlp")]
    impl alloy_rlp::Encodable for RawBal {
        #[inline]
        fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
            out.put_slice(&self.raw);
        }

        #[inline]
        fn length(&self) -> usize {
            self.raw.len()
        }
    }

    #[cfg(feature = "rlp")]
    impl alloy_rlp::Decodable for RawBal {
        #[inline]
        fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
            let original = *buf;
            let header = alloy_rlp::Header::decode(buf)?;
            let header_len = original.len() - buf.len();
            let raw_len = header_len + header.payload_length;
            let raw = Bytes::copy_from_slice(&original[..raw_len]);
            *buf = &original[raw_len..];
            Ok(Self::new(raw))
        }
    }

    /// A decoded block access list with lazy hash computation.
    ///
    /// This type wraps a decoded block access list along with the original raw RLP bytes,
    /// allowing efficient hash computation on demand without re-encoding.
    #[derive(Clone, Debug)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub struct DecodedBal<T = Bal> {
        /// The decoded block access list.
        decoded: T,
        /// Raw RLP bytes and lazily computed hash of the block access list.
        raw: RawBal,
    }

    impl<T: PartialEq> PartialEq for DecodedBal<T> {
        #[inline]
        fn eq(&self, other: &Self) -> bool {
            self.decoded == other.decoded && self.raw == other.raw
        }
    }

    impl<T: Eq> Eq for DecodedBal<T> {}

    impl<T> DecodedBal<T> {
        /// Creates a new [`DecodedBal`] from decoded data and raw bytes.
        #[inline]
        pub const fn new(decoded: T, raw: Bytes) -> Self {
            Self { decoded, raw: RawBal::new(raw) }
        }

        /// Creates a new [`DecodedBal`] from decoded data, raw bytes, and a precomputed hash.
        ///
        /// The hash is not checked against the raw bytes. Callers must ensure `hash` is the
        /// keccak256 hash of `raw`.
        #[inline]
        pub fn new_unchecked(decoded: T, raw: Bytes, hash: B256) -> Self {
            Self { decoded, raw: RawBal::new_unchecked(raw, hash) }
        }

        /// Creates a new [`DecodedBal`] from decoded data and a [`RawBal`].
        #[inline]
        pub const fn with_raw_bal(decoded: T, raw: RawBal) -> Self {
            Self { decoded, raw }
        }

        /// Returns a reference to the decoded block access list.
        #[inline]
        pub const fn as_bal(&self) -> &T {
            &self.decoded
        }

        /// Returns the original raw RLP bytes.
        #[inline]
        pub const fn as_raw(&self) -> &Bytes {
            self.raw.as_raw()
        }

        /// Returns the raw BAL.
        #[inline]
        pub const fn as_raw_bal(&self) -> &RawBal {
            &self.raw
        }

        /// Splits this struct into the decoded BAL and raw bytes.
        #[inline]
        pub fn split(self) -> (T, Bytes) {
            (self.decoded, self.raw.into_raw())
        }

        /// Splits this struct into the decoded BAL and raw BAL.
        #[inline]
        pub fn split_raw_bal(self) -> (T, RawBal) {
            (self.decoded, self.raw)
        }

        /// Splits this struct into the decoded BAL, raw bytes, and hash.
        #[inline]
        pub fn into_parts(self) -> (T, Bytes, B256) {
            let hash = self.hash();
            let (decoded, raw) = self.split();
            (decoded, raw, hash)
        }

        /// Ensures the raw RLP hash matches the expected block access list hash.
        ///
        /// This checks `keccak256(raw_rlp_of_received_bal) == expected` using the cached hash of
        /// the original raw RLP bytes captured at decode time.
        #[inline]
        pub fn ensure_hash(&self, expected: B256) -> Result<(), BlockAccessListHashMismatch> {
            let computed = self.hash();
            if computed == expected {
                Ok(())
            } else {
                Err(BlockAccessListHashMismatch::new(computed, expected))
            }
        }

        /// Returns the hash of this block access list.
        ///
        /// The hash is computed lazily on first call and cached for subsequent calls.
        #[inline]
        pub fn hash(&self) -> B256 {
            self.raw.hash()
        }

        /// Converts the decoded BAL to the given alternative that is [`From<T>`].
        #[inline]
        pub fn convert<U>(self) -> DecodedBal<U>
        where
            U: From<T>,
        {
            self.map(U::from)
        }

        /// Converts the decoded BAL to the given alternative that is [`TryFrom<T>`].
        #[inline]
        pub fn try_convert<U>(self) -> Result<DecodedBal<U>, U::Error>
        where
            U: TryFrom<T>,
        {
            self.try_map(U::try_from)
        }

        /// Applies the given closure to the decoded BAL.
        #[inline]
        pub fn map<U>(self, f: impl FnOnce(T) -> U) -> DecodedBal<U> {
            let Self { decoded, raw } = self;
            DecodedBal { decoded: f(decoded), raw }
        }

        /// Applies the given fallible closure to the decoded BAL.
        #[inline]
        pub fn try_map<U, E>(self, f: impl FnOnce(T) -> Result<U, E>) -> Result<DecodedBal<U>, E> {
            let Self { decoded, raw } = self;
            Ok(DecodedBal { decoded: f(decoded)?, raw })
        }
    }

    #[cfg(feature = "rlp")]
    impl DecodedBal {
        /// Creates a new [`DecodedBal`] by decoding from raw RLP bytes.
        #[inline]
        pub fn from_rlp_bytes(raw: Bytes) -> Result<Self, alloy_rlp::Error> {
            Self::from_rlp_bytes_as(raw)
        }

        /// Creates a new [`DecodedBal`] by decoding from raw RLP bytes in a [`RawBal`].
        #[inline]
        pub fn from_raw_bal(raw: RawBal) -> Result<Self, alloy_rlp::Error> {
            Self::from_raw_bal_as(raw)
        }

        /// Creates a new [`DecodedBal`] by decoding from raw RLP bytes into `T`.
        #[inline]
        pub fn from_rlp_bytes_as<T>(raw: Bytes) -> Result<DecodedBal<T>, alloy_rlp::Error>
        where
            T: alloy_rlp::Decodable,
        {
            Self::from_raw_bal_as(RawBal::new(raw))
        }

        /// Creates a new [`DecodedBal`] by decoding from raw RLP bytes in a [`RawBal`] into `T`.
        #[inline]
        pub fn from_raw_bal_as<T>(raw: RawBal) -> Result<DecodedBal<T>, alloy_rlp::Error>
        where
            T: alloy_rlp::Decodable,
        {
            let mut slice = raw.as_raw().as_ref();
            let decoded = T::decode(&mut slice)?;
            if !slice.is_empty() {
                return Err(alloy_rlp::Error::UnexpectedLength);
            }
            Ok(DecodedBal::with_raw_bal(decoded, raw))
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> DecodedBal<T>
    where
        T: alloy_primitives::Sealable,
    {
        /// Returns the decoded BAL as a sealed borrowed value.
        #[inline]
        pub fn as_sealed_bal(&self) -> alloy_primitives::Sealed<&T> {
            alloy_primitives::Sealable::seal_ref_unchecked(&self.decoded, self.hash())
        }

        /// Consumes this struct and returns the decoded BAL together with its hash.
        #[inline]
        pub fn into_sealed(self) -> alloy_primitives::Sealed<T> {
            let seal = self.hash();
            let (decoded, _) = self.split();
            alloy_primitives::Sealable::seal_unchecked(decoded, seal)
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> alloy_rlp::Decodable for DecodedBal<T>
    where
        T: alloy_rlp::Decodable,
    {
        #[inline]
        fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
            let original = *buf;
            let decoded = T::decode(buf)?;
            let consumed = original.len() - buf.len();
            let raw = Bytes::copy_from_slice(&original[..consumed]);
            Ok(Self::new(decoded, raw))
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> alloy_rlp::Encodable for DecodedBal<T> {
        #[inline]
        fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
            alloy_rlp::Encodable::encode(&self.raw, out);
        }

        #[inline]
        fn length(&self) -> usize {
            alloy_rlp::Encodable::length(&self.raw)
        }
    }

    /// Either raw RLP bytes or a decoded block access list.
    ///
    /// This type is useful when callers may receive raw BAL bytes before the BAL needs to be
    /// decoded, while still allowing decoded values to preserve and re-use their original raw
    /// bytes.
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    pub enum RawOrDecodedBal<T = Bal> {
        /// Raw RLP bytes for a block access list with lazy hash computation.
        Raw(RawBal),
        /// A decoded block access list with its original raw RLP bytes.
        Decoded(DecodedBal<T>),
    }

    impl<T> From<Bytes> for RawOrDecodedBal<T> {
        #[inline]
        fn from(raw: Bytes) -> Self {
            Self::Raw(RawBal::new(raw))
        }
    }

    impl<T> From<RawBal> for RawOrDecodedBal<T> {
        #[inline]
        fn from(raw: RawBal) -> Self {
            Self::Raw(raw)
        }
    }

    impl<T> From<DecodedBal<T>> for RawOrDecodedBal<T> {
        #[inline]
        fn from(decoded: DecodedBal<T>) -> Self {
            Self::Decoded(decoded)
        }
    }

    impl<T> RawOrDecodedBal<T> {
        /// Creates a new [`RawOrDecodedBal`] from raw RLP bytes.
        #[inline]
        pub const fn raw(raw: Bytes) -> Self {
            Self::Raw(RawBal::new(raw))
        }

        /// Creates a new [`RawOrDecodedBal`] from raw RLP bytes and a precomputed hash.
        ///
        /// The hash is not checked against the raw bytes. Callers must ensure `hash` is the
        /// keccak256 hash of `raw`.
        #[inline]
        pub fn raw_unchecked(raw: Bytes, hash: B256) -> Self {
            Self::Raw(RawBal::new_unchecked(raw, hash))
        }

        /// Creates a new [`RawOrDecodedBal`] from a [`RawBal`].
        #[inline]
        pub const fn raw_bal(raw: RawBal) -> Self {
            Self::Raw(raw)
        }

        /// Creates a new [`RawOrDecodedBal`] from a decoded BAL.
        #[inline]
        pub const fn decoded(decoded: DecodedBal<T>) -> Self {
            Self::Decoded(decoded)
        }

        /// Returns `true` if this contains raw RLP bytes.
        #[inline]
        pub const fn is_raw(&self) -> bool {
            matches!(self, Self::Raw(_))
        }

        /// Returns `true` if this contains a decoded BAL.
        #[inline]
        pub const fn is_decoded(&self) -> bool {
            matches!(self, Self::Decoded(_))
        }

        /// Returns the raw RLP bytes.
        #[inline]
        pub const fn as_raw(&self) -> &Bytes {
            match self {
                Self::Raw(raw) => raw.as_raw(),
                Self::Decoded(decoded) => decoded.as_raw(),
            }
        }

        /// Returns the raw BAL.
        #[inline]
        pub const fn as_raw_bal(&self) -> &RawBal {
            match self {
                Self::Raw(raw) => raw,
                Self::Decoded(decoded) => decoded.as_raw_bal(),
            }
        }

        /// Returns the decoded BAL if available.
        #[inline]
        pub const fn as_decoded(&self) -> Option<&DecodedBal<T>> {
            match self {
                Self::Raw(_) => None,
                Self::Decoded(decoded) => Some(decoded),
            }
        }

        /// Returns the decoded BAL if available.
        #[inline]
        pub fn into_decoded(self) -> Option<DecodedBal<T>> {
            match self {
                Self::Raw(_) => None,
                Self::Decoded(decoded) => Some(decoded),
            }
        }

        /// Returns the decoded block access list if available.
        #[inline]
        pub const fn as_bal(&self) -> Option<&T> {
            match self {
                Self::Raw(_) => None,
                Self::Decoded(decoded) => Some(decoded.as_bal()),
            }
        }

        /// Consumes this value and returns the raw RLP bytes.
        #[inline]
        pub fn into_raw(self) -> Bytes {
            match self {
                Self::Raw(raw) => raw.into_raw(),
                Self::Decoded(decoded) => decoded.split().1,
            }
        }

        /// Consumes this value and returns the raw BAL.
        #[inline]
        pub fn into_raw_bal(self) -> RawBal {
            match self {
                Self::Raw(raw) => raw,
                Self::Decoded(decoded) => decoded.split_raw_bal().1,
            }
        }

        /// Splits this value into its decoded BAL, if available, and raw RLP bytes.
        #[inline]
        pub fn split(self) -> (Option<T>, Bytes) {
            match self {
                Self::Raw(raw) => (None, raw.into_raw()),
                Self::Decoded(decoded) => {
                    let (bal, raw) = decoded.split();
                    (Some(bal), raw)
                }
            }
        }

        /// Splits this value into its decoded BAL, if available, and raw BAL.
        #[inline]
        pub fn split_raw_bal(self) -> (Option<T>, RawBal) {
            match self {
                Self::Raw(raw) => (None, raw),
                Self::Decoded(decoded) => {
                    let (bal, raw) = decoded.split_raw_bal();
                    (Some(bal), raw)
                }
            }
        }

        /// Ensures the raw RLP hash matches the expected block access list hash.
        #[inline]
        pub fn ensure_hash(&self, expected: B256) -> Result<(), BlockAccessListHashMismatch> {
            let computed = self.hash();
            if computed == expected {
                Ok(())
            } else {
                Err(BlockAccessListHashMismatch::new(computed, expected))
            }
        }

        /// Returns the hash of the raw block access list bytes.
        #[inline]
        pub fn hash(&self) -> B256 {
            match self {
                Self::Raw(raw) => raw.hash(),
                Self::Decoded(decoded) => decoded.hash(),
            }
        }

        /// Converts the decoded BAL to the given alternative that is [`From<T>`].
        ///
        /// Raw values stay raw.
        #[inline]
        pub fn convert<U>(self) -> RawOrDecodedBal<U>
        where
            U: From<T>,
        {
            self.map(U::from)
        }

        /// Converts the decoded BAL to the given alternative that is [`TryFrom<T>`].
        ///
        /// Raw values stay raw.
        #[inline]
        pub fn try_convert<U>(self) -> Result<RawOrDecodedBal<U>, U::Error>
        where
            U: TryFrom<T>,
        {
            self.try_map(U::try_from)
        }

        /// Applies the given closure to the decoded BAL if available.
        #[inline]
        pub fn map<U>(self, f: impl FnOnce(T) -> U) -> RawOrDecodedBal<U> {
            match self {
                Self::Raw(raw) => RawOrDecodedBal::Raw(raw),
                Self::Decoded(decoded) => RawOrDecodedBal::Decoded(decoded.map(f)),
            }
        }

        /// Applies the given fallible closure to the decoded BAL if available.
        #[inline]
        pub fn try_map<U, E>(
            self,
            f: impl FnOnce(T) -> Result<U, E>,
        ) -> Result<RawOrDecodedBal<U>, E> {
            match self {
                Self::Raw(raw) => Ok(RawOrDecodedBal::Raw(raw)),
                Self::Decoded(decoded) => decoded.try_map(f).map(RawOrDecodedBal::Decoded),
            }
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> RawOrDecodedBal<T>
    where
        T: alloy_rlp::Decodable,
    {
        /// Up-converts raw RLP bytes into a decoded BAL, or returns the existing decoded BAL.
        #[inline]
        pub fn try_into_decoded(self) -> Result<DecodedBal<T>, alloy_rlp::Error> {
            match self {
                Self::Raw(raw) => DecodedBal::from_raw_bal_as(raw),
                Self::Decoded(decoded) => Ok(decoded),
            }
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> alloy_rlp::Encodable for RawOrDecodedBal<T> {
        #[inline]
        fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
            out.put_slice(self.as_raw());
        }

        #[inline]
        fn length(&self) -> usize {
            self.as_raw().len()
        }
    }

    #[cfg(feature = "rlp")]
    impl<T> alloy_rlp::Decodable for RawOrDecodedBal<T> {
        #[inline]
        fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
            <RawBal as alloy_rlp::Decodable>::decode(buf).map(Self::Raw)
        }
    }
}

/// Error returned when a block access list item cost exceeds the block gas limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error(
    "block access list item cost exceeds gas limit: items={items}, max_items={max_items}, gas_limit={gas_limit}"
)]
pub struct BlockAccessListGasError {
    /// Number of block access list items.
    pub items: u64,
    /// Maximum number of block access list items allowed by the gas limit.
    pub max_items: u64,
    /// Block gas limit used for validation.
    pub gas_limit: u64,
}

impl BlockAccessListGasError {
    /// Creates a new gas limit validation error for the provided item count and gas limit.
    #[inline]
    pub const fn new(items: u64, gas_limit: u64) -> Self {
        Self { items, max_items: gas_limit / crate::constants::ITEM_COST as u64, gas_limit }
    }
}

/// Error returned when a decoded block access list hash does not match the expected hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("block access list hash mismatch: computed={computed}, expected={expected}")]
pub struct BlockAccessListHashMismatch {
    /// Hash computed from the received BAL bytes.
    pub computed: alloy_primitives::B256,
    /// Hash expected by the caller, typically `header.block_access_list_hash`.
    pub expected: alloy_primitives::B256,
}

impl BlockAccessListHashMismatch {
    /// Creates a new block access list hash validation error.
    #[inline]
    pub const fn new(computed: alloy_primitives::B256, expected: alloy_primitives::B256) -> Self {
        Self { computed, expected }
    }
}

#[cfg(test)]
mod hash_tests {
    use super::bal::{Bal, DecodedBal, RawBal, RawOrDecodedBal};
    use crate::{
        AccountChanges, BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges,
        StorageChange, constants::ITEM_COST,
    };
    use alloy_primitives::{Address, B256, Bytes, U256};

    #[test]
    fn decoded_bal_hash_uses_raw_bytes_without_rlp_feature() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());

        assert_eq!(decoded.hash(), alloy_primitives::keccak256(raw.as_ref()));

        let (bal, split_raw, split_hash) = decoded.into_parts();
        assert!(bal.is_empty());
        assert_eq!(split_raw, raw);
        assert_eq!(split_hash, alloy_primitives::keccak256(raw.as_ref()));
    }

    #[test]
    fn decoded_bal_map_preserves_raw_and_hash() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());
        let hash = decoded.hash();

        let mapped = decoded.map(|bal| bal.len());

        assert_eq!(mapped.as_bal(), &0);
        assert_eq!(mapped.as_raw(), &raw);
        assert_eq!(mapped.hash(), hash);
    }

    #[test]
    fn decoded_bal_try_map_converts_or_returns_error() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());

        let mapped = decoded.try_map(|bal| Ok::<_, core::convert::Infallible>(bal.len())).unwrap();

        assert_eq!(mapped.as_bal(), &0);
        assert_eq!(mapped.as_raw(), &raw);

        let decoded = DecodedBal::new(Bal::default(), raw);
        let err = decoded.try_map(|_| Err::<usize, _>("expected error")).unwrap_err();

        assert_eq!(err, "expected error");
    }

    #[test]
    fn bal_as_vec_returns_inner_vector_ref() {
        let bal = Bal::new(vec![AccountChanges::new(Address::from([0x11; 20]))]);

        assert_eq!(bal.as_vec().len(), 1);
        assert_eq!(bal.as_vec().as_slice(), bal.as_slice());
    }

    #[derive(Debug, PartialEq, Eq)]
    struct BalLen(usize);

    impl From<Bal> for BalLen {
        fn from(value: Bal) -> Self {
            Self(value.len())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct NonEmptyBal(Bal);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct EmptyBal;

    impl TryFrom<Bal> for NonEmptyBal {
        type Error = EmptyBal;

        fn try_from(value: Bal) -> Result<Self, Self::Error> {
            if value.is_empty() { Err(EmptyBal) } else { Ok(Self(value)) }
        }
    }

    #[test]
    fn decoded_bal_convert_and_try_convert_use_inner_conversions() {
        let raw = Bytes::from_static(&[0xc0]);
        let converted: DecodedBal<BalLen> = DecodedBal::new(Bal::default(), raw.clone()).convert();

        assert_eq!(converted.as_bal(), &BalLen(0));
        assert_eq!(converted.as_raw(), &raw);

        let err = DecodedBal::new(Bal::default(), raw.clone()).try_convert::<NonEmptyBal>();
        assert_eq!(err.unwrap_err(), EmptyBal);

        let bal = Bal::new(vec![AccountChanges::new(Address::from([0x11; 20]))]);
        let converted = DecodedBal::new(bal, raw).try_convert::<NonEmptyBal>().unwrap();

        assert_eq!(converted.as_bal().0.len(), 1);
    }

    #[test]
    fn decoded_bal_ensure_hash_reports_both_hashes() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());
        let computed = alloy_primitives::keccak256(raw.as_ref());
        let expected = B256::from([0x11; 32]);

        assert_eq!(decoded.ensure_hash(computed), Ok(()));
        assert_eq!(
            decoded.ensure_hash(expected),
            Err(super::BlockAccessListHashMismatch::new(computed, expected))
        );
    }

    #[test]
    fn raw_bal_hash_uses_raw_bytes() {
        let raw = Bytes::from_static(&[0xc0]);
        let raw_bal = RawBal::new(raw.clone());
        let computed = alloy_primitives::keccak256(raw.as_ref());
        let expected = B256::from([0x11; 32]);

        assert_eq!(raw_bal.as_raw(), &raw);
        assert_eq!(raw_bal.hash(), computed);
        assert_eq!(raw_bal.ensure_hash(computed), Ok(()));
        assert_eq!(
            raw_bal.ensure_hash(expected),
            Err(super::BlockAccessListHashMismatch::new(computed, expected))
        );

        let (split_raw, split_hash) = raw_bal.into_parts();
        assert_eq!(split_raw, raw);
        assert_eq!(split_hash, computed);
    }

    #[test]
    fn raw_bal_new_unchecked_uses_supplied_hash() {
        let raw = Bytes::from_static(&[0xc0]);
        let hash = B256::from([0x11; 32]);
        let raw_bal = RawBal::new_unchecked(raw.clone(), hash);

        assert_eq!(raw_bal.as_raw(), &raw);
        assert_eq!(raw_bal.hash(), hash);
        assert_eq!(raw_bal.ensure_hash(hash), Ok(()));

        let (split_raw, split_hash) = raw_bal.into_parts();
        assert_eq!(split_raw, raw);
        assert_eq!(split_hash, hash);
    }

    #[test]
    fn decoded_bal_exposes_raw_bal() {
        let raw = Bytes::from_static(&[0xc0]);
        let raw_bal = RawBal::new(raw.clone());
        let decoded = DecodedBal::with_raw_bal(Bal::default(), raw_bal.clone());

        assert_eq!(decoded.as_raw_bal(), &raw_bal);
        assert_eq!(decoded.as_raw(), &raw);

        let (bal, split_raw_bal) = decoded.split_raw_bal();
        assert!(bal.is_empty());
        assert_eq!(split_raw_bal, raw_bal);
    }

    #[test]
    fn decoded_bal_new_unchecked_uses_supplied_hash() {
        let raw = Bytes::from_static(&[0xc0]);
        let hash = B256::from([0x11; 32]);
        let decoded = DecodedBal::new_unchecked(Bal::default(), raw.clone(), hash);

        assert_eq!(decoded.as_raw(), &raw);
        assert_eq!(decoded.hash(), hash);
        assert_eq!(decoded.ensure_hash(hash), Ok(()));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn decoded_bal_serde_keeps_raw_bytes_field() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());
        let value = serde_json::to_value(&decoded).unwrap();

        assert!(value.get("decoded").is_some());
        assert_eq!(value.get("raw"), Some(&serde_json::to_value(&raw).unwrap()));
        assert!(value.get("hash").is_none());

        let decoded = serde_json::from_value::<DecodedBal>(value).unwrap();
        assert_eq!(decoded.as_bal(), &Bal::default());
        assert_eq!(decoded.as_raw(), &raw);
    }

    #[test]
    fn raw_or_decoded_bal_raw_helpers_use_raw_bytes() {
        let raw = Bytes::from_static(&[0xc0]);
        let bal = RawOrDecodedBal::<Bal>::raw(raw.clone());
        let hash = alloy_primitives::keccak256(raw.as_ref());

        assert!(bal.is_raw());
        assert!(!bal.is_decoded());
        assert_eq!(bal.as_raw(), &raw);
        assert_eq!(bal.as_raw_bal().as_raw(), &raw);
        assert_eq!(bal.as_decoded(), None);
        assert_eq!(bal.as_bal(), None);
        assert_eq!(bal.hash(), hash);
        assert_eq!(bal.ensure_hash(hash), Ok(()));

        let (decoded, split_raw) = bal.clone().split();
        assert_eq!(decoded, None);
        assert_eq!(split_raw, raw);
        let (decoded, split_raw_bal) = bal.clone().split_raw_bal();
        assert_eq!(decoded, None);
        assert_eq!(split_raw_bal.as_raw(), &raw);
        assert_eq!(bal.clone().into_raw_bal().as_raw(), &raw);
        assert_eq!(bal.into_raw(), raw);
    }

    #[test]
    fn raw_or_decoded_bal_raw_unchecked_uses_supplied_hash() {
        let raw = Bytes::from_static(&[0xc0]);
        let hash = B256::from([0x11; 32]);
        let bal = RawOrDecodedBal::<Bal>::raw_unchecked(raw.clone(), hash);

        assert!(bal.is_raw());
        assert_eq!(bal.as_raw(), &raw);
        assert_eq!(bal.hash(), hash);
        assert_eq!(bal.ensure_hash(hash), Ok(()));
    }

    #[test]
    fn raw_or_decoded_bal_decoded_helpers_use_decoded_bal() {
        let raw = Bytes::from_static(&[0xc0]);
        let decoded = DecodedBal::new(Bal::default(), raw.clone());
        let hash = decoded.hash();
        let bal = RawOrDecodedBal::decoded(decoded.clone());

        assert!(!bal.is_raw());
        assert!(bal.is_decoded());
        assert_eq!(bal.as_raw(), &raw);
        assert_eq!(bal.as_raw_bal(), decoded.as_raw_bal());
        assert_eq!(bal.as_decoded(), Some(&decoded));
        assert_eq!(bal.as_bal(), Some(decoded.as_bal()));
        assert_eq!(bal.hash(), hash);

        let (split_bal, split_raw) = bal.clone().split();
        assert_eq!(split_bal, Some(Bal::default()));
        assert_eq!(split_raw, raw);
        let (split_bal, split_raw_bal) = bal.clone().split_raw_bal();
        assert_eq!(split_bal, Some(Bal::default()));
        assert_eq!(split_raw_bal.as_raw(), &raw);
        assert_eq!(bal.into_decoded(), Some(decoded));
    }

    #[test]
    fn raw_or_decoded_bal_convert_maps_only_decoded_values() {
        let raw = Bytes::from_static(&[0xc0]);
        let raw_bal: RawOrDecodedBal<Bal> = RawOrDecodedBal::raw(raw.clone());
        let converted_raw: RawOrDecodedBal<BalLen> = raw_bal.convert();

        assert!(converted_raw.is_raw());
        assert_eq!(converted_raw.as_raw(), &raw);
        assert_eq!(converted_raw.as_bal(), None);

        let decoded = DecodedBal::new(Bal::default(), raw.clone());
        let converted_decoded: RawOrDecodedBal<BalLen> =
            RawOrDecodedBal::decoded(decoded).convert();

        assert!(converted_decoded.is_decoded());
        assert_eq!(converted_decoded.as_bal(), Some(&BalLen(0)));
        assert_eq!(converted_decoded.as_raw(), &raw);

        let err = RawOrDecodedBal::decoded(DecodedBal::new(Bal::default(), raw.clone()))
            .try_convert::<NonEmptyBal>();
        assert_eq!(err.unwrap_err(), EmptyBal);

        let raw_result: Result<RawOrDecodedBal<NonEmptyBal>, EmptyBal> =
            RawOrDecodedBal::<Bal>::raw(raw.clone()).try_convert();
        let raw_result = raw_result.unwrap();
        assert!(raw_result.is_raw());
        assert_eq!(raw_result.as_raw(), &raw);
    }

    #[test]
    fn bal_sort_orders_all_eip7928_lists() {
        let address_1 = Address::from([0x11; 20]);
        let address_2 = Address::from([0x22; 20]);
        let mut bal = Bal::new(vec![
            AccountChanges {
                address: address_2,
                storage_changes: vec![
                    SlotChanges::new(
                        U256::from(3),
                        vec![
                            StorageChange::new(BlockAccessIndex::new(8), U256::from(0x80)),
                            StorageChange::new(BlockAccessIndex::new(2), U256::from(0x20)),
                        ],
                    ),
                    SlotChanges::new(
                        U256::from(1),
                        vec![
                            StorageChange::new(BlockAccessIndex::new(5), U256::from(0x50)),
                            StorageChange::new(BlockAccessIndex::new(1), U256::from(0x10)),
                        ],
                    ),
                ],
                storage_reads: vec![U256::from(4), U256::from(2)],
                balance_changes: vec![
                    BalanceChange::new(BlockAccessIndex::new(6), U256::from(600)),
                    BalanceChange::new(BlockAccessIndex::new(3), U256::from(300)),
                ],
                nonce_changes: vec![
                    NonceChange::new(BlockAccessIndex::new(7), 70),
                    NonceChange::new(BlockAccessIndex::new(4), 40),
                ],
                code_changes: vec![
                    CodeChange::new(BlockAccessIndex::new(9), Bytes::from_static(&[0x60, 0x09])),
                    CodeChange::new(BlockAccessIndex::new(5), Bytes::from_static(&[0x60, 0x05])),
                ],
            },
            AccountChanges {
                address: address_1,
                storage_changes: vec![
                    SlotChanges::new(
                        U256::from(2),
                        vec![
                            StorageChange::new(BlockAccessIndex::new(4), U256::from(0x40)),
                            StorageChange::new(BlockAccessIndex::new(0), U256::from(0x00)),
                        ],
                    ),
                    SlotChanges::new(
                        U256::from(1),
                        vec![
                            StorageChange::new(BlockAccessIndex::new(3), U256::from(0x30)),
                            StorageChange::new(BlockAccessIndex::new(1), U256::from(0x10)),
                        ],
                    ),
                ],
                storage_reads: vec![U256::from(5), U256::from(3)],
                balance_changes: vec![
                    BalanceChange::new(BlockAccessIndex::new(5), U256::from(500)),
                    BalanceChange::new(BlockAccessIndex::new(2), U256::from(200)),
                ],
                nonce_changes: vec![
                    NonceChange::new(BlockAccessIndex::new(8), 80),
                    NonceChange::new(BlockAccessIndex::new(1), 10),
                ],
                code_changes: vec![
                    CodeChange::new(BlockAccessIndex::new(4), Bytes::from_static(&[0x60, 0x04])),
                    CodeChange::new(BlockAccessIndex::new(2), Bytes::from_static(&[0x60, 0x02])),
                ],
            },
        ]);

        bal.sort();

        assert_eq!(bal[0].address, address_1);
        assert_eq!(bal[1].address, address_2);

        for account in bal.iter() {
            assert!(account.storage_changes.windows(2).all(|slots| slots[0].slot <= slots[1].slot));
            for slot_changes in &account.storage_changes {
                assert!(
                    slot_changes
                        .changes
                        .windows(2)
                        .all(|changes| changes[0].block_access_index
                            <= changes[1].block_access_index)
                );
            }
            assert!(account.storage_reads.windows(2).all(|slots| slots[0] <= slots[1]));
            assert!(
                account
                    .balance_changes
                    .windows(2)
                    .all(|changes| changes[0].block_access_index <= changes[1].block_access_index)
            );
            assert!(
                account
                    .nonce_changes
                    .windows(2)
                    .all(|changes| changes[0].block_access_index <= changes[1].block_access_index)
            );
            assert!(
                account
                    .code_changes
                    .windows(2)
                    .all(|changes| changes[0].block_access_index <= changes[1].block_access_index)
            );
        }
    }

    #[test]
    fn bal_validate_gas_limit_accepts_exact_item_cost() {
        let bal = Bal::new(vec![
            AccountChanges::new(Address::from([0x11; 20]))
                .with_storage_read(U256::from(1))
                .with_storage_change(SlotChanges::new(
                    U256::from(2),
                    vec![StorageChange::new(BlockAccessIndex::new(0), U256::from(0xaa))],
                )),
        ]);

        assert_eq!(bal.total_bal_items(), 3);
        assert_eq!(bal.validate_gas_limit(3 * ITEM_COST as u64), Ok(()));
    }

    #[test]
    fn bal_total_items_counts_storage_entries_without_deduplicating() {
        let bal = Bal::new(vec![
            AccountChanges::new(Address::from([0x11; 20]))
                .with_storage_read(U256::from(1))
                .with_storage_change(SlotChanges::new(
                    U256::from(1),
                    vec![StorageChange::new(BlockAccessIndex::new(0), U256::from(0xaa))],
                )),
        ]);

        assert_eq!(bal.total_bal_items(), 3);
    }

    #[test]
    fn bal_validate_gas_limit_rejects_item_cost_above_limit() {
        let bal = Bal::new(vec![
            AccountChanges::new(Address::from([0x11; 20]))
                .with_storage_read(U256::from(1))
                .with_storage_read(U256::from(2)),
        ]);
        let gas_limit = 3 * ITEM_COST as u64 - 1;

        assert_eq!(bal.total_bal_items(), 3);
        assert_eq!(
            bal.validate_gas_limit(gas_limit),
            Err(super::BlockAccessListGasError::new(3, gas_limit))
        );
    }
}

#[cfg(all(test, feature = "rlp"))]
mod tests {
    use super::bal::{Bal, DecodedBal, RawBal, RawOrDecodedBal};
    use crate::{
        AccountChanges, BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges,
        StorageChange, constants::EMPTY_BLOCK_ACCESS_LIST_HASH,
    };
    use alloy_primitives::{Address, Bytes, U256};

    fn sample_bal() -> Bal {
        Bal::new(vec![
            AccountChanges::new(Address::from([0x11; 20]))
                .with_storage_read(U256::from(0x10))
                .with_storage_change(SlotChanges::new(
                    U256::from(0x01),
                    vec![StorageChange::new(BlockAccessIndex::new(0), U256::from(0xaa))],
                ))
                .with_balance_change(BalanceChange::new(
                    BlockAccessIndex::new(1),
                    U256::from(1_000),
                ))
                .with_nonce_change(NonceChange::new(BlockAccessIndex::new(2), 7))
                .with_code_change(CodeChange::new(
                    BlockAccessIndex::new(3),
                    Bytes::from(vec![0x60, 0x00]),
                )),
            AccountChanges::new(Address::from([0x22; 20]))
                .with_storage_read(U256::from(0x20))
                .with_storage_change(SlotChanges::new(
                    U256::from(0x02),
                    vec![StorageChange::new(BlockAccessIndex::new(4), U256::from(0xbb))],
                )),
        ])
    }

    #[test]
    fn bal_compute_hash_returns_empty_hash_for_empty_bal() {
        let bal = Bal::default();

        assert_eq!(bal.compute_hash(), EMPTY_BLOCK_ACCESS_LIST_HASH);
    }

    #[test]
    fn bal_compute_hash_matches_free_function_for_non_empty_bal() {
        let bal = sample_bal();

        assert_eq!(bal.compute_hash(), super::compute_block_access_list_hash(bal.as_slice()));
        assert_ne!(bal.compute_hash(), EMPTY_BLOCK_ACCESS_LIST_HASH);
    }

    #[test]
    fn decoded_bal_from_rlp_bytes_preserves_raw_and_hash() {
        let bal = sample_bal();
        let raw = Bytes::from(alloy_rlp::encode(&bal));
        let decoded = DecodedBal::from_rlp_bytes(raw.clone()).unwrap();

        assert_eq!(decoded.as_bal(), &bal);
        assert_eq!(decoded.as_raw(), &raw);
        assert_eq!(decoded.hash(), bal.compute_hash());
        assert_eq!(decoded.hash(), alloy_primitives::keccak256(raw.as_ref()));
        assert_eq!(decoded.as_sealed_bal().hash(), bal.compute_hash());
        assert_eq!(decoded.as_sealed_bal().inner(), &decoded.as_bal());

        let (split_bal, split_raw) = decoded.clone().split();
        assert_eq!(split_bal, bal);
        assert_eq!(split_raw, raw);

        let (split_bal, split_raw, split_hash) = decoded.clone().into_parts();
        assert_eq!(split_bal, bal);
        assert_eq!(split_raw, raw);
        assert_eq!(split_hash, bal.compute_hash());

        let sealed = decoded.into_sealed();
        assert_eq!(sealed.hash(), bal.compute_hash());
        assert_eq!(sealed.inner(), &bal);
    }

    #[test]
    fn decoded_bal_from_rlp_bytes_decodes_generic_inner_type() {
        let bal = sample_bal();
        let raw = Bytes::from(alloy_rlp::encode(&bal));
        let decoded = DecodedBal::from_rlp_bytes_as::<Vec<AccountChanges>>(raw.clone()).unwrap();

        assert_eq!(decoded.as_bal().as_slice(), bal.as_slice());
        assert_eq!(decoded.as_raw(), &raw);
        assert_eq!(decoded.hash(), bal.compute_hash());
    }

    #[test]
    fn decoded_bal_decode_consumes_exact_raw_rlp_item() {
        let bal = sample_bal();
        let raw = alloy_rlp::encode(&bal);
        let mut buf = raw.as_ref();
        let decoded = <DecodedBal as alloy_rlp::Decodable>::decode(&mut buf).unwrap();

        assert!(buf.is_empty());
        assert_eq!(decoded.as_bal(), &bal);
        assert_eq!(decoded.as_raw().as_ref(), raw.as_slice());
        assert_eq!(alloy_rlp::encode(&decoded), raw);
    }

    #[test]
    fn raw_bal_rlp_roundtrip_preserves_raw_item() {
        let bal = sample_bal();
        let raw = alloy_rlp::encode(&bal);
        let mut buf = raw.as_ref();
        let raw_bal = <RawBal as alloy_rlp::Decodable>::decode(&mut buf).unwrap();

        assert!(buf.is_empty());
        assert_eq!(raw_bal.as_raw().as_ref(), raw.as_slice());
        assert_eq!(alloy_rlp::encode(&raw_bal), raw);
        assert_eq!(raw_bal.hash(), bal.compute_hash());
    }

    #[test]
    fn raw_or_decoded_bal_try_into_decoded_decodes_raw() {
        let bal = sample_bal();
        let raw = Bytes::from(alloy_rlp::encode(&bal));
        let decoded = RawOrDecodedBal::<Bal>::raw(raw.clone()).try_into_decoded().unwrap();

        assert_eq!(decoded.as_bal(), &bal);
        assert_eq!(decoded.as_raw(), &raw);
        assert_eq!(decoded.hash(), bal.compute_hash());
    }

    #[test]
    fn raw_or_decoded_bal_try_into_decoded_reuses_decoded() {
        let bal = sample_bal();
        let raw = Bytes::from(alloy_rlp::encode(&bal));
        let decoded = DecodedBal::new(bal.clone(), raw.clone());
        let decoded = RawOrDecodedBal::decoded(decoded).try_into_decoded().unwrap();

        assert_eq!(decoded.as_bal(), &bal);
        assert_eq!(decoded.as_raw(), &raw);
        assert_eq!(decoded.hash(), bal.compute_hash());
    }

    #[test]
    fn raw_or_decoded_bal_rlp_encodes_raw_bytes() {
        let bal = sample_bal();
        let raw = alloy_rlp::encode(&bal);
        let raw_bal = RawOrDecodedBal::<Bal>::raw(Bytes::from(raw.clone()));
        let decoded_bal = RawOrDecodedBal::decoded(DecodedBal::new(bal, Bytes::from(raw.clone())));

        assert_eq!(alloy_rlp::encode(&raw_bal), raw);
        assert_eq!(alloy_rlp::encode(&decoded_bal), raw);
    }

    #[test]
    fn raw_or_decoded_bal_decode_preserves_raw_rlp_item() {
        let bal = sample_bal();
        let raw = alloy_rlp::encode(&bal);
        let mut buf = raw.as_ref();
        let decoded = <RawOrDecodedBal as alloy_rlp::Decodable>::decode(&mut buf).unwrap();

        assert!(buf.is_empty());
        assert!(decoded.is_raw());
        assert_eq!(decoded.as_raw().as_ref(), raw.as_slice());
        assert_eq!(alloy_rlp::encode(&decoded), raw);

        let decoded = decoded.try_into_decoded().unwrap();
        assert_eq!(decoded.as_bal(), &bal);
    }
}
