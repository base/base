//! Pre-authentication allocation-amplification regression test for
//! [`NetworkPayloadEnvelope::decode_v4`].
//!
//! This lives in its own integration test binary rather than a unit test so the
//! `#[global_allocator]` below is isolated to a single binary: a crate-wide
//! allocator declared inside the crate's `#[cfg(test)] mod tests` would apply to
//! every test in that binary (and would collide with any other test that
//! declared its own allocator). Here it governs only this one measurement.

#![cfg(feature = "std")]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use base_common_rpc_types_engine::{MAX_DECOMPRESSED_ENVELOPE_BYTES, NetworkPayloadEnvelope};

// Running total of bytes allocated on the current thread. Const-initialized so
// reading it never allocates (which would recurse through the allocator).
thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// Global allocator that tallies allocation volume per thread, delegating to the
/// system allocator. Lets the test measure how much heap a single decode forces.
/// Only allocation (growth) is counted, which is what a resource-exhaustion
/// bound cares about.
struct CountingAllocator;

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

/// Regression test for the pre-authentication allocation-amplification
/// denial-of-service (CWE-770). `decode_v4` is the gossip block-decode entry
/// point that `BlockHandler::handle` runs for every message arriving on the
/// public P2P port, before any signature check.
///
/// A frame only ~0.5 `MiB` on the wire can declare a `transactions` list of
/// ~2.6M entries. An unbounded decode honors that count and allocates ~125 `MiB`
/// of heap per frame; a length-bounded list must instead keep the per-frame heap
/// within a small multiple of the decompressed size. This measures the actual
/// bytes the decode allocates and holds it to that bound.
#[test]
fn decode_v4_transaction_bomb_allocation_is_bounded() {
    // SSZ container fixed-region byte offsets (see the `from_ssz_bytes`
    // regression test in the payload module for the full field layout).
    const FIXED_LEN: usize = 560;
    const EXTRA_DATA_OFFSET: usize = 436;
    const TRANSACTIONS_OFFSET: usize = 504;
    const WITHDRAWALS_OFFSET: usize = 508;
    // Each envelope carries a 65-byte signature and 32-byte parent beacon root
    // ahead of the SSZ container; the whole decompressed envelope must stay under
    // the decompression cap.
    const ENVELOPE_PREFIX: usize = 65 + 32;

    let txs_len = ((MAX_DECOMPRESSED_ENVELOPE_BYTES - ENVELOPE_PREFIX - FIXED_LEN)
        / ssz::BYTES_PER_LENGTH_OFFSET)
        * ssz::BYTES_PER_LENGTH_OFFSET;
    let declared_txs = txs_len / ssz::BYTES_PER_LENGTH_OFFSET;

    // Container: zeroed fixed fields, empty extra_data/withdrawals, and a
    // transactions region whose leading offset declares `declared_txs` empty
    // items (every offset points past the end of the list).
    let mut container = Vec::with_capacity(FIXED_LEN + txs_len);
    container.resize(FIXED_LEN, 0);
    container[EXTRA_DATA_OFFSET..][..4].copy_from_slice(&(FIXED_LEN as u32).to_le_bytes());
    container[TRANSACTIONS_OFFSET..][..4].copy_from_slice(&(FIXED_LEN as u32).to_le_bytes());
    container[WITHDRAWALS_OFFSET..][..4]
        .copy_from_slice(&((FIXED_LEN + txs_len) as u32).to_le_bytes());
    for _ in 0..declared_txs {
        container.extend_from_slice(&(txs_len as u32).to_le_bytes());
    }

    // Envelope = signature (r=1, s=1, v=0 so it parses and the decode reaches the
    // SSZ sink) + parent beacon root + container, snappy-compressed as it arrives
    // on the wire.
    let mut decompressed = Vec::with_capacity(ENVELOPE_PREFIX + container.len());
    let mut signature = [0u8; 65];
    signature[31] = 1;
    signature[63] = 1;
    decompressed.extend_from_slice(&signature);
    decompressed.extend_from_slice(&[0u8; 32]);
    decompressed.extend_from_slice(&container);
    let frame = snap::raw::Encoder::new().compress_vec(&decompressed).unwrap();

    // Measure only the heap the decode itself grows on this thread.
    ALLOCATED.with(|c| c.set(0));
    let _ = NetworkPayloadEnvelope::decode_v4(&frame);
    let allocated = ALLOCATED.with(|c| c.get());

    // A legitimate decode allocates the decompressed buffer plus the hashing
    // buffer (~2x the frame); anything approaching the bomb's ~125 MiB means the
    // transaction list honored the attacker's element count.
    let max_allocation = 4 * MAX_DECOMPRESSED_ENVELOPE_BYTES;
    assert!(
        allocated < max_allocation,
        "decoding one frame declaring {declared_txs} transactions allocated {allocated} bytes \
         pre-authentication (limit {max_allocation}); the transaction list is not length-bounded",
    );
}
