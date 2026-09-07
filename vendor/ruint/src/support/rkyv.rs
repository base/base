//! Support for the [rkyv](https://github.com/rkyv/rkyv) crate.

#![cfg(feature = "rkyv")]
#![cfg_attr(docsrs, doc(cfg(feature = "rkyv")))]

use core::fmt;

use rkyv::{
    Archive, Archived, Deserialize, Place, Portable, Serialize,
    bytecheck::CheckBytes,
    primitive::ArchivedU64,
    rancor::{Fallible, Trace},
};

use crate::{Bits, Uint};

/// An archived [`Uint`]
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct ArchivedUint<const BITS: usize, const LIMBS: usize>([ArchivedU64; LIMBS]);

/// An archived [`Bits`]
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct ArchivedBits<const BITS: usize, const LIMBS: usize>(ArchivedUint<BITS, LIMBS>);

unsafe impl<const BITS: usize, const LIMBS: usize, C: Fallible + ?Sized> CheckBytes<C>
    for ArchivedUint<BITS, LIMBS>
where
    <C as Fallible>::Error: Trace,
{
    unsafe fn check_bytes(
        value: *const Self,
        context: &mut C,
    ) -> Result<(), <C as Fallible>::Error> {
        unsafe {
            <[ArchivedU64; LIMBS]>::check_bytes(value.cast(), context)?;
        }
        Ok(())
    }
}

impl<const BITS: usize, const LIMBS: usize> Archive for Uint<BITS, LIMBS> {
    type Archived = ArchivedUint<BITS, LIMBS>;
    type Resolver = [(); LIMBS];

    fn resolve(&self, resolver: Self::Resolver, out: Place<Self::Archived>) {
        self.limbs.resolve(resolver, unsafe { out.cast_unchecked() });
    }
}

unsafe impl<const BITS: usize, const LIMBS: usize> Portable for ArchivedUint<BITS, LIMBS> {}

impl<S: Fallible + ?Sized, const BITS: usize, const LIMBS: usize> Serialize<S>
    for Uint<BITS, LIMBS>
{
    fn serialize(&self, serializer: &mut S) -> Result<[(); LIMBS], <S as Fallible>::Error> {
        self.limbs.serialize(serializer)
    }
}

impl<D: Fallible + ?Sized, const BITS: usize, const LIMBS: usize> Deserialize<Uint<BITS, LIMBS>, D>
    for Archived<Uint<BITS, LIMBS>>
{
    fn deserialize(
        &self,
        deserializer: &mut D,
    ) -> Result<Uint<BITS, LIMBS>, <D as Fallible>::Error> {
        Ok(Uint { limbs: <[ArchivedU64; LIMBS]>::deserialize(&self.0, deserializer)? })
    }
}

unsafe impl<const BITS: usize, const LIMBS: usize, C: Fallible + ?Sized> CheckBytes<C>
    for ArchivedBits<BITS, LIMBS>
where
    <C as Fallible>::Error: Trace,
{
    unsafe fn check_bytes(
        value: *const Self,
        context: &mut C,
    ) -> Result<(), <C as Fallible>::Error> {
        unsafe {
            <ArchivedUint<BITS, LIMBS>>::check_bytes(value.cast(), context)?;
        }
        Ok(())
    }
}

impl<const BITS: usize, const LIMBS: usize> Archive for Bits<BITS, LIMBS> {
    type Archived = ArchivedBits<BITS, LIMBS>;
    type Resolver = [(); LIMBS];

    fn resolve(&self, resolver: Self::Resolver, out: Place<Self::Archived>) {
        self.as_uint().resolve(resolver, unsafe { out.cast_unchecked() });
    }
}

unsafe impl<const BITS: usize, const LIMBS: usize> Portable for ArchivedBits<BITS, LIMBS> {}

impl<S: Fallible + ?Sized, const BITS: usize, const LIMBS: usize> Serialize<S>
    for Bits<BITS, LIMBS>
{
    fn serialize(&self, serializer: &mut S) -> Result<[(); LIMBS], <S as Fallible>::Error> {
        self.as_uint().serialize(serializer)
    }
}

impl<D: Fallible + ?Sized, const BITS: usize, const LIMBS: usize> Deserialize<Bits<BITS, LIMBS>, D>
    for Archived<Bits<BITS, LIMBS>>
{
    fn deserialize(
        &self,
        deserializer: &mut D,
    ) -> Result<Bits<BITS, LIMBS>, <D as Fallible>::Error> {
        Ok(Bits::from(Deserialize::<Uint<BITS, LIMBS>, D>::deserialize(&self.0, deserializer)?))
    }
}

impl<'a, const BITS: usize, const LIMBS: usize> From<&'a ArchivedUint<BITS, LIMBS>>
    for Uint<BITS, LIMBS>
{
    fn from(archived: &'a ArchivedUint<BITS, LIMBS>) -> Self {
        Self { limbs: archived.0.map(ArchivedU64::to_native) }
    }
}

impl<'a, const BITS: usize, const LIMBS: usize> From<&'a ArchivedBits<BITS, LIMBS>>
    for Bits<BITS, LIMBS>
{
    fn from(archived: &'a ArchivedBits<BITS, LIMBS>) -> Self {
        Self::from(Into::<Uint<BITS, LIMBS>>::into(archived.0))
    }
}

impl<const BITS: usize, const LIMBS: usize> From<ArchivedUint<BITS, LIMBS>> for Uint<BITS, LIMBS> {
    fn from(archived: ArchivedUint<BITS, LIMBS>) -> Self {
        (&archived).into()
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::Display for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::Debug for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::Binary for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Binary::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::Octal for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Octal::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::LowerHex for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

impl<const BITS: usize, const LIMBS: usize> fmt::UpperHex for ArchivedUint<BITS, LIMBS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::UpperHex::fmt(&Into::<Uint<BITS, LIMBS>>::into(self), f)
    }
}

#[cfg(test)]
mod tests {
    use proptest::proptest;
    use rkyv::rancor;

    use super::*;
    use crate::{Uint, const_for, nlimbs};

    #[test]
    fn test_rkyv() {
        const_for!(BITS in NON_ZERO {
            const LIMBS: usize = nlimbs(BITS);
            proptest!(|(n: Uint<BITS, LIMBS>)| {
                let s = rkyv::to_bytes::<rancor::Error>(&n).unwrap();
                let a = rkyv::access::<ArchivedUint<BITS, LIMBS>, rancor::Error>(&s).unwrap();
                assert_eq!(n, Into::<Uint<BITS, LIMBS>>::into(a));
                let d = rkyv::deserialize::<_, rancor::Error>(a).unwrap();
                assert_eq!(n, d);

                let b = Bits::from(n);
                let s = rkyv::to_bytes::<rancor::Error>(&b).unwrap();
                let a = rkyv::access::<ArchivedBits<BITS, LIMBS>, rancor::Error>(&s).unwrap();
                assert_eq!(b, a.into());
                let d = rkyv::deserialize::<_, rancor::Error>(a).unwrap();
                assert_eq!(b, d);
            });
        });
    }
}
