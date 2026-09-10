//! The block hash type stored by `shadow_blocks`.

use std::{fmt, str::FromStr};

use alloy_primitives::{B256, hex};
use sqlx::{
    Postgres,
    encode::IsNull,
    error::BoxDynError,
    postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueRef},
};

/// A block hash, held as bytes and stored as `0x`-prefixed lowercase hex text.
///
/// The database side of this is TEXT and must stay TEXT: the Snowflake ETL reads the table with
/// psycopg, which renders a BYTEA column as a `memoryview` that nothing in the pipeline unwraps,
/// so the warehouse records the repr of a Python object instead of a hash.
///
/// Keeping the Rust side a [`B256`] behind a newtype is what makes that hard to undo by accident.
/// `alloy-primitives` ships its own sqlx support, but it maps `FixedBytes` through `Vec<u8>` onto
/// BYTEA; Cargo features are additive, so the day any crate in the workspace enables
/// `alloy-primitives/sqlx`, a bare `B256` bound into a query starts silently writing bytes. This
/// type never hands a bare `B256` to sqlx, and its [`sqlx::Type`] impl names TEXT outright.
///
/// The spelling matters beyond the column type. Lookups are string equality, so the writer and
/// the reader have to agree on one rendering of a hash or a query misses instead of failing. That
/// rendering is `0x`-prefixed lowercase hex, produced here and nowhere else, which is also what
/// the shadow-metrics JSON API emits and how every other EVM hash in Snowflake is written.
/// The field is private on purpose. Exposing it would let a caller reach past the newtype and
/// bind the inner `B256` to a query directly, which is the BYTEA hazard this type exists to make
/// impossible rather than merely discouraged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShadowHash(B256);

impl ShadowHash {
    /// Wraps raw hash bytes.
    #[must_use]
    pub const fn new(hash: B256) -> Self {
        Self(hash)
    }
}

/// Renders the stored spelling. Parsing tolerates case, this never varies.
///
/// Delegates rather than calling `hex::encode_prefixed`, which would allocate a `String` only to
/// copy it into the formatter; `B256` writes its hex digits straight out.
impl fmt::Display for ShadowHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for ShadowHash {
    type Err = hex::FromHexError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<B256>().map(Self)
    }
}

impl From<B256> for ShadowHash {
    fn from(hash: B256) -> Self {
        Self(hash)
    }
}

impl sqlx::Type<Postgres> for ShadowHash {
    fn type_info() -> PgTypeInfo {
        <String as sqlx::Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as sqlx::Type<Postgres>>::compatible(ty)
    }
}

/// Binds arrays, which `resolve_canonical_hashes` needs for its `UNNEST`.
impl PgHasArrayType for ShadowHash {
    fn array_type_info() -> PgTypeInfo {
        <String as PgHasArrayType>::array_type_info()
    }
}

impl sqlx::Encode<'_, Postgres> for ShadowHash {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as sqlx::Encode<'_, Postgres>>::encode_by_ref(&self.to_string(), buf)
    }
}

impl<'r> sqlx::Decode<'r, Postgres> for ShadowHash {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let stored = <&str as sqlx::Decode<'r, Postgres>>::decode(value)?;
        stored.parse::<Self>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use sqlx::{Postgres, Type};

    use super::ShadowHash;

    #[test]
    fn renders_lowercase_with_an_0x_prefix() {
        assert_eq!(
            ShadowHash::new(B256::repeat_byte(0xab)).to_string(),
            format!("0x{}", "ab".repeat(32))
        );
    }

    #[test]
    fn parsing_normalizes_a_hash_onto_the_stored_spelling() {
        let upper: ShadowHash =
            format!("0X{}", "AB".repeat(32)).parse().expect("uppercase input is still a hash");
        assert_eq!(upper.to_string(), format!("0x{}", "ab".repeat(32)));
        assert_eq!(upper, ShadowHash::new(B256::repeat_byte(0xab)));
    }

    #[test]
    fn rejects_anything_that_is_not_a_32_byte_hash() {
        assert!("0xabcd".parse::<ShadowHash>().is_err(), "a short value is not a block hash");
        assert!("nothex".parse::<ShadowHash>().is_err());
    }

    #[test]
    fn binds_as_text_so_the_etl_never_sees_bytes() {
        assert_eq!(
            <ShadowHash as Type<Postgres>>::type_info(),
            <String as Type<Postgres>>::type_info(),
            "the column must stay TEXT; BYTEA reaches Snowflake as a memoryview repr"
        );
        assert!(
            !<ShadowHash as Type<Postgres>>::compatible(&<Vec<u8> as Type<Postgres>>::type_info()),
            "a BYTEA column must not decode into a ShadowHash"
        );
    }
}
