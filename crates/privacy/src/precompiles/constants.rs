//! Precompile Constants
//!
//! Addresses and function selectors for the privacy precompiles.

use alloy_primitives::Address;

/// Address of the Registry precompile (0x0200)
///
/// This address is chosen to be distinct from standard precompiles (0x01-0x0A)
/// and Optimism-specific precompiles.
pub const REGISTRY_PRECOMPILE_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x00,
]);

/// Address of the Auth precompile (0x0201)
pub const AUTH_PRECOMPILE_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x01,
]);

// ============================================================================
// Registry Precompile Function Selectors
// ============================================================================

/// Selector for `register(address admin, tuple[] slots, bool hideEvents)`
/// keccak256("register(address,(uint256,uint8,uint8)[],bool)")[:4]
/// Verified against Solidity interface compilation
pub(super) const REGISTER_SELECTOR: [u8; 4] = [0x4b, 0xb0, 0x56, 0xb3];

/// Selector for `addSlots(tuple[] slots)`
/// keccak256("addSlots((uint256,uint8,uint8)[])")[:4]
/// Verified against Solidity interface compilation
pub(super) const ADD_SLOTS_SELECTOR: [u8; 4] = [0x50, 0xad, 0x0d, 0x4c];

/// Selector for `setHideEvents(bool hide)`
/// keccak256("setHideEvents(bool)")[:4]
pub(super) const SET_HIDE_EVENTS_SELECTOR: [u8; 4] = [0x77, 0x4a, 0x2d, 0xd8];

/// Selector for `isRegistered(address contract)`
/// keccak256("isRegistered(address)")[:4]
pub(super) const IS_REGISTERED_SELECTOR: [u8; 4] = [0xc3, 0xc5, 0xa5, 0x47];

// ============================================================================
// Auth Precompile Function Selectors
// ============================================================================

/// Selector for `grant(address contract, uint256 slot, address grantee, uint8 permission)`
/// keccak256("grant(address,uint256,address,uint8)")[:4]
/// Verified against Solidity interface compilation
pub(super) const GRANT_SELECTOR: [u8; 4] = [0x75, 0x8e, 0x42, 0xeb];

/// Selector for `revoke(address contract, uint256 slot, address grantee)`
/// keccak256("revoke(address,uint256,address)")[:4]
/// Verified against Solidity interface compilation
pub(super) const REVOKE_SELECTOR: [u8; 4] = [0x92, 0xf5, 0xf3, 0x4e];

/// Selector for `isAuthorized(address contract, uint256 slot, address query)`
/// keccak256("isAuthorized(address,uint256,address)")[:4]
/// Verified against Solidity interface compilation
pub(super) const IS_AUTHORIZED_SELECTOR: [u8; 4] = [0xe8, 0x77, 0x52, 0xe9];

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    // Verify the addresses are what we expect
    #[test]
    fn test_registry_address() {
        assert_eq!(
            REGISTRY_PRECOMPILE_ADDRESS,
            Address::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x00])
        );
    }

    #[test]
    fn test_auth_address() {
        assert_eq!(
            AUTH_PRECOMPILE_ADDRESS,
            Address::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x01])
        );
    }

    // Verify selectors match the function signatures
    // Note: We use simplified signatures without tuple expansion for selector calculation
    // In practice, you'd want to verify these against actual Solidity compilation

    #[test]
    fn test_selector_uniqueness() {
        // All selectors should be unique
        let selectors = [
            REGISTER_SELECTOR,
            ADD_SLOTS_SELECTOR,
            SET_HIDE_EVENTS_SELECTOR,
            IS_REGISTERED_SELECTOR,
            GRANT_SELECTOR,
            REVOKE_SELECTOR,
            IS_AUTHORIZED_SELECTOR,
        ];

        for i in 0..selectors.len() {
            for j in (i + 1)..selectors.len() {
                assert_ne!(selectors[i], selectors[j], "Selectors at {} and {} are equal", i, j);
            }
        }
    }

    #[test]
    fn test_is_registered_selector() {
        // This one has a simple signature we can verify
        let hash = keccak256(b"isRegistered(address)");
        assert_eq!(&hash[..4], &IS_REGISTERED_SELECTOR);
    }

    #[test]
    fn test_set_hide_events_selector() {
        let hash = keccak256(b"setHideEvents(bool)");
        assert_eq!(&hash[..4], &SET_HIDE_EVENTS_SELECTOR);
    }

    // Verify selectors match Solidity interface compilation
    // These use tuple types, so we verify the canonical Solidity encoding

    #[test]
    fn test_register_selector() {
        // register(address,(uint256,uint8,uint8)[],bool)
        let hash = keccak256(b"register(address,(uint256,uint8,uint8)[],bool)");
        assert_eq!(&hash[..4], &REGISTER_SELECTOR);
    }

    #[test]
    fn test_add_slots_selector() {
        // addSlots((uint256,uint8,uint8)[])
        let hash = keccak256(b"addSlots((uint256,uint8,uint8)[])");
        assert_eq!(&hash[..4], &ADD_SLOTS_SELECTOR);
    }

    #[test]
    fn test_grant_selector() {
        let hash = keccak256(b"grant(address,uint256,address,uint8)");
        assert_eq!(&hash[..4], &GRANT_SELECTOR);
    }

    #[test]
    fn test_revoke_selector() {
        let hash = keccak256(b"revoke(address,uint256,address)");
        assert_eq!(&hash[..4], &REVOKE_SELECTOR);
    }

    #[test]
    fn test_is_authorized_selector() {
        let hash = keccak256(b"isAuthorized(address,uint256,address)");
        assert_eq!(&hash[..4], &IS_AUTHORIZED_SELECTOR);
    }
}
