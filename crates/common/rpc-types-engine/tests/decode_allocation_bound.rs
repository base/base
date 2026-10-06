//! Pre-authentication allocation regressions for all network payload versions.
//!
//! This lives in its own integration test binary rather than a unit test so the
//! `#[global_allocator]` below is isolated to a single binary: a crate-wide
//! allocator declared inside the crate's `#[cfg(test)] mod tests` would apply to
//! every test in that binary (and would collide with any other test that
//! declared its own allocator). Measurements are isolated per calling thread.

#![cfg(feature = "std")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::Bytes;
use base_common_rpc_types_engine::{
    MAX_DECOMPRESSED_ENVELOPE_BYTES, MAX_TRANSACTIONS_PER_PAYLOAD, NetworkPayloadEnvelope,
    PayloadEnvelopeError,
};
use ssz::Decode;

// Running total of bytes allocated on the current thread. Const-initialized so
// reading it never allocates (which would recurse through the allocator).
thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// Global allocator that tallies allocation volume per thread, delegating to the
/// system allocator. Lets the test measure how much heap a single decode forces.
/// Only allocation (growth) is counted, which is what a resource-exhaustion
/// bound cares about.
#[derive(Debug)]
pub struct CountingAllocator;

// SAFETY: every call is forwarded to the system allocator with an unchanged
// layout, so all `GlobalAlloc` invariants are those of `System`; the wrapper
// only records the requested size on a successful allocation.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged from the caller, upholding
        // `System::alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let _ = ALLOCATED.try_with(|c| c.set(c.get().saturating_add(layout.size())));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` come straight from the caller and originate
        // from `System.alloc`, satisfying `System::dealloc`.
        unsafe { System.dealloc(ptr, layout) };
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: CountingAllocator = CountingAllocator;

/// Wire layouts from the versioned SSZ execution payload schemas.
#[derive(Clone, Copy, Debug)]
pub struct PayloadVersion {
    /// Payload version used in assertion diagnostics.
    pub name: &'static str,
    /// Size of the SSZ container's fixed region.
    pub fixed_len: usize,
    /// Signature and optional parent beacon block root preceding the container.
    pub envelope_prefix: usize,
    /// Whether this version has a withdrawals list.
    pub has_withdrawals: bool,
    /// Public network decoder exercised by this case.
    pub decode: fn(&[u8]) -> Result<NetworkPayloadEnvelope, PayloadEnvelopeError>,
}

impl PayloadVersion {
    /// Every subscribed block payload version.
    pub const ALL: [Self; 4] = [
        Self {
            name: "v1",
            fixed_len: 508,
            envelope_prefix: 65,
            has_withdrawals: false,
            decode: NetworkPayloadEnvelope::decode_v1,
        },
        Self {
            name: "v2",
            fixed_len: 512,
            envelope_prefix: 65,
            has_withdrawals: true,
            decode: NetworkPayloadEnvelope::decode_v2,
        },
        Self {
            name: "v3",
            fixed_len: 528,
            envelope_prefix: 97,
            has_withdrawals: true,
            decode: NetworkPayloadEnvelope::decode_v3,
        },
        Self {
            name: "v4",
            fixed_len: 560,
            envelope_prefix: 97,
            has_withdrawals: true,
            decode: NetworkPayloadEnvelope::decode_v4,
        },
    ];

    /// Creates a compressed envelope without allocating a transaction object per element.
    ///
    /// All transaction offsets point to the end of the list, encoding empty items.
    /// Withdrawals are zero-filled; non-multiple lengths exercise truncated items.
    pub fn frame(self, transaction_count: usize, withdrawals_len: usize) -> Vec<u8> {
        const EXTRA_DATA_OFFSET: usize = 436;
        const TRANSACTIONS_OFFSET: usize = 504;
        const WITHDRAWALS_OFFSET: usize = 508;

        let transactions_len = transaction_count * ssz::BYTES_PER_LENGTH_OFFSET;
        let decoded_len =
            self.envelope_prefix + self.fixed_len + transactions_len + withdrawals_len;
        assert!(decoded_len <= MAX_DECOMPRESSED_ENVELOPE_BYTES);
        assert!(self.has_withdrawals || withdrawals_len == 0);

        let mut data = Vec::with_capacity(decoded_len);
        data.resize(self.envelope_prefix + self.fixed_len, 0);
        // Structurally parseable signature; no sequencer authentication is needed to decode.
        data[31] = 1;
        data[63] = 1;
        let container = &mut data[self.envelope_prefix..];
        container[EXTRA_DATA_OFFSET..][..4].copy_from_slice(&(self.fixed_len as u32).to_le_bytes());
        container[TRANSACTIONS_OFFSET..][..4]
            .copy_from_slice(&(self.fixed_len as u32).to_le_bytes());
        if self.has_withdrawals {
            container[WITHDRAWALS_OFFSET..][..4]
                .copy_from_slice(&((self.fixed_len + transactions_len) as u32).to_le_bytes());
        }
        for _ in 0..transaction_count {
            data.extend_from_slice(&(transactions_len as u32).to_le_bytes());
        }
        data.resize(decoded_len, 0);
        snap::raw::Encoder::new().compress_vec(&data).unwrap()
    }

