use alloc::vec::Vec;
use core::fmt::{self, Display};

use alloy_primitives::{Address, map::AddressMap};
use derive_where::derive_where;

use crate::{
    BaseEvmTypes, Evm, EvmTypesHost,
    interpreter::{GasTracker, Message},
    precompiles::{
        PrecompileId, PrecompileResult, blake2, bls12_381, bn254, hash, identity,
        kzg_point_evaluation, modexp, secp256k1, secp256r1,
    },
};

/// Precompile implementation function.
pub type PrecompileFn<T = BaseEvmTypes> =
    fn(&mut Evm<'_, T>, &Message<T>, &mut GasTracker) -> PrecompileResult;

/// Precompile descriptor.
#[derive_where(Clone, Debug)]
pub struct Precompile<T: EvmTypesHost = BaseEvmTypes> {
    /// Precompile address.
    address: Address,
    /// Precompile data.
    data: PrecompileData<T>,
}

impl<T: EvmTypesHost> Precompile<T> {
    /// Creates a precompile descriptor.
    #[inline]
    pub const fn new(address: Address, id: PrecompileId, f: PrecompileFn<T>) -> Self {
        Self { address, data: PrecompileData::new(id, f) }
    }

    /// Returns the precompile address.
    #[inline]
    pub const fn address(&self) -> Address {
        self.address
    }

    /// Returns this precompile descriptor with a different address.
    #[inline]
    pub fn with_address(self, address: Address) -> Self {
        Self { address, data: self.data }
    }

    /// Returns this precompile descriptor with different data.
    #[inline]
    pub fn with_data(self, data: PrecompileData<T>) -> Self {
        Self { address: self.address, data }
    }

    /// Returns the precompile data.
    #[inline]
    pub const fn data(&self) -> &PrecompileData<T> {
        &self.data
    }

    /// Consumes the precompile descriptor and returns its data.
    #[inline]
    pub fn into_data(self) -> PrecompileData<T> {
        self.data
    }

    /// Returns the precompile ID.
    #[inline]
    pub const fn id(&self) -> &PrecompileId {
        self.data.id()
    }

    /// Returns the precompile implementation function.
    #[inline]
    pub const fn run(&self) -> PrecompileFn<T> {
        self.data.run()
    }
}

fn dummy_precompile<T: EvmTypesHost>(
    _evm: &mut Evm<'_, T>,
    _message: &Message<T>,
    _gas: &mut GasTracker,
) -> PrecompileResult {
    unreachable!("dummy precompile data must be replaced before use")
}

/// Address-free precompile data.
#[derive_where(Clone, Debug)]
pub struct PrecompileData<T: EvmTypesHost = BaseEvmTypes> {
    /// Precompile implementation function.
    run: PrecompileFn<T>,
    /// Precompile ID.
    id: PrecompileId,
}

impl<T: EvmTypesHost> PrecompileData<T> {
    const DUMMY: Self = Self::new(PrecompileId::custom("__dummy__"), dummy_precompile);

    /// Creates precompile data.
    #[inline]
    pub const fn new(id: PrecompileId, f: PrecompileFn<T>) -> Self {
        Self { id, run: f }
    }

    /// Returns this precompile data with a different ID.
    #[inline]
    pub fn with_id(self, id: PrecompileId) -> Self {
        Self { id, run: self.run }
    }

    /// Returns this precompile data with a different implementation function.
    #[inline]
    pub fn with_run(self, f: PrecompileFn<T>) -> Self {
        Self { id: self.id, run: f }
    }

    /// Returns the precompile ID.
    #[inline]
    pub const fn id(&self) -> &PrecompileId {
        &self.id
    }

    /// Returns the precompile implementation function.
    #[inline]
    pub const fn run(&self) -> PrecompileFn<T> {
        self.run
    }

    /// Executes this precompile.
    #[inline]
    pub fn execute(
        &self,
        evm: &mut Evm<'_, T>,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> PrecompileResult {
        (self.run)(evm, message, gas)
    }
}

/// Precompile dispatch map.
#[derive_where(Clone, Debug, Default)]
pub struct PrecompileMap<T: EvmTypesHost = BaseEvmTypes> {
    inner: AddressMap<PrecompileData<T>>,
}

impl<T: EvmTypesHost> PrecompileMap<T> {
    /// Creates an empty precompile map.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty precompile map with at least the specified capacity.
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { inner: AddressMap::with_capacity_and_hasher(capacity, Default::default()) }
    }

    /// Reserves capacity for at least `additional` more precompiles.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }

    /// Creates a precompile map from precompile descriptors.
    #[inline]
    pub fn from_precompiles(precompiles: impl IntoIterator<Item = Precompile<T>>) -> Self {
        let mut map = Self::new();
        map.extend(precompiles);
        map
    }

    /// Extends this map with precompile descriptors.
    #[inline]
    pub fn extend(&mut self, precompiles: impl IntoIterator<Item = Precompile<T>>) {
        for precompile in precompiles {
            self.insert(precompile);
        }
    }

    /// Inserts a precompile descriptor, replacing any existing precompile at the same address.
    #[inline]
    pub fn insert(&mut self, precompile: Precompile<T>) -> Option<Precompile<T>> {
        let address = precompile.address();
        self.inner.insert(address, precompile.into_data()).map(|data| Precompile { address, data })
    }

    /// Removes a precompile by address.
    #[inline]
    pub fn remove(&mut self, address: Address) -> Option<Precompile<T>> {
        self.inner.remove(&address).map(|data| Precompile { address, data })
    }

    /// Removes a precompile by descriptor address.
    #[inline]
    pub fn remove_precompile(&mut self, precompile: &Precompile<T>) -> Option<Precompile<T>> {
        self.remove(precompile.address())
    }

    /// Returns the precompile at `address`, if any.
    #[inline]
    pub fn get(&self, address: &Address) -> Option<Precompile<T>> {
        self.inner.get(address).cloned().map(|data| Precompile { address: *address, data })
    }

    /// Returns the precompile data at `address`, if any.
    #[inline]
    pub(super) fn get_data(&self, address: &Address) -> Option<&PrecompileData<T>> {
        self.inner.get(address)
    }

    /// Maps the precompile at `address`, if it exists.
    #[inline]
    pub fn map_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(PrecompileData<T>) -> PrecompileData<T>,
    {
        if let Some(data) = self.inner.get_mut(address) {
            *data = f(core::mem::replace(data, PrecompileData::DUMMY));
        }
    }

    /// Maps all precompiles.
    #[inline]
    pub fn map_precompiles<F>(&mut self, mut f: F)
    where
        F: FnMut(&Address, PrecompileData<T>) -> PrecompileData<T>,
    {
        for (address, data) in &mut self.inner {
            *data = f(address, core::mem::replace(data, PrecompileData::DUMMY));
        }
    }

    /// Applies a transformation to the precompile at `address`.
    ///
    /// The closure receives the existing precompile data, if any, and returns the data that should
    /// be installed at `address`.
    #[inline]
    pub fn apply_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(Option<PrecompileData<T>>) -> Option<PrecompileData<T>>,
    {
        if let Some(data) = self.inner.get_mut(address) {
            let current = core::mem::replace(data, PrecompileData::DUMMY);
            if let Some(new_data) = f(Some(current)) {
                *data = new_data;
            } else {
                self.inner.remove(address);
            }
        } else if let Some(data) = f(None) {
            self.inner.insert(*address, data);
        }
    }

    /// Builder-style version of [`Self::map_precompile`].
    #[inline]
    pub fn with_mapped_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(PrecompileData<T>) -> PrecompileData<T>,
    {
        self.map_precompile(address, f);
        self
    }

    /// Builder-style version of [`Self::map_precompiles`].
    #[inline]
    pub fn with_mapped_precompiles<F>(mut self, f: F) -> Self
    where
        F: FnMut(&Address, PrecompileData<T>) -> PrecompileData<T>,
    {
        self.map_precompiles(f);
        self
    }

    /// Builder-style version of [`Self::apply_precompile`].
    #[inline]
    pub fn with_applied_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(Option<PrecompileData<T>>) -> Option<PrecompileData<T>>,
    {
        self.apply_precompile(address, f);
        self
    }

    /// Builder-style version of [`Self::extend`].
    #[inline]
    pub fn with_extended_precompiles(
        mut self,
        precompiles: impl IntoIterator<Item = Precompile<T>>,
    ) -> Self {
        self.extend(precompiles);
        self
    }

    /// Moves precompiles from source addresses to destination addresses.
    ///
    /// All sources are validated before the map is mutated.
    pub fn move_precompiles<I>(&mut self, moves: I) -> Result<(), MovePrecompileError>
    where
        I: IntoIterator<Item = (Address, Address)>,
    {
        let moves = moves.into_iter().filter(|(source, dest)| source != dest).collect::<Vec<_>>();

        for (source, _) in &moves {
            if !self.contains(source) {
                return Err(MovePrecompileError::NotAPrecompile(*source));
            }
        }

        let mut moved = Vec::with_capacity(moves.len());
        for (source, dest) in moves {
            if let Some(precompile) = self.remove(source) {
                moved.push(precompile.with_address(dest));
            }
        }

        self.extend(moved);
        Ok(())
    }

    /// Builder-style version of [`Self::move_precompiles`].
    #[inline]
    pub fn with_moved_precompiles<I>(mut self, moves: I) -> Result<Self, MovePrecompileError>
    where
        I: IntoIterator<Item = (Address, Address)>,
    {
        self.move_precompiles(moves)?;
        Ok(self)
    }

    /// Returns all precompile addresses.
    #[inline]
    pub fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        self.inner.keys().copied()
    }

    /// Returns `true` if the map contains `address`.
    #[inline]
    pub fn contains(&self, address: &Address) -> bool {
        self.inner.contains_key(address)
    }

    /// Returns the number of precompiles in this map.
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if this map contains no precompiles.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Shrinks the map to fit its current size.
    #[inline]
    pub fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }
}

