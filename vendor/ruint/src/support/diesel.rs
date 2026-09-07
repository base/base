//! Support for the [`diesel`](https://crates.io/crates/diesel) crate.
//!
//! Currently only encodes to/from a big-endian byte array.

#![cfg(feature = "diesel")]
#![cfg_attr(docsrs, doc(cfg(feature = "diesel")))]

use std::io::Write;

use diesel::{
    Queryable,
    backend::Backend,
    deserialize::{FromSql, Result as DeserResult},
    expression::AsExpression,
    internal::derives::as_expression::Bound,
    query_builder::bind_collector::RawBytesBindCollector,
    serialize::{IsNull, Output, Result as SerResult, ToSql},
    sql_types::{Binary, Nullable, SingleValue},
};
use thiserror::Error;

use crate::Uint;

#[derive(Error, Debug)]
pub enum DecodeError {
    #[error("Value too large for target type")]
    Overflow,
}

impl<const BITS: usize, const LIMBS: usize, Db> ToSql<Binary, Db> for Uint<BITS, LIMBS>
where
    for<'c> Db: Backend<BindCollector<'c> = RawBytesBindCollector<Db>>,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Db>) -> SerResult {
        out.write_all(&self.to_be_bytes_vec())?;
        Ok(IsNull::No)
    }
}

impl<const BITS: usize, const LIMBS: usize, Db: Backend> FromSql<Binary, Db> for Uint<BITS, LIMBS>
where
    *const [u8]: FromSql<Binary, Db>,
{
    fn from_sql(bytes: Db::RawValue<'_>) -> DeserResult<Self> {
        let bytes: *const [u8] = FromSql::<Binary, Db>::from_sql(bytes)?;
        let bytes: &[u8] = unsafe { &*bytes };
        Self::try_from_be_slice(bytes).ok_or_else(|| DecodeError::Overflow.into())
    }
}

// NB: the following code is expanded derive macros. They were produced by
// expanding the the following code:
// ```
// #[derive(diesel::AsExpression, diesel::FromSqlRow)]
// #[diesel(sql_type = diesel::sql_types::Binary)]
// pub struct Uint<const BITS: usize, const LIMBS: usize> { .. }
// ```

impl<const BITS: usize, const LIMBS: usize> AsExpression<Binary> for &Uint<BITS, LIMBS> {
    type Expression = Bound<Binary, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize> AsExpression<Nullable<Binary>> for &Uint<BITS, LIMBS> {
    type Expression = Bound<Nullable<Binary>, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize> AsExpression<Binary> for &&Uint<BITS, LIMBS> {
    type Expression = Bound<Binary, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize> AsExpression<Nullable<Binary>> for &&Uint<BITS, LIMBS> {
    type Expression = Bound<Nullable<Binary>, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize, Db> ToSql<Nullable<Binary>, Db> for Uint<BITS, LIMBS>
where
    Db: Backend,
    Self: ToSql<Binary, Db>,
{
    fn to_sql<'a>(&'a self, out: &mut Output<'a, '_, Db>) -> SerResult {
        ToSql::<Binary, Db>::to_sql(self, out)
    }
}

impl<const BITS: usize, const LIMBS: usize> AsExpression<Binary> for Uint<BITS, LIMBS> {
    type Expression = Bound<Binary, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize> AsExpression<Nullable<Binary>> for Uint<BITS, LIMBS> {
    type Expression = Bound<Nullable<Binary>, Self>;
    fn as_expression(self) -> Self::Expression {
        Bound::new(self)
    }
}

impl<const BITS: usize, const LIMBS: usize, Db, St> Queryable<St, Db> for Uint<BITS, LIMBS>
where
    Db: Backend,
    St: SingleValue,
    Self: FromSql<St, Db>,
{
    type Row = Self;
    fn build(row: Self::Row) -> DeserResult<Self> {
        Ok(row)
    }
}
