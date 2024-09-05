//! Utility methods used by protocol types.

/// Returns if the given `value` is a deposit transaction.
pub fn starts_with_2781_deposit<B>(value: &B) -> bool
where
    B: AsRef<[u8]>,
{
    value.as_ref().first() == Some(&0x7E)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn test_is_deposit() {
        assert!(starts_with_2781_deposit(&[0x7E]));
        assert!(!starts_with_2781_deposit(&[]));
        assert!(!starts_with_2781_deposit(&[0x7F]));
    }

    #[test]
    fn test_bytes_deposit() {
        assert!(starts_with_2781_deposit(&Bytes::from_static(&[0x7E])));
        assert!(!starts_with_2781_deposit(&Bytes::from_static(&[])));
        assert!(!starts_with_2781_deposit(&Bytes::from_static(&[0x7F])));
    }
}