/// Error that can occur when moving precompiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovePrecompileError {
    /// The source address is not a precompile.
    NotAPrecompile(Address),
}

impl Display for MovePrecompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAPrecompile(address) => {
                write!(f, "source address {address} is not a precompile")
            }
        }
    }
}

impl core::error::Error for MovePrecompileError {}

/// Defines precompile constants.
#[macro_export]
macro_rules! define_precompiles {
    (@address $address:literal) => {
        $crate::precompiles::u64_to_address($address)
    };
    (@address $address:expr) => {
        $address
    };
    ($(
        $(#[$attr:meta])*
        $vis:vis const $name:ident = ($address:expr, $id:expr) => $f:path;
    )*) => {
        $(
            $(#[$attr])*
            #[allow(non_snake_case)]
            $vis const fn $name<T: $crate::EvmTypesHost>() -> $crate::precompiles::Precompile<T> {
                fn run<T: $crate::EvmTypesHost>(
                    _evm: &mut $crate::Evm<'_, T>,
                    message: &$crate::interpreter::Message<T>,
                    gas: &mut $crate::interpreter::GasTracker,
                ) -> $crate::precompiles::PrecompileResult {
                    $f(message.input.as_ref(), gas)
                }

                $crate::precompiles::Precompile::new(
                    $crate::define_precompiles!(@address $address),
                    $id,
                    run::<T>,
                )
            }
        )*
    };
}

pub use crate::define_precompiles;

define_precompiles! {
    /// secp256k1 public key recovery precompile.
    pub const SECP256K1_ECRECOVER = (0x01, PrecompileId::EcRec) => secp256k1::run;
    /// SHA-256 precompile.
    pub const SHA256 = (0x02, PrecompileId::Sha256) => hash::run_sha256;
    /// RIPEMD-160 precompile.
    pub const RIPEMD160 = (0x03, PrecompileId::Ripemd160) => hash::run_ripemd160;
    /// Identity precompile.
    pub const IDENTITY = (0x04, PrecompileId::Identity) => identity::run;
    /// Byzantium modexp precompile.
    pub const MODEXP_BYZANTIUM = (0x05, PrecompileId::ModExp) => modexp::run_byzantium;
    /// Berlin modexp precompile.
    pub const MODEXP_BERLIN = (0x05, PrecompileId::ModExp) => modexp::run_berlin;
    /// Osaka modexp precompile.
    pub const MODEXP_OSAKA = (0x05, PrecompileId::ModExp) => modexp::run_osaka;
    /// Byzantium BN254 addition precompile.
    pub const BN254_ADD_BYZANTIUM = (0x06, PrecompileId::Bn254Add) => bn254::add::run_byzantium;
    /// Istanbul BN254 addition precompile.
    pub const BN254_ADD_ISTANBUL = (0x06, PrecompileId::Bn254Add) => bn254::add::run_istanbul;
    /// Byzantium BN254 multiplication precompile.
    pub const BN254_MUL_BYZANTIUM = (0x07, PrecompileId::Bn254Mul) => bn254::mul::run_byzantium;
    /// Istanbul BN254 multiplication precompile.
    pub const BN254_MUL_ISTANBUL = (0x07, PrecompileId::Bn254Mul) => bn254::mul::run_istanbul;
    /// Byzantium BN254 pairing precompile.
    pub const BN254_PAIR_BYZANTIUM = (0x08, PrecompileId::Bn254Pairing) => bn254::pair::run_byzantium;
    /// Istanbul BN254 pairing precompile.
    pub const BN254_PAIR_ISTANBUL = (0x08, PrecompileId::Bn254Pairing) => bn254::pair::run_istanbul;
    /// BLAKE2 compression precompile.
    pub const BLAKE2F = (0x09, PrecompileId::Blake2F) => blake2::run;
    /// KZG point evaluation precompile.
    pub const KZG_POINT_EVALUATION = (0x0a, PrecompileId::KzgPointEvaluation) => kzg_point_evaluation::run;
    /// BLS12-381 G1 addition precompile.
    pub const BLS12_381_G1_ADD = (0x0b, PrecompileId::Bls12G1Add) => bls12_381::g1_add::run;
    /// BLS12-381 G1 MSM precompile.
    pub const BLS12_381_G1_MSM = (0x0c, PrecompileId::Bls12G1Msm) => bls12_381::g1_msm::run;
    /// BLS12-381 G2 addition precompile.
    pub const BLS12_381_G2_ADD = (0x0d, PrecompileId::Bls12G2Add) => bls12_381::g2_add::run;
    /// BLS12-381 G2 MSM precompile.
    pub const BLS12_381_G2_MSM = (0x0e, PrecompileId::Bls12G2Msm) => bls12_381::g2_msm::run;
    /// BLS12-381 pairing precompile.
    pub const BLS12_381_PAIRING = (0x0f, PrecompileId::Bls12Pairing) => bls12_381::pairing::run;
    /// BLS12-381 map FP to G1 precompile.
    pub const BLS12_381_MAP_FP_TO_G1 = (0x10, PrecompileId::Bls12MapFpToGp1) => bls12_381::map_fp_to_g1::run;
    /// BLS12-381 map FP2 to G2 precompile.
    pub const BLS12_381_MAP_FP2_TO_G2 = (0x11, PrecompileId::Bls12MapFp2ToGp2) => bls12_381::map_fp2_to_g2::run;
    /// secp256r1 signature verification precompile.
    pub const P256VERIFY = (0x100, PrecompileId::P256Verify) => secp256r1::run;
    /// secp256r1 signature verification precompile with Osaka gas rules.
    pub const P256VERIFY_OSAKA = (0x100, PrecompileId::P256Verify) => secp256r1::run_osaka;
}

#[cfg(test)]
mod tests {
    use core::assert_matches;

    use alloy_primitives::{Bytes, address};

    use super::*;
    use crate::evm::precompile::PrecompileOutput;

    #[test]
    fn map_precompile_updates_data_at_target_address() {
        let identity = IDENTITY::<BaseEvmTypes>();
        let address = identity.address();
        let mut map = PrecompileMap::from_precompiles([identity]);

        map.map_precompile(&address, |precompile| {
            precompile
                .with_id(PrecompileId::Sha256)
                .with_run(|_, _, _| Ok(PrecompileOutput::new(Bytes::from_static(b"a"))))
        });

        let precompile = map.get(&address).unwrap();
        assert_eq!(precompile.address(), address);
        assert_eq!(precompile.id(), &PrecompileId::Sha256);
    }

    #[test]
    fn apply_precompile_inserts_and_removes_at_target_address() {
        let address = address!("0x0000000000000000000000000000000000000101");
        let mut map = PrecompileMap::<BaseEvmTypes>::new();

        map.apply_precompile(&address, |_| {
            Some(
                Precompile::new(address, PrecompileId::Identity, |_, _, _| {
                    Ok(PrecompileOutput::new(Bytes::from_static(b"a")))
                })
                .into_data(),
            )
        });

        assert!(map.contains(&address));

        map.apply_precompile(&address, |_| None);

        assert!(!map.contains(&address));
    }

    #[test]
    fn map_precompiles_preserves_existing_addresses() {
        let identity = IDENTITY::<BaseEvmTypes>();
        let sha256 = SHA256::<BaseEvmTypes>();
        let mut map = PrecompileMap::from_precompiles([identity.clone(), sha256.clone()]);

        map.map_precompiles(|_, precompile| {
            assert_matches!(precompile.id(), PrecompileId::Identity | PrecompileId::Sha256);
            precompile
                .with_id(PrecompileId::Ripemd160)
                .with_run(|_, _, _| Ok(PrecompileOutput::new(Bytes::from_static(b"b"))))
        });

        assert_eq!(map.get(&identity.address()).unwrap().id(), &PrecompileId::Ripemd160);
        assert_eq!(map.get(&sha256.address()).unwrap().id(), &PrecompileId::Ripemd160);
    }

    #[test]
    fn move_precompiles_validates_before_mutating() {
        let identity = IDENTITY::<BaseEvmTypes>();
        let sha256 = SHA256::<BaseEvmTypes>();
        let source = identity.address();
        let missing = address!("0x0000000000000000000000000000000000000999");
        let dest = address!("0x0000000000000000000000000000000000001000");
        let mut map = PrecompileMap::from_precompiles([identity]);

        let err = map.move_precompiles([(source, dest), (missing, sha256.address())]);

        assert_eq!(err, Err(MovePrecompileError::NotAPrecompile(missing)));
        assert!(map.contains(&source));
        assert!(!map.contains(&dest));
    }

    #[test]
    fn move_precompiles_moves_after_validation() {
        let identity = IDENTITY::<BaseEvmTypes>();
        let sha256 = SHA256::<BaseEvmTypes>();
        let identity_address = identity.address();
        let sha256_address = sha256.address();
        let new_identity = address!("0x0000000000000000000000000000000000001001");
        let new_sha256 = address!("0x0000000000000000000000000000000000001002");
        let mut map = PrecompileMap::from_precompiles([identity, sha256]);

        map.move_precompiles([(identity_address, new_identity), (sha256_address, new_sha256)])
            .unwrap();

        assert!(!map.contains(&identity_address));
        assert!(!map.contains(&sha256_address));
        assert!(map.contains(&new_identity));
        assert!(map.contains(&new_sha256));
    }

    #[test]
    fn move_precompiles_skips_duplicate_sources_after_first_move() {
        let identity = IDENTITY::<BaseEvmTypes>();
        let identity_address = identity.address();
        let first = address!("0x0000000000000000000000000000000000001001");
        let second = address!("0x0000000000000000000000000000000000001002");
        let mut map = PrecompileMap::from_precompiles([identity]);

        map.move_precompiles([(identity_address, first), (identity_address, second)]).unwrap();

        assert!(map.contains(&first));
        assert!(!map.contains(&second));
    }
}
