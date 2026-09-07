use alloc::string::String;
use core::{cmp::Ordering, fmt, str::FromStr, time::Duration};

pub use crate::generated::named::NamedChain;

impl NamedChain {
    /// Returns an iterator over all named chains.
    #[inline]
    pub fn iter() -> NamedChainIter {
        NamedChainIter { inner: Self::VARIANTS.iter().copied() }
    }

    /// Returns the chain's average blocktime, if applicable.
    #[inline]
    pub const fn average_blocktime_hint(self) -> Option<Duration> {
        let millis = self.average_blocktime_millis();
        if millis == 0 { None } else { Some(Duration::from_millis(millis as u64)) }
    }

    /// Returns the chain's blockchain explorer API key from the environment.
    #[cfg(feature = "std")]
    pub fn etherscan_api_key(self) -> Option<String> {
        self.etherscan_api_key_name().and_then(|name| std::env::var(name).ok())
    }

    /// Returns the address of the public DNS node list for the given chain.
    pub fn public_dns_network_protocol(self) -> Option<String> {
        const DNS_PREFIX: &str = "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@";
        if matches!(
            self,
            Self::Mainnet
                | Self::Goerli
                | Self::Sepolia
                | Self::Ropsten
                | Self::Rinkeby
                | Self::Holesky
                | Self::Hoodi
        ) {
            let mut s = String::with_capacity(DNS_PREFIX.len() + 32);
            s.push_str(DNS_PREFIX);
            s.push_str("all.");
            let chain_str = self.as_ref();
            s.push_str(chain_str);
            let l = s.len();
            s[l - chain_str.len()..].make_ascii_lowercase();
            s.push_str(".ethdisco.net");
            Some(s)
        } else {
            None
        }
    }
}

/// Error returned when parsing a named chain from a string fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParseNamedChainError;

impl fmt::Display for ParseNamedChainError {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("matching variant not found")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseNamedChainError {}

/// Iterator over all named chains.
#[derive(Clone, Debug)]
pub struct NamedChainIter {
    inner: core::iter::Copied<core::slice::Iter<'static, NamedChain>>,
}

impl Default for NamedChainIter {
    #[inline]
    fn default() -> Self {
        NamedChain::iter()
    }
}

impl Iterator for NamedChainIter {
    type Item = NamedChain;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl DoubleEndedIterator for NamedChainIter {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.next_back()
    }
}

impl ExactSizeIterator for NamedChainIter {}
impl core::iter::FusedIterator for NamedChainIter {}

impl From<NamedChain> for &'static str {
    #[inline]
    fn from(chain: NamedChain) -> Self {
        (&chain).into()
    }
}

impl From<&NamedChain> for &'static str {
    #[inline]
    fn from(chain: &NamedChain) -> Self {
        chain.as_str()
    }
}

impl Default for NamedChain {
    #[inline]
    fn default() -> Self {
        Self::Mainnet
    }
}

macro_rules! impl_into_numeric {
    ($($t:ty)+) => {$(
        impl From<NamedChain> for $t {
            #[inline]
            fn from(chain: NamedChain) -> Self {
                chain as $t
            }
        }
    )+};
}

impl_into_numeric!(u64 i64 u128 i128);
#[cfg(target_pointer_width = "64")]
impl_into_numeric!(usize isize);

impl num_enum::TryFromPrimitive for NamedChain {
    type Primitive = u64;
    type Error = num_enum::TryFromPrimitiveError<Self>;

    const NAME: &'static str = "NamedChain";

    fn try_from_primitive(number: Self::Primitive) -> Result<Self, Self::Error> {
        Self::from_chain_id(number).ok_or_else(|| num_enum::TryFromPrimitiveError::new(number))
    }
}

impl TryFrom<u64> for NamedChain {
    type Error = num_enum::TryFromPrimitiveError<Self>;

    #[inline]
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        num_enum::TryFromPrimitive::try_from_primitive(value)
    }
}

macro_rules! impl_try_from_numeric {
    ($($native:ty)+) => {
        $(
            impl TryFrom<$native> for NamedChain {
                type Error = num_enum::TryFromPrimitiveError<NamedChain>;

                #[inline]
                fn try_from(value: $native) -> Result<Self, Self::Error> {
                    (value as u64).try_into()
                }
            }
        )+
    };
}

impl_try_from_numeric!(u8 i8 u16 i16 u32 i32 usize isize);

impl fmt::Display for NamedChain {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

impl AsRef<str> for NamedChain {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for NamedChain {
    type Err = ParseNamedChainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NamedChain::from_parse_str(s).ok_or(ParseNamedChainError)
    }
}

impl TryFrom<&str> for NamedChain {
    type Error = ParseNamedChainError;

