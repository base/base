/// Specification IDs and their activation points.
///
/// Information was obtained from the [Ethereum Execution Specifications](https://github.com/ethereum/execution-specs).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
#[allow(non_camel_case_types)]
pub enum SpecId {
    /// Frontier
    ///
    /// Activated at block 1
    FRONTIER = 0,
    /// Homestead
    ///
    /// Activated at block 1150000
    HOMESTEAD,
    /// Tangerine Whistle
    ///
    /// Activated at block 2463000
    TANGERINE,
    /// Spurious Dragon
    ///
    /// Activated at block 2675000
    SPURIOUS_DRAGON,
    /// Byzantium
    ///
    /// Activated at block 4370000
    BYZANTIUM,
    /// Petersburg
    ///
    /// Activated at block 7280000
    PETERSBURG,
    /// Istanbul
    ///
    /// Activated at block 9069000
    ISTANBUL,
    /// Berlin
    ///
    /// Activated at block 12244000
    BERLIN,
    /// London
    ///
    /// Activated at block 12965000
    LONDON,
    /// Paris/Merge
    ///
    /// Activated at block 15537394
    MERGE,
    /// Shanghai
    ///
    /// Activated at block 17034870 (timestamp 1681338455)
    SHANGHAI,
    /// Cancun
    ///
    /// Activated at block 19426587 (timestamp 1710338135)
    CANCUN,
    /// Prague
    ///
    /// Activated at block 22431084
    PRAGUE,
    /// Osaka
    ///
    /// Activated at block 23935694
    #[default]
    OSAKA,
    /// Amsterdam
    ///
    /// Activated at block TBD
    AMSTERDAM,
}

impl SpecId {
    /// Smallest specification ID.
    pub const MIN: Self = Self::FRONTIER;

    /// Default specification ID.
    pub const DEFAULT: Self = Self::OSAKA;

    /// Latest known specification ID.
    #[doc(alias = "MAX")]
    pub const NEXT: Self = Self::AMSTERDAM;

    /// Number of SpecId variants.
    pub const COUNT: usize = Self::NEXT as usize - Self::MIN as usize + 1;

    /// Returns the specification ID for a raw value.
    #[inline]
    pub const fn try_from_u32(spec_id: u32) -> Option<Self> {
        if spec_id <= Self::NEXT as u32 {
            // SAFETY: `spec_id` is within the valid variant range.
            return Some(unsafe { core::mem::transmute::<u32, Self>(spec_id) });
        }
        None
    }

    /// Returns the previous specification ID.
    #[inline]
    pub const fn prev(self) -> Option<Self> {
        Self::try_from_u32((self as u32).wrapping_sub(1))
    }

    /// Returns the next specification ID.
    #[inline]
    pub const fn next(self) -> Option<Self> {
        Self::try_from_u32((self as u32).wrapping_add(1))
    }

    /// Returns `true` if this specification enables `other`.
    #[inline]
    pub const fn enables(self, other: Self) -> bool {
        self as u32 >= other as u32
    }

    /// Returns `true` if `other` is enabled in this specification.
    #[deprecated(note = "use SpecId::enables instead")]
    #[inline]
    pub const fn is_enabled_in(self, other: Self) -> bool {
        self.enables(other)
    }
}

impl From<SpecId> for u32 {
    #[inline]
    fn from(spec_id: SpecId) -> Self {
        spec_id as Self
    }
}

impl TryFrom<u32> for SpecId {
    type Error = u32;

    #[inline]
    fn try_from(value: u32) -> core::result::Result<Self, Self::Error> {
        Self::try_from_u32(value).ok_or(value)
    }
}

/// Maps a base specification ID to its compile-time `u32` discriminant.
///
/// Syntax: `spec_to_generic!(spec_id_value, |SPEC_ID| do_something::<SPEC_ID>())`
#[macro_export]
macro_rules! spec_to_generic {
    (@spec $spec_id:ident, |$spec_const:ident| $e:expr) => {{
        const $spec_const: u32 = $crate::SpecId::$spec_id as u32;
        $e
    }};
    ([@match $spec_id:expr, $spec_const:ident, $e:expr] $($spec:ident $name:ident,)*) => {{
        match $spec_id {
            $(
                $crate::SpecId::$spec => {
                    $crate::spec_to_generic!(@spec $spec, |$spec_const| $e)
                }
            )*
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }};
    ($spec_id:expr, |$spec_const:ident| $e:expr) => {{
        $crate::for_each_spec!([@match $spec_id, $spec_const, $e] $crate::spec_to_generic)
    }};
}

/// Calls a macro with all specification IDs in activation order.
#[macro_export]
macro_rules! for_each_spec {
    ([$($extra:tt)*] $($m:tt)+) => {
        $($m)+! {
            [$($extra)*]
            FRONTIER frontier,
            HOMESTEAD homestead,
            TANGERINE tangerine,
            SPURIOUS_DRAGON spurious_dragon,
            BYZANTIUM byzantium,
            PETERSBURG petersburg,
            ISTANBUL istanbul,
            BERLIN berlin,
            LONDON london,
            MERGE merge,
            SHANGHAI shanghai,
            CANCUN cancun,
            PRAGUE prague,
            OSAKA osaka,
            AMSTERDAM amsterdam,
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(SpecId::DEFAULT, SpecId::default());
    }
}
