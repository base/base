/// Macro that defines different variants of a chain specific enum. See [`crate::Hardfork`] as an
/// example.
#[macro_export]
macro_rules! hardfork {
    ($(#[$enum_meta:meta])* $enum:ident { $( $(#[$meta:meta])* $variant:ident ),* $(,)? }) => {
        $(#[$enum_meta])*
        #[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
        pub enum $enum {
            $( $(#[$meta])* $variant ),*
        }

        impl $enum {

            /// All hardfork variants
            pub const VARIANTS: &'static [Self] =  &[$(Self::$variant ),*];

            /// Returns variant as `str`.
            pub const fn name(&self) -> &'static str {
                match self {
                    $( $enum::$variant => stringify!($variant), )*
                }
            }
        }

        impl core::str::FromStr for $enum {
            type Err = $crate::error::ParseHardforkError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s.to_lowercase().as_str() {
                    $(
                        s if s == stringify!($variant).to_lowercase() => Ok($enum::$variant),
                    )*
                      _ => Err($crate::error::ParseHardforkError::new(
                $crate::__private::format!("Unknown hardfork: {s}")
            )),
                }
            }
        }

        impl $crate::Hardfork for $enum {
            fn name(&self) -> &'static str {
                Self::name(self)
            }
        }

        impl core::fmt::Display for $enum {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{self:?}")
            }
        }
    }
}
