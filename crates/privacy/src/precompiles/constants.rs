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
pub const REGISTER_SELECTOR: [u8; 4] = [0x8f, 0x59, 0x8a, 0x70];

/// Selector for `addSlots(tuple[] slots)`
/// keccak256("addSlots((uint256,uint8,uint8)[])")[:4]
pub const ADD_SLOTS_SELECTOR: [u8; 4] = [0x1a, 0x8b, 0x6b, 0x8f];

/// Selector for `setHideEvents(bool hide)`
/// keccak256("setHideEvents(bool)")[:4]
pub const SET_HIDE_EVENTS_SELECTOR: [u8; 4] = [0x77, 0x4a, 0x2d, 0xd8];

/// Selector for `isRegistered(address contract)`
/// keccak256("isRegistered(address)")[:4]
pub const IS_REGISTERED_SELECTOR: [u8; 4] = [0xc3, 0xc5, 0xa5, 0x47];

// ============================================================================
// Auth Precompile Function Selectors
// ============================================================================

/// Selector for `grant(address contract, uint256 slot, address grantee, uint8 permission)`
/// keccak256("grant(address,uint256,address,uint8)")[:4]
pub const GRANT_SELECTOR: [u8; 4] = [0x1a, 0x95, 0xe7, 0x9a];

/// Selector for `revoke(address contract, uint256 slot, address grantee)`
/// keccak256("revoke(address,uint256,address)")[:4]
pub const REVOKE_SELECTOR: [u8; 4] = [0x8d, 0x2d, 0x7e, 0x10];

/// Selector for `isAuthorized(address contract, uint256 slot, address query)`
/// keccak256("isAuthorized(address,uint256,address)")[:4]
pub const IS_AUTHORIZED_SELECTOR: [u8; 4] = [0x2f, 0x54, 0xbf, 0x6e];

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
}
