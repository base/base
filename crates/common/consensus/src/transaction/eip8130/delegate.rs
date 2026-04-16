//! Shared parsing helpers for canonical delegate auth envelopes.
//!
//! Canonical encoding:
//! `DELEGATE(20) || delegate_account(20) || nested_auth(verifier(20) || data...)`
//!
//! Some call paths already strip the outer `DELEGATE(20)` prefix and work with:
//! `delegate_account(20) || nested_auth(verifier(20) || data...)`.

use alloy_primitives::Address;

use super::{DELEGATE_VERIFIER_ADDRESS, K1_VERIFIER_ADDRESS};

/// Parsed view of a canonical delegate auth payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDelegateAuth<'a> {
    /// Delegate account controlled by the outer delegate verifier.
    pub delegate_account: Address,
    /// Verifier from nested auth (`Address::ZERO` for implicit EOA/K1).
    pub nested_verifier: Address,
    /// Raw nested auth (`verifier(20) || data...`).
    pub nested_auth: &'a [u8],
    /// Nested verifier-specific data (`nested_auth[20..]`).
    pub nested_data: &'a [u8],
}

/// Parses canonical delegate auth with outer verifier prefix.
pub fn parse_delegate_auth(auth: &[u8]) -> Option<ParsedDelegateAuth<'_>> {
    if auth.len() < 60 || Address::from_slice(&auth[..20]) != DELEGATE_VERIFIER_ADDRESS {
        return None;
    }
    parse_delegate_data(&auth[20..])
}

/// Parses delegate data after the outer verifier has been stripped.
///
/// Expected format:
/// `delegate_account(20) || nested_auth(verifier(20) || data...)`.
pub fn parse_delegate_data(delegate_data: &[u8]) -> Option<ParsedDelegateAuth<'_>> {
    if delegate_data.len() < 40 {
        return None;
    }

    let delegate_account = Address::from_slice(&delegate_data[..20]);
    let nested_auth = &delegate_data[20..];
    let nested_verifier = Address::from_slice(&nested_auth[..20]);
    let nested_data = &nested_auth[20..];

    Some(ParsedDelegateAuth { delegate_account, nested_verifier, nested_auth, nested_data })
}

/// Extracts the nested verifier from canonical delegate auth.
///
/// `Address::ZERO` nested verifier is normalized to `K1_VERIFIER_ADDRESS`.
pub fn delegate_inner_verifier(auth: &[u8]) -> Option<Address> {
    parse_delegate_auth(auth).map(|parsed| {
        if parsed.nested_verifier == Address::ZERO {
            K1_VERIFIER_ADDRESS
        } else {
            parsed.nested_verifier
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_delegate_auth_roundtrip() {
        let delegate = Address::repeat_byte(0x11);
        let nested_verifier = Address::repeat_byte(0x22);
        let nested_data = [0xaa, 0xbb, 0xcc];
        let mut auth = Vec::new();
        auth.extend_from_slice(DELEGATE_VERIFIER_ADDRESS.as_slice());
        auth.extend_from_slice(delegate.as_slice());
        auth.extend_from_slice(nested_verifier.as_slice());
        auth.extend_from_slice(&nested_data);

        let parsed = parse_delegate_auth(&auth).expect("delegate auth should parse");
        assert_eq!(parsed.delegate_account, delegate);
        assert_eq!(parsed.nested_verifier, nested_verifier);
        assert_eq!(parsed.nested_auth, &auth[40..]);
        assert_eq!(parsed.nested_data, nested_data);
    }

    #[test]
    fn parse_delegate_data_roundtrip() {
        let delegate = Address::repeat_byte(0x11);
        let nested_verifier = Address::repeat_byte(0x22);
        let nested_data = [0xaa, 0xbb, 0xcc];
        let mut delegate_data = Vec::new();
        delegate_data.extend_from_slice(delegate.as_slice());
        delegate_data.extend_from_slice(nested_verifier.as_slice());
        delegate_data.extend_from_slice(&nested_data);

        let parsed = parse_delegate_data(&delegate_data).expect("delegate data should parse");
        assert_eq!(parsed.delegate_account, delegate);
        assert_eq!(parsed.nested_verifier, nested_verifier);
        assert_eq!(parsed.nested_auth, &delegate_data[20..]);
        assert_eq!(parsed.nested_data, nested_data);
    }

    #[test]
    fn parse_delegate_auth_rejects_non_delegate_prefix() {
        let auth = vec![0u8; 60];
        assert!(parse_delegate_auth(&auth).is_none());
    }

    #[test]
    fn delegate_inner_verifier_normalizes_zero_to_k1() {
        let delegate = Address::repeat_byte(0x11);
        let mut auth = Vec::new();
        auth.extend_from_slice(DELEGATE_VERIFIER_ADDRESS.as_slice());
        auth.extend_from_slice(delegate.as_slice());
        auth.extend_from_slice(Address::ZERO.as_slice());

        assert_eq!(delegate_inner_verifier(&auth), Some(K1_VERIFIER_ADDRESS));
    }
}