    /// Measures cumulative requested allocation, excluding fixture construction.
    pub fn measured_decode(
        self,
        frame: &[u8],
        max_allocation: usize,
    ) -> Result<NetworkPayloadEnvelope, PayloadEnvelopeError> {
        ALLOCATED.with(|c| c.set(0));
        let decoded = (self.decode)(frame);
        let allocated = ALLOCATED.with(|c| c.get());
        assert!(
            allocated < max_allocation,
            "{} allocated {allocated} bytes (budget {max_allocation})",
            self.name
        );
        decoded
    }
}

/// Oversized counts must fail without allocating the declared transaction objects.
#[test]
pub fn transaction_count_allocation_is_bounded_in_every_version() {
    for version in PayloadVersion::ALL {
        let maximum_wire_count =
            (MAX_DECOMPRESSED_ENVELOPE_BYTES - version.envelope_prefix - version.fixed_len)
                / ssz::BYTES_PER_LENGTH_OFFSET;
        for count in [MAX_TRANSACTIONS_PER_PAYLOAD + 1, maximum_wire_count] {
            let frame = version.frame(count, 0);
            let declared = snap::raw::decompress_len(&frame).unwrap();
            assert!(frame.len() < MAX_DECOMPRESSED_ENVELOPE_BYTES);
            // Decompression plus the V3/V4 hashing copy, with room for container bookkeeping.
            // No per-transaction allocation is permitted on this rejection path.
            let decoded = version.measured_decode(&frame, 2 * declared + 64 * 1024);
            assert_eq!(
                decoded,
                Err(PayloadEnvelopeError::BrokenSszEncoding),
                "{} count {count}",
                version.name
            );
        }
    }
}

/// Preserve the protocol maximum while making its remaining allocation budget explicit.
#[test]
pub fn permitted_transaction_counts_decode_in_every_version() {
    for version in PayloadVersion::ALL {
        for count in [0, MAX_TRANSACTIONS_PER_PAYLOAD - 1, MAX_TRANSACTIONS_PER_PAYLOAD] {
            let frame = version.frame(count, 0);
            let declared = snap::raw::decompress_len(&frame).unwrap();
            let budget = 2 * declared + count * std::mem::size_of::<Bytes>() + 64 * 1024;
            let decoded = version.measured_decode(&frame, budget).unwrap();
            assert_eq!(decoded.payload.transactions().len(), count, "{}", version.name);
            assert!(
                decoded.payload.transactions().iter().all(|transaction| transaction.is_empty())
            );
        }
    }
}

/// Base requires empty withdrawals from V2 onward, including before Isthmus.
#[test]
pub fn nonempty_withdrawals_are_rejected_without_list_allocation() {
    for version in PayloadVersion::ALL.into_iter().filter(|version| version.has_withdrawals) {
        let per_item = Withdrawal::ssz_fixed_len();
        let maximum_wire_len =
            (MAX_DECOMPRESSED_ENVELOPE_BYTES - version.envelope_prefix - version.fixed_len)
                / per_item
                * per_item;
        for len in [1, per_item - 1, per_item, maximum_wire_len] {
            let frame = version.frame(0, len);
            let declared = snap::raw::decompress_len(&frame).unwrap();
            let decoded = version.measured_decode(&frame, 2 * declared + 64 * 1024);
            assert_eq!(
                decoded,
                Err(PayloadEnvelopeError::BrokenSszEncoding),
                "{} withdrawals bytes {len}",
                version.name
            );
        }
    }
}

/// A count within the limit must not bypass SSZ offset validation.
#[test]
pub fn malformed_transaction_offsets_are_rejected_in_every_version() {
    for version in PayloadVersion::ALL {
        let frame = version.frame(3, 0);
        let mut data = snap::raw::Decoder::new().decompress_vec(&frame).unwrap();
        let last_offset =
            version.envelope_prefix + version.fixed_len + 2 * ssz::BYTES_PER_LENGTH_OFFSET;
        data[last_offset..][..4].copy_from_slice(&0u32.to_le_bytes());
        let frame = snap::raw::Encoder::new().compress_vec(&data).unwrap();
        assert_eq!(
            (version.decode)(&frame),
            Err(PayloadEnvelopeError::BrokenSszEncoding),
            "{}",
            version.name
        );
    }
}
