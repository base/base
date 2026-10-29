// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Minimal storage-write load generator for stressing state-trie proof
///         systems by forcing fresh (zero -> non-zero) SSTOREs on every call.
/// @dev For load testing only. The slot key mixes `block.number` and
///      `block.prevrandao` in addition to `seed` and `msg.sender`, so keys are
///      disjoint across blocks and concurrent senders even when the load
///      generator replays an identical `seed` sequence across runs. This keeps
///      every write in the cold zero -> non-zero case and grows state
///      unboundedly by design.
contract StorageFiller {
    /// @dev Backing store; each written key occupies one storage slot.
    mapping(uint256 => uint256) private slots;

    /// @notice Number of slots written by `writeStorage`; keys `0..written` hold non-zero values.
    uint256 public written;

    /// @notice Writes `slotCount` fresh storage slots.
    /// @param slotCount Number of distinct storage slots to write this call.
    /// @param seed Caller-supplied entropy mixed into each slot key.
    function fillStorage(uint256 slotCount, uint256 seed) external {
        uint256 base =
            uint256(keccak256(abi.encodePacked(seed, msg.sender, block.number, block.prevrandao)));
        for (uint256 i = 0; i < slotCount; i++) {
            slots[base + i] = base + i + 1;
        }
    }

    /// @notice Writes `newCount` fresh slots, then overwrites `existingCount` slots picked at
    ///         random from those written by earlier `writeStorage` calls.
    /// @dev The first call on a fresh contract has no earlier slots, so it skips the overwrites.
    function writeStorage(uint256 newCount, uint256 existingCount, uint256 seed) external {
        uint256 start = written;
        uint256 end = start + newCount;
        for (uint256 i = start; i < end; i++) {
            slots[i] = i + 1;
        }
        written = end;
        if (start == 0) return;
        for (uint256 i = 0; i < existingCount; i++) {
            uint256 h = uint256(keccak256(abi.encode(seed, msg.sender, i)));
            slots[h % start] = h;
        }
    }
}
