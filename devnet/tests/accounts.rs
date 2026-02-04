//! Tests for mnemonic-based account derivation.

use alloy_primitives::address;
use devnet::config::{
    ANVIL_ACCOUNT_0, ANVIL_ACCOUNT_1, ANVIL_ACCOUNT_2, ANVIL_ACCOUNT_3, ANVIL_ACCOUNT_4,
    ANVIL_ACCOUNT_5, ANVIL_ACCOUNT_6, ANVIL_ACCOUNT_7, ANVIL_ACCOUNT_8, ANVIL_ACCOUNT_9,
};

#[test]
fn test_mnemonic_derivation() {
    let expected_addresses = [
        address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
        address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
        address!("90F79bf6EB2c4f870365E785982E1f101E93b906"),
        address!("15d34AAf54267DB7D7c367839AAf71A00a2C6A65"),
        address!("9965507D1a55bcC2695C58ba16FB37d819B0A4dc"),
        address!("976EA74026E726554dB657fA54763abd0C3a0aa9"),
        address!("14dC79964da2C08b23698B3D3cc7Ca32193d9955"),
        address!("23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"),
        address!("a0Ee7A142d267C1f36714E4a8F75612F20a79720"),
    ];

    let derived_addresses = [
        ANVIL_ACCOUNT_0.address,
        ANVIL_ACCOUNT_1.address,
        ANVIL_ACCOUNT_2.address,
        ANVIL_ACCOUNT_3.address,
        ANVIL_ACCOUNT_4.address,
        ANVIL_ACCOUNT_5.address,
        ANVIL_ACCOUNT_6.address,
        ANVIL_ACCOUNT_7.address,
        ANVIL_ACCOUNT_8.address,
        ANVIL_ACCOUNT_9.address,
    ];

    for (i, (expected, derived)) in
        expected_addresses.iter().zip(derived_addresses.iter()).enumerate()
    {
        assert_eq!(expected, derived, "account {i} address mismatch");
    }
}
