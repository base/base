use context_deserialize::ContextDeserialize;
use serde::{
    Deserialize,
    de::{Deserializer, Error},
};
use typenum::Unsigned;

use crate::{Bitfield, Fixed, Variable};

impl<'de, C, N> ContextDeserialize<'de, C> for Bitfield<Variable<N>>
where
    N: Unsigned + Clone,
{
    fn context_deserialize<D>(deserializer: D, _context: C) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Bitfield::<Variable<N>>::deserialize(deserializer)
            .map_err(|e| D::Error::custom(format!("{:?}", e)))
    }
}

impl<'de, C, N> ContextDeserialize<'de, C> for Bitfield<Fixed<N>>
where
    N: Unsigned + Clone,
{
    fn context_deserialize<D>(deserializer: D, _context: C) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Bitfield::<Fixed<N>>::deserialize(deserializer)
            .map_err(|e| D::Error::custom(format!("{:?}", e)))
    }
}