    #[inline]
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl Ord for NamedChain {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        (*self as u64).cmp(&(*other as u64))
    }
}

impl PartialOrd for NamedChain {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq<u64> for NamedChain {
    #[inline]
    fn eq(&self, other: &u64) -> bool {
        (*self as u64) == *other
    }
}

impl PartialOrd<u64> for NamedChain {
    #[inline]
    fn partial_cmp(&self, other: &u64) -> Option<Ordering> {
        (*self as u64).partial_cmp(other)
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for NamedChain {
    #[inline]
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_ref())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for NamedChain {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NamedChainVisitor;

        impl serde::de::Visitor<'_> for NamedChainVisitor {
            type Value = NamedChain;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a named chain")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                NamedChain::from_serde_str(value).ok_or_else(|| {
                    serde::de::Error::unknown_variant(value, NamedChain::VARIANT_NAMES)
                })
            }
        }

        deserializer.deserialize_str(NamedChainVisitor)
    }
}

#[cfg(feature = "rlp")]
impl alloy_rlp::Encodable for NamedChain {
    #[inline]
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        (*self as u64).encode(out)
    }

    #[inline]
    fn length(&self) -> usize {
        (*self as u64).length()
    }
}

#[cfg(feature = "rlp")]
impl alloy_rlp::Decodable for NamedChain {
    #[inline]
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let n = u64::decode(buf)?;
        Self::try_from(n).map_err(|_| alloy_rlp::Error::Overflow)
    }
}

#[cfg(feature = "arbitrary")]
impl<'a> arbitrary::Arbitrary<'a> for NamedChain {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let idx = u.choose_index(Self::COUNT)?;
        Ok(Self::VARIANTS[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::named::PARSE_ALIASES;
    #[cfg(feature = "serde")]
    use crate::generated::named::SERDE_ALIASES;

    #[allow(unused_imports)]
    use alloc::string::ToString;

    #[test]
    #[cfg(feature = "serde")]
    fn default() {
        assert_eq!(serde_json::to_string(&NamedChain::default()).unwrap(), "\"mainnet\"");
    }

    #[test]
    fn enum_iter() {
        assert_eq!(NamedChain::COUNT, NamedChain::iter().size_hint().0);
        assert_eq!(NamedChain::COUNT, NamedChain::VARIANTS.len());
        assert_eq!(NamedChain::COUNT, NamedChain::VARIANT_NAMES.len());
    }

    #[test]
    fn roundtrip_string() {
        for chain in NamedChain::iter() {
            let chain_string = chain.to_string();
            assert_eq!(chain_string, format!("{chain}"));
            assert_eq!(chain_string.as_str(), chain.as_ref());
            #[cfg(feature = "serde")]
            assert_eq!(serde_json::to_string(&chain).unwrap(), format!("\"{chain_string}\""));

            assert_eq!(chain_string.parse::<NamedChain>().unwrap(), chain);
        }
    }

    #[test]
    #[cfg(feature = "serde")]
    fn roundtrip_serde() {
        for chain in NamedChain::iter() {
            let chain_string = serde_json::to_string(&chain).unwrap();
            let chain_string = chain_string.replace('-', "_");
            assert_eq!(serde_json::from_str::<'_, NamedChain>(&chain_string).unwrap(), chain);
        }
    }

    #[test]
    #[cfg(feature = "arbitrary")]
    fn test_arbitrary_named_chain() {
        use arbitrary::{Arbitrary, Unstructured};
        let data = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 255];
        let mut unstructured = Unstructured::new(&data);

        for _ in 0..10 {
            let _chain = NamedChain::arbitrary(&mut unstructured).unwrap();
        }
    }

    #[test]
    fn aliases() {
        for &(chain, alias) in PARSE_ALIASES {
            assert_eq!(alias.parse::<NamedChain>().unwrap(), chain);

            #[cfg(feature = "serde")]
            assert_eq!(serde_json::from_str::<NamedChain>(&format!("\"{alias}\"")).unwrap(), chain);
        }

        #[cfg(feature = "serde")]
        for &(chain, alias) in SERDE_ALIASES {
            assert_eq!(serde_json::from_str::<NamedChain>(&format!("\"{alias}\"")).unwrap(), chain);
        }
    }

    #[test]
    #[cfg(feature = "serde")]
    fn serde_to_string_match() {
        for chain in NamedChain::iter() {
            let chain_serde = serde_json::to_string(&chain).unwrap();
            let chain_string = format!("\"{chain}\"");
            assert_eq!(chain_serde, chain_string);
        }
    }

    #[test]
    fn test_dns_network() {
        let s = "enrtree://AKA3AM6LPBYEUDMVNU3BSVQJ5AD45Y7YPOHJLEF6W26QOE4VTUDPE@all.mainnet.ethdisco.net";
        assert_eq!(NamedChain::Mainnet.public_dns_network_protocol().unwrap(), s);
    }

    #[test]
    fn ensure_no_trailing_etherscan_url_separator() {
        for chain in NamedChain::iter() {
            if let Some((api, base)) = chain.etherscan_urls() {
                assert!(!api.ends_with('/'), "{chain:?} api url has trailing /");
                assert!(!base.ends_with('/'), "{chain:?} base url has trailing /");
            }
        }
    }
}
