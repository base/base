//! Helpers for dealing with Precompiles.

use crate::{env::BlockEnvironment, Database, EvmInternals};
use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    vec::Vec,
};
use alloy_consensus::transaction::Either;
use alloy_primitives::{
    map::{AddressMap, AddressSet},
    Address, U256,
};
use core::fmt::{self, Debug, Display};
use revm::{
    context::ContextTr,
    handler::{precompile_output_to_interpreter_result, EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{PrecompileFn, PrecompileId, PrecompileResult, Precompiles},
    Context, Journal,
};

/// Returns whether the given [`PrecompileId`] supports caching.
///
/// This returns `false` for precompiles where the cost of computing a cache key (hashing the
/// input) is comparable to just re-executing the precompile (e.g., the identity precompile which
/// simply copies input to output).
const fn precompile_id_supports_caching(id: &PrecompileId) -> bool {
    !matches!(id, PrecompileId::Identity)
}

/// A mapping of precompile contracts that can be either static (builtin) or dynamic.
///
/// This is an optimization that allows us to keep using the static precompiles
/// until we need to modify them, at which point we convert to the dynamic representation.
pub struct PrecompilesMap {
    /// The wrapped precompiles in their current representation.
    precompiles: PrecompilesKind,
    /// An optional dynamic precompile loader that can lookup precompiles dynamically.
    lookup: Option<Box<dyn PrecompileLookup>>,
}

impl PrecompilesMap {
    /// Creates the [`PrecompilesMap`] from a static reference.
    pub fn from_static(precompiles: &'static Precompiles) -> Self {
        Self::new(Cow::Borrowed(precompiles))
    }

    /// Creates a new set of precompiles for a spec.
    pub fn new(precompiles: Cow<'static, Precompiles>) -> Self {
        Self { precompiles: PrecompilesKind::Builtin(precompiles), lookup: None }
    }

    /// Maps a precompile at the given address using the provided function.
    pub fn map_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(DynPrecompile) -> DynPrecompile + 'static,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();

        // get the current precompile at the address
        if let Some(dyn_precompile) = dyn_precompiles.inner.remove(address) {
            // apply the transformation function
            let transformed = f(dyn_precompile);

            // update the precompile at the address
            dyn_precompiles.inner.insert(*address, transformed);
        }
    }

    /// Maps all precompiles using the provided function.
    pub fn map_precompiles<F>(&mut self, f: F)
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
    {
        self.map_precompiles_filtered(f, |_, _| true);
    }

    /// Maps all cacheable precompiles using the provided function.
    ///
    /// This is a variant of [`Self::map_precompiles`] that only applies the transformation
    /// to precompiles that support caching, see [`Precompile::supports_caching`].
    pub fn map_cacheable_precompiles<F>(&mut self, f: F)
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
    {
        self.map_precompiles_filtered(f, |_, precompile| precompile.supports_caching());
    }

    /// Internal helper to map precompiles with an optional filter.
    ///
    /// The `filter` decides whether to apply the mapping function `f` to a given
    /// precompile. If the filter returns `false`, the original precompile is kept.
    #[inline]
    fn map_precompiles_filtered<F, P>(&mut self, mut f: F, mut filter: P)
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
        P: FnMut(&Address, &DynPrecompile) -> bool,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();

        // apply the transformation to each precompile
        let entries = dyn_precompiles.inner.drain();
        let mut new_map =
            AddressMap::with_capacity_and_hasher(entries.size_hint().0, Default::default());
        for (addr, precompile) in entries {
            if filter(&addr, &precompile) {
                let transformed = f(&addr, precompile);
                new_map.insert(addr, transformed);
            } else {
                new_map.insert(addr, precompile);
            }
        }

        dyn_precompiles.inner = new_map;
    }

    /// Applies a transformation to the precompile at the given address.
    ///
    /// This method allows you to add, update, or remove a precompile by applying a closure
    /// to the existing precompile (if any) at the specified address.
    ///
    /// # Behavior
    ///
    /// The closure receives:
    /// - `Some(precompile)` if a precompile exists at the address
    /// - `None` if no precompile exists at the address
    ///
    /// Based on what the closure returns:
    /// - `Some(precompile)` - Insert or replace the precompile at the address
    /// - `None` - Remove the precompile from the address (if it exists)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Add a new precompile
    /// precompiles.apply_precompile(&address, |_| Some(my_precompile));
    ///
    /// // Update an existing precompile
    /// precompiles.apply_precompile(&address, |existing| {
    ///     existing.map(|p| wrap_with_logging(p))
    /// });
    ///
    /// // Remove a precompile
    /// precompiles.apply_precompile(&address, |_| None);
    ///
    /// // Conditionally update
    /// precompiles.apply_precompile(&address, |existing| {
    ///     if let Some(p) = existing {
    ///         Some(modify_precompile(p))
    ///     } else {
    ///         Some(create_default_precompile())
    ///     }
    /// });
    /// ```
    pub fn apply_precompile<F>(&mut self, address: &Address, f: F)
    where
        F: FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>,
    {
        let dyn_precompiles = self.ensure_dynamic_precompiles();
        let current = dyn_precompiles.inner.remove(address);

        // apply the transformation function
        let result = f(current);

        match result {
            Some(transformed) => {
                // insert the transformed precompile
                dyn_precompiles.inner.insert(*address, transformed);
                dyn_precompiles.addresses.insert(*address);
            }
            None => {
                // remove the precompile if the transformation returned None
                dyn_precompiles.addresses.remove(address);
            }
        }
    }

    /// Builder-style method that maps a precompile at the given address using the provided
    /// function.
    ///
    /// This is a consuming version of [`map_precompile`](Self::map_precompile) that returns `Self`.
    pub fn with_mapped_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(DynPrecompile) -> DynPrecompile + 'static,
    {
        self.map_precompile(address, f);
        self
    }

    /// Builder-style method that maps all precompiles using the provided function.
    ///
    /// This is a consuming version of [`map_precompiles`](Self::map_precompiles) that returns
    /// `Self`.
    pub fn with_mapped_precompiles<F>(mut self, f: F) -> Self
    where
        F: FnMut(&Address, DynPrecompile) -> DynPrecompile,
    {
        self.map_precompiles(f);
        self
    }

    /// Builder-style method that applies a transformation to the precompile at the given address.
    ///
    /// This is a consuming version of [`apply_precompile`](Self::apply_precompile) that returns
    /// `Self`. See [`apply_precompile`](Self::apply_precompile) for detailed behavior and
    /// examples.
    pub fn with_applied_precompile<F>(mut self, address: &Address, f: F) -> Self
    where
        F: FnOnce(Option<DynPrecompile>) -> Option<DynPrecompile>,
    {
        self.apply_precompile(address, f);
        self
    }

    /// Extends the precompile map with multiple precompiles.
    ///
    /// This is a convenience method for inserting or replacing multiple precompiles at once.
    /// Each precompile in the iterator is applied to its corresponding address.
    ///
    /// **Note**: This method will **replace** any existing precompiles at the given addresses.
    /// If you need to modify existing precompiles, use [`map_precompile`](Self::map_precompile)
    /// or [`apply_precompile`](Self::apply_precompile) instead.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let precompiles = vec![
    ///     (address1, my_precompile1),
    ///     (address2, my_precompile2),
    /// ];
    /// precompiles_map.extend_precompiles(precompiles);
    /// ```
    pub fn extend_precompiles<I>(&mut self, precompiles: I)
    where
        I: IntoIterator<Item = (Address, DynPrecompile)>,
    {
        for (addr, precompile) in precompiles {
            self.apply_precompile(&addr, |_| Some(precompile));
        }
    }

    /// Builder-style method that extends the precompile map with multiple precompiles.
    ///
    /// This is a consuming version of [`extend_precompiles`](Self::extend_precompiles) that returns
    /// `Self`.
    ///
    /// **Note**: This method will **replace** any existing precompiles at the given addresses.
    /// If you need to modify existing precompiles, use
    /// [`with_mapped_precompile`](Self::with_mapped_precompile)
    /// or [`with_applied_precompile`](Self::with_applied_precompile) instead.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let precompiles = vec![
    ///     (address1, my_precompile1),
    ///     (address2, my_precompile2),
    /// ];
    /// let map = PrecompilesMap::new(precompiles_cow)
    ///     .with_extended_precompiles(precompiles);
    /// ```
    pub fn with_extended_precompiles<I>(mut self, precompiles: I) -> Self
    where
        I: IntoIterator<Item = (Address, DynPrecompile)>,
    {
        self.extend_precompiles(precompiles);
        self
    }

    /// Moves precompiles from source addresses to destination addresses.
    ///
    /// For each `(source, dest)` pair in the iterator:
    /// - If `source == dest`, the pair is skipped (no-op).
    /// - If the source address is not a precompile, returns an error.
    /// - Otherwise, the precompile is removed from the source address and installed at the
    ///   destination address.
    ///
    /// # Errors
    ///
    /// Returns [`MovePrecompileError::NotAPrecompile`] if any source address does not have a
    /// precompile installed.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Move ECRECOVER from 0x01 to a custom address
    /// let moves = [(
    ///     address!("0x0000000000000000000000000000000000000001"),
    ///     address!("0x0000000000000000000000000000000000000100"),
    /// )];
    /// precompiles.move_precompiles(moves)?;
    /// ```
    pub fn move_precompiles<I>(&mut self, moves: I) -> Result<(), MovePrecompileError>
    where
        I: IntoIterator<Item = (Address, Address)>,
    {
        let moves: Vec<_> = moves.into_iter().filter(|(src, dest)| src != dest).collect();

        if moves.is_empty() {
            return Ok(());
        }

        // Validate all source addresses are precompiles before making any changes
        for (source, _dest) in &moves {
            if self.get(source).is_none() {
                return Err(MovePrecompileError::NotAPrecompile(*source));
            }
        }

        // Extract precompiles from source addresses
        let mut extracted: Vec<(Address, DynPrecompile)> = Vec::with_capacity(moves.len());

        for (source, dest) in moves {
            let mut found_precompile: Option<DynPrecompile> = None;
            self.apply_precompile(&source, |existing| {
                found_precompile = existing;
                None
            });

            if let Some(precompile) = found_precompile {
                extracted.push((dest, precompile));
            }
        }

        // Install precompiles at destination addresses
        for (dest, precompile) in extracted {
            self.apply_precompile(&dest, |_| Some(precompile));
        }

        Ok(())
    }

    /// Builder-style method that moves precompiles from source addresses to destination addresses.
    ///
    /// This is a consuming version of [`move_precompiles`](Self::move_precompiles) that returns
    /// `Self` on success.
    ///
    /// # Errors
    ///
    /// Returns [`MovePrecompileError::NotAPrecompile`] if any source address does not have a
    /// precompile installed.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let moves = [(
    ///     address!("0x0000000000000000000000000000000000000001"),
    ///     address!("0x0000000000000000000000000000000000000100"),
    /// )];
    /// let map = PrecompilesMap::new(precompiles_cow)
    ///     .with_moved_precompiles(moves)?;
    /// ```
    pub fn with_moved_precompiles<I>(mut self, moves: I) -> Result<Self, MovePrecompileError>
    where
        I: IntoIterator<Item = (Address, Address)>,
    {
        self.move_precompiles(moves)?;
        Ok(self)
    }

    /// Sets a dynamic precompile lookup function that is called for addresses not found
    /// in the static precompile map.
    ///
    /// This method allows you to provide runtime-resolved precompiles that aren't known
    /// at initialization time. The lookup function is called whenever a precompile check
    /// is performed for an address that doesn't exist in the main precompile map.
    ///
    /// # Important Notes
    ///
    /// - **Priority**: Static precompiles take precedence. The lookup function is only called if
    ///   the address is not found in the main precompile map.
    /// - **Gas accounting**: Addresses resolved through this lookup are always treated as cold,
    ///   meaning they incur cold access costs even on repeated calls within the same transaction.
    ///   See also [`PrecompileProvider::warm_addresses`].
    /// - **Performance**: The lookup function is called on every precompile check for
    ///   non-registered addresses, so it should be efficient.
    ///
    /// # Example
    ///
    /// ```ignore
    /// precompiles.set_precompile_lookup(|address| {
    ///     // Dynamically resolve precompiles based on address pattern
    ///     if address.as_slice().starts_with(&[0xDE, 0xAD]) {
    ///         Some(DynPrecompile::new(|input| {
    ///             // Custom precompile logic
    ///             Ok(PrecompileOutput {
    ///                 gas_used: 100,
    ///                 bytes: Bytes::from("dynamic precompile"),
    ///             })
    ///         }))
    ///     } else {
    ///         None
    ///     }
    /// });
    /// ```
    pub fn set_precompile_lookup<L>(&mut self, lookup: L)
    where
        L: PrecompileLookup + 'static,
    {
        self.lookup = Some(Box::new(lookup));
    }

    /// Maps the dynamic precompile lookup function while preserving access to the previous lookup.
    ///
    /// This is useful when layering additional lookup behavior on top of an existing dynamic
    /// resolver. The mapper receives the requested address and the previous lookup, if one was
    /// installed. Callers can delegate to the previous lookup for addresses they do not handle.
    pub fn map_precompile_lookup<F>(&mut self, f: F)
    where
        F: Fn(&Address, Option<&dyn PrecompileLookup>) -> Option<DynPrecompile>
            + Send
            + Sync
            + 'static,
    {
        let previous = self.lookup.take();
        self.lookup = Some(Box::new(move |address: &Address| f(address, previous.as_deref())));
    }

    /// Builder-style method to set a dynamic precompile lookup function.
    ///
    /// This is a consuming version of [`set_precompile_lookup`](Self::set_precompile_lookup)
    /// that returns `Self` for method chaining.
    ///
    /// See [`set_precompile_lookup`](Self::set_precompile_lookup) for detailed behavior,
    /// important notes, and examples.
    pub fn with_precompile_lookup<L>(mut self, lookup: L) -> Self
    where
        L: PrecompileLookup + 'static,
    {
        self.set_precompile_lookup(lookup);
        self
    }

    /// Builder-style method to map the dynamic precompile lookup function.
    ///
    /// This is a consuming version of [`map_precompile_lookup`](Self::map_precompile_lookup)
    /// that returns `Self` for method chaining.
    pub fn with_mapped_precompile_lookup<F>(mut self, f: F) -> Self
    where
        F: Fn(&Address, Option<&dyn PrecompileLookup>) -> Option<DynPrecompile>
            + Send
            + Sync
            + 'static,
    {
        self.map_precompile_lookup(f);
        self
    }

    /// Consumes the type and returns a set of [`DynPrecompile`].
    pub fn into_dyn_precompiles(mut self) -> DynPrecompiles {
        self.ensure_dynamic_precompiles();
        match self.precompiles {
            PrecompilesKind::Dynamic(dynamic) => dynamic,
            _ => unreachable!("We just ensured that this is a Dynamic variant"),
        }
    }

    /// Ensures that precompiles are in their dynamic representation.
    /// If they are already dynamic, this is a no-op.
    /// Returns a mutable reference to the dynamic precompiles.
    pub fn ensure_dynamic_precompiles(&mut self) -> &mut DynPrecompiles {
        if let PrecompilesKind::Builtin(ref precompiles_cow) = self.precompiles {
            let mut dynamic = DynPrecompiles::default();

            let static_precompiles = match precompiles_cow {
                Cow::Borrowed(static_ref) => static_ref,
                Cow::Owned(owned) => owned,
            };

            for (&addr, pc) in static_precompiles.inner().iter() {
                dynamic.inner.insert(addr, DynPrecompile(Box::new(pc.clone())));
                dynamic.addresses.insert(addr);
            }

            self.precompiles = PrecompilesKind::Dynamic(dynamic);
        }

        match &mut self.precompiles {
            PrecompilesKind::Dynamic(dynamic) => dynamic,
            _ => unreachable!("We just ensured that this is a Dynamic variant"),
        }
    }

    /// Returns an iterator over the [`PrecompileId`]s of the installed precompiles.
    pub fn identifiers(&self) -> impl Iterator<Item = &PrecompileId> {
        match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => {
                Either::Left(precompiles.inner().values().map(|p| p.precompile_id()))
            }
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                Either::Right(dyn_precompiles.inner.values().map(|p| p.precompile_id()))
            }
        }
    }

    /// Returns an iterator over references to precompile addresses.
    pub fn addresses(&self) -> impl Iterator<Item = &Address> {
        match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => Either::Left(precompiles.addresses()),
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                Either::Right(dyn_precompiles.addresses.iter())
            }
        }
    }

    /// Gets a reference to the precompile at the given address.
    ///
    /// This method first checks the static precompile map, and if not found,
    /// falls back to the dynamic lookup function (if set).
    pub fn get(&self, address: &Address) -> Option<impl Precompile + '_> {
        // First check static precompiles
        let static_result = match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => precompiles.get(address).map(Either::Left),
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(address).map(Either::Right)
            }
        };

        // If found in static precompiles, wrap in Left and return
        if let Some(precompile) = static_result {
            return Some(Either::Left(precompile));
        }

        // Otherwise, try the lookup function if available
        let lookup = self.lookup.as_ref()?;
        lookup.lookup(address).map(Either::Right)
    }
}

impl From<EthPrecompiles> for PrecompilesMap {
    fn from(value: EthPrecompiles) -> Self {
        Self::from_static(value.precompiles)
    }
}

impl core::fmt::Debug for PrecompilesMap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.precompiles {
            PrecompilesKind::Builtin(_) => f.debug_struct("PrecompilesMap::Builtin").finish(),
            PrecompilesKind::Dynamic(precompiles) => f
                .debug_struct("PrecompilesMap::Dynamic")
                .field("addresses", &precompiles.addresses)
                .finish(),
        }
    }
}

impl<BlockEnv, TxEnv, CfgEnv, DB, Chain>
    PrecompileProvider<Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, Chain>> for PrecompilesMap
where
    BlockEnv: BlockEnvironment,
    TxEnv: revm::context::Transaction,
    CfgEnv: revm::context::Cfg,
    DB: Database,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, _spec: CfgEnv::Spec) -> bool {
        false
    }

    fn run(
        &mut self,
        context: &mut Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, Chain>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        // Get the precompile at the address
        let Some(precompile) = self.get(&inputs.bytecode_address) else {
            return Ok(None);
        };

        let (block, tx, cfg, journaled_state, _, local) = context.all_mut();

        let precompile_output = {
            let _span =
                tracing::trace_span!("precompile", name = precompile.precompile_id().name(),)
                    .entered();
            precompile.call(PrecompileInput {
                data: inputs.input.as_bytes_local(local).as_ref(),
                gas: inputs.gas_limit,
                reservoir: inputs.reservoir,
                caller: inputs.caller,
                value: inputs.call_value(),
                is_static: inputs.is_static,
                internals: EvmInternals::new(journaled_state, block, cfg, tx),
                target_address: inputs.target_address,
                bytecode_address: inputs.bytecode_address,
            })
        }
        .map_err(|e| e.to_string())?;

        Ok(Some(precompile_output_to_interpreter_result(precompile_output, inputs.gas_limit)))
    }

    fn warm_addresses(&self) -> &AddressSet {
        match &self.precompiles {
            PrecompilesKind::Builtin(precompiles) => precompiles.addresses_set(),
            PrecompilesKind::Dynamic(dyn_precompiles) => &dyn_precompiles.addresses,
        }
    }

    fn contains(&self, address: &Address) -> bool {
        self.get(address).is_some()
    }
}

/// A mapping of precompile contracts that can be either static (builtin) or dynamic.
///
/// This is an optimization that allows us to keep using the static precompiles
/// until we need to modify them, at which point we convert to the dynamic representation.
enum PrecompilesKind {
    /// Static builtin precompiles.
    Builtin(Cow<'static, Precompiles>),
    /// Dynamic precompiles that can be modified at runtime.
    Dynamic(DynPrecompiles),
}

/// A dynamic precompile implementation that can be modified at runtime.
pub struct DynPrecompile(pub(crate) Box<dyn Precompile>);

impl DynPrecompile {
    /// Creates a new [`DynPrecompiles`] with the given closure.
    pub fn new<F>(id: PrecompileId, f: F) -> Self
    where
        F: Fn(PrecompileInput<'_>) -> PrecompileResult + 'static,
    {
        Self(Box::new((id, f)))
    }

    /// Creates a new [`DynPrecompiles`] with the given closure and
    /// [`Precompile::supports_caching`] returning `false`.
    pub fn new_stateful<F>(id: PrecompileId, f: F) -> Self
    where
        F: Fn(PrecompileInput<'_>) -> PrecompileResult + 'static,
    {
        Self(Box::new(StatefulPrecompile((id, f))))
    }

    /// Flips [`Precompile::supports_caching`] to `false`.
    pub fn stateful(self) -> Self {
        Self(Box::new(StatefulPrecompile(self.0)))
    }
}

impl core::fmt::Debug for DynPrecompile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynPrecompile").finish()
    }
}

/// A mutable representation of precompiles that allows for runtime modification.
///
/// This structure stores dynamic precompiles that can be modified at runtime,
/// unlike the static `Precompiles` struct from revm.
#[derive(Default)]
pub struct DynPrecompiles {
    /// Precompiles
    inner: AddressMap<DynPrecompile>,
    /// Addresses of precompile
    addresses: AddressSet,
}

impl DynPrecompiles {
    /// Consumes the type and returns an iterator over the addresses and the corresponding
    /// precompile.
    pub fn into_precompiles(self) -> impl Iterator<Item = (Address, DynPrecompile)> {
        self.inner.into_iter()
    }
}

impl core::fmt::Debug for DynPrecompiles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DynPrecompiles").field("addresses", &self.addresses).finish()
    }
}

/// Input for a precompile call.
#[derive(Debug)]
pub struct PrecompileInput<'a> {
    /// Input data bytes.
    pub data: &'a [u8],
    /// Gas limit.
    pub gas: u64,
    /// State gas reservoir (EIP-8037). Zero on mainnet.
    pub reservoir: u64,
    /// Caller address.
    pub caller: Address,
    /// Value sent with the call.
    pub value: U256,
    /// Target address of the call. Would be the same as `bytecode_address` unless it's a
    /// DELEGATECALL.
    pub target_address: Address,
    /// Whether this call is in a STATICCALL context.
    pub is_static: bool,
    /// Bytecode address of the call.
    pub bytecode_address: Address,
    /// Various hooks for interacting with the EVM state.
    pub internals: EvmInternals<'a>,
}

impl<'a> PrecompileInput<'a> {
    /// Returns the calldata of the call.
    pub const fn data(&self) -> &[u8] {
        self.data
    }

    /// Returns the caller address of the call.
    pub const fn caller(&self) -> &Address {
        &self.caller
    }

    /// Returns the gas limit of the call.
    pub const fn gas(&self) -> u64 {
        self.gas
    }

    /// Returns the value of the call.
    pub const fn value(&self) -> &U256 {
        &self.value
    }

    /// Returns the target address of the call.
    pub const fn target_address(&self) -> &Address {
        &self.target_address
    }

    /// Returns the bytecode address of the call.
    pub const fn bytecode_address(&self) -> &Address {
        &self.bytecode_address
    }

    /// Returns whether the call is a direct call, i.e when precompile was called directly and not
    /// via a DELEGATECALL/CALLCODE.
    pub fn is_direct_call(&self) -> bool {
        self.target_address == self.bytecode_address
    }

    /// Returns whether this call is in a STATICCALL context.
    pub const fn is_static_call(&self) -> bool {
        self.is_static
    }

    /// Returns the [`EvmInternals`].
    pub const fn internals(&self) -> &EvmInternals<'_> {
        &self.internals
    }

    /// Returns a mutable reference to the [`EvmInternals`].
    pub const fn internals_mut(&mut self) -> &mut EvmInternals<'a> {
        &mut self.internals
    }
}

/// Trait for implementing precompiled contracts.
#[auto_impl::auto_impl(&, Box, Arc)]
pub trait Precompile {
    /// Returns precompile ID.
    fn precompile_id(&self) -> &PrecompileId;

    /// Execute the precompile with the given input data, gas limit, and caller address.
    ///
    /// `Ok(PrecompileOutput)` covers non-fatal outcomes (success, revert, halt such as
    /// out-of-gas), distinguished by [`PrecompileOutput::status`]. `Err(PrecompileError)` is
    /// reserved for fatal errors that abort EVM execution.
    ///
    /// [`PrecompileOutput::status`]: revm::precompile::PrecompileOutput::status
    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult;

    /// Returns whether this precompile's results should be cached.
    ///
    /// A cacheable precompile has deterministic output based solely on its input,
    /// and the cost of execution is high enough that caching the result is beneficial.
    ///
    /// # Default
    ///
    /// Returns `true` by default, indicating the precompile supports caching,
    /// as this is what most precompiles benefit from.
    ///
    /// # When to return `false`
    ///
    /// - **Non-deterministic precompiles**: If the output depends on state or external factors
    /// - **Cheap precompiles**: If the cost of computing a cache key (hashing the input) is
    ///   comparable to just re-executing the precompile (e.g., the identity precompile which simply
    ///   copies input to output)
    ///
    /// # Examples
    ///
    /// ```ignore
    /// impl Precompile for MyPrecompile {
    ///     fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
    ///         // ...
    ///     }
    ///
    ///     fn supports_caching(&self) -> bool {
    ///         false
    ///     }
    /// }
    /// ```
    fn supports_caching(&self) -> bool {
        true
    }
}

impl<F> Precompile for (PrecompileId, F)
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult,
{
    fn precompile_id(&self) -> &PrecompileId {
        &self.0
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.1(input)
    }

    fn supports_caching(&self) -> bool {
        precompile_id_supports_caching(&self.0)
    }
}

impl<F> Precompile for (&PrecompileId, F)
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult,
{
    fn precompile_id(&self) -> &PrecompileId {
        self.0
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.1(input)
    }

    fn supports_caching(&self) -> bool {
        precompile_id_supports_caching(self.0)
    }
}

impl Precompile for revm::precompile::Precompile {
    fn precompile_id(&self) -> &PrecompileId {
        self.id()
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.execute(input.data, input.gas, input.reservoir)
    }

    fn supports_caching(&self) -> bool {
        precompile_id_supports_caching(self.id())
    }
}

impl<F> From<F> for DynPrecompile
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult + 'static,
{
    fn from(f: F) -> Self {
        Self::new(PrecompileId::Custom("closure".into()), f)
    }
}

impl From<PrecompileFn> for DynPrecompile {
    fn from(f: PrecompileFn) -> Self {
        let p = move |input: PrecompileInput<'_>| -> PrecompileResult {
            f(input.data, input.gas, input.reservoir)
        };
        p.into()
    }
}

impl<F> From<(PrecompileId, F)> for DynPrecompile
where
    F: Fn(PrecompileInput<'_>) -> PrecompileResult + 'static,
{
    fn from((id, f): (PrecompileId, F)) -> Self {
        Self(Box::new((id, f)))
    }
}

impl From<(PrecompileId, PrecompileFn)> for DynPrecompile {
    fn from((id, f): (PrecompileId, PrecompileFn)) -> Self {
        let p = move |input: PrecompileInput<'_>| -> PrecompileResult {
            f(input.data, input.gas, input.reservoir)
        };
        (id, p).into()
    }
}

impl Precompile for DynPrecompile {
    fn precompile_id(&self) -> &PrecompileId {
        self.0.precompile_id()
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.0.call(input)
    }

    fn supports_caching(&self) -> bool {
        self.0.supports_caching()
    }
}

impl<A: Precompile, B: Precompile> Precompile for Either<A, B> {
    fn precompile_id(&self) -> &PrecompileId {
        match self {
            Self::Left(p) => p.precompile_id(),
            Self::Right(p) => p.precompile_id(),
        }
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        match self {
            Self::Left(p) => p.call(input),
            Self::Right(p) => p.call(input),
        }
    }

    fn supports_caching(&self) -> bool {
        match self {
            Self::Left(p) => p.supports_caching(),
            Self::Right(p) => p.supports_caching(),
        }
    }
}

struct StatefulPrecompile<P>(P);

impl<P: Precompile> Precompile for StatefulPrecompile<P> {
    fn precompile_id(&self) -> &PrecompileId {
        self.0.precompile_id()
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        self.0.call(input)
    }

    fn supports_caching(&self) -> bool {
        false
    }
}

/// Trait for dynamically resolving precompile contracts.
///
/// This trait allows for runtime resolution of precompiles that aren't known
/// at initialization time.
pub trait PrecompileLookup {
    /// Looks up a precompile at the given address.
    ///
    /// Returns `Some(precompile)` if a precompile exists at the address,
    /// or `None` if no precompile is found.
    fn lookup(&self, address: &Address) -> Option<DynPrecompile>;
}

/// Implement PrecompileLookup for closure types
impl<F> PrecompileLookup for F
where
    F: Fn(&Address) -> Option<DynPrecompile>,
{
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        self(address)
    }
}

/// Error that can occur when moving precompiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MovePrecompileError {
    /// The source address is not a precompile.
    NotAPrecompile(Address),
}

impl Display for MovePrecompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAPrecompile(addr) => {
                write!(f, "source address {addr} is not a precompile")
            }
        }
    }
}

impl core::error::Error for MovePrecompileError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::EthEvmContext;
    use alloy_primitives::{address, Bytes};
    use revm::{
        context::BlockEnv,
        database::EmptyDB,
        precompile::{PrecompileId, PrecompileOutput},
        primitives::hardfork::SpecId,
    };

    #[test]
    fn test_map_precompile() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());

        // create a test input for the precompile (identity precompile)
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let test_input = Bytes::from_static(b"test data");
        let gas_limit = 1000;

        // Ensure we're using dynamic precompiles
        spec_precompiles.ensure_dynamic_precompiles();

        // using the dynamic precompiles interface
        let dyn_precompile = match &spec_precompiles.precompiles {
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(&identity_address).unwrap()
            }
            _ => panic!("Expected dynamic precompiles"),
        };

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: identity_address,
                bytecode_address: identity_address,
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Identity precompile should return the input data");

        // define a function to modify the precompile
        // this will change the identity precompile to always return a fixed value
        let constant_bytes = Bytes::from_static(b"constant value");

        // define a function to modify the precompile to always return a constant value
        spec_precompiles.map_precompile(&identity_address, move |_original_dyn| {
            // create a new DynPrecompile that always returns our constant
            (|_input: PrecompileInput<'_>| -> PrecompileResult {
                Ok(PrecompileOutput::new(10, Bytes::from_static(b"constant value"), 0))
            })
            .into()
        });

        // get the modified precompile and check it
        let dyn_precompile = match &spec_precompiles.precompiles {
            PrecompilesKind::Dynamic(dyn_precompiles) => {
                dyn_precompiles.inner.get(&identity_address).unwrap()
            }
            _ => panic!("Expected dynamic precompiles"),
        };

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: identity_address,
                bytecode_address: identity_address,
            })
            .unwrap();
        assert_eq!(
            result.bytes, constant_bytes,
            "Modified precompile should return the constant value"
        );
    }

    #[test]
    fn test_evm_internals_downcasts_block_env() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        let internals = EvmInternals::from_context(&mut ctx);

        assert!(internals.block_env_downcast_ref::<BlockEnv>().is_some());
    }

    #[test]
    fn test_closure_precompile() {
        let test_input = Bytes::from_static(b"test data");
        let expected_output = Bytes::from_static(b"processed: test data");
        let gas_limit = 1000;

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());

        // define a closure that implements the precompile functionality
        let closure_precompile = |input: PrecompileInput<'_>| -> PrecompileResult {
            let _timestamp = input.internals.block_env().timestamp();
            let mut output = b"processed: ".to_vec();
            output.extend_from_slice(input.data.as_ref());
            Ok(PrecompileOutput::new(15, Bytes::from(output), 0))
        };

        let dyn_precompile: DynPrecompile = closure_precompile.into();

        let result = dyn_precompile
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: Address::ZERO,
                bytecode_address: Address::ZERO,
            })
            .unwrap();
        let gas_used = result.gas_used;
        assert_eq!(gas_used, 15);
        assert_eq!(result.bytes, expected_output);
    }

    #[test]
    fn test_supports_caching() {
        let closure_precompile = |_input: PrecompileInput<'_>| -> PrecompileResult {
            Ok(PrecompileOutput::new(10, Bytes::from_static(b"output"), 0))
        };

        let dyn_precompile: DynPrecompile = closure_precompile.into();
        assert!(dyn_precompile.supports_caching(), "should support caching by default");

        let stateful_precompile =
            DynPrecompile::new_stateful(PrecompileId::Custom("closure".into()), closure_precompile);
        assert!(
            !stateful_precompile.supports_caching(),
            "stateful precompile should not support caching"
        );

        let either_left = Either::<DynPrecompile, DynPrecompile>::Left(stateful_precompile);
        assert!(
            !either_left.supports_caching(),
            "Either::Left with non-cacheable should return false"
        );

        let either_right = Either::<DynPrecompile, DynPrecompile>::Right(dyn_precompile);
        assert!(either_right.supports_caching(), "Either::Right with cacheable should return true");

        // Identity precompile should not support caching
        let identity = revm::precompile::identity::FUN;
        assert!(!identity.supports_caching(), "identity precompile should not support caching");

        // Other builtin precompiles should support caching
        let sha256 = revm::precompile::hash::SHA256;
        assert!(sha256.supports_caching(), "sha256 precompile should support caching");
    }

    #[test]
    fn test_precompile_lookup() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());

        // Define a custom address pattern for dynamic precompiles
        let dynamic_prefix = [0xDE, 0xAD];

        // Set up the lookup function
        spec_precompiles.set_precompile_lookup(move |address: &Address| {
            if address.as_slice().starts_with(&dynamic_prefix) {
                Some(DynPrecompile::new(PrecompileId::Custom("dynamic".into()), |_input| {
                    Ok(PrecompileOutput::new(100, Bytes::from("dynamic precompile response"), 0))
                }))
            } else {
                None
            }
        });

        // Test that static precompiles still work
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        assert!(spec_precompiles.get(&identity_address).is_some());

        // Test dynamic lookup for matching address
        let dynamic_address = address!("0xDEAD000000000000000000000000000000000001");
        let dynamic_precompile = spec_precompiles.get(&dynamic_address);
        assert!(dynamic_precompile.is_some(), "Dynamic precompile should be found");

        // Execute the dynamic precompile
        let gas_limit = 1000;
        let result = dynamic_precompile
            .unwrap()
            .call(PrecompileInput {
                data: &[],
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: dynamic_address,
                bytecode_address: dynamic_address,
            })
            .unwrap();
        let gas_used = result.gas_used;
        assert_eq!(gas_used, 100);
        assert_eq!(result.bytes, Bytes::from("dynamic precompile response"));

        // Test non-matching address returns None
        let non_matching_address = address!("0x1234000000000000000000000000000000000001");
        assert!(spec_precompiles.get(&non_matching_address).is_none());
    }

    #[test]
    fn test_map_precompile_lookup_preserves_previous_lookup() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let previous_lookup_address = address!("0x1000000000000000000000000000000000000001");
        let mapped_lookup_address = address!("0x2000000000000000000000000000000000000001");
        let missing_address = address!("0x3000000000000000000000000000000000000001");

        spec_precompiles.set_precompile_lookup(move |address: &Address| {
            (address == &previous_lookup_address).then(|| {
                DynPrecompile::new(PrecompileId::Custom("previous".into()), |_input| {
                    Ok(PrecompileOutput::new(100, Bytes::new(), 0))
                })
            })
        });

        spec_precompiles.map_precompile_lookup(move |address, previous| {
            if address == &mapped_lookup_address {
                return Some(DynPrecompile::new(PrecompileId::Custom("mapped".into()), |_input| {
                    Ok(PrecompileOutput::new(200, Bytes::new(), 0))
                }));
            }

            previous.and_then(|lookup| lookup.lookup(address))
        });

        assert!(spec_precompiles.get(&previous_lookup_address).is_some());
        assert!(spec_precompiles.get(&mapped_lookup_address).is_some());
        assert!(spec_precompiles.get(&missing_address).is_none());
    }

    #[test]
    fn test_get_precompile() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());

        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let test_input = Bytes::from_static(b"test data");
        let gas_limit = 1000;

        let precompile = spec_precompiles.get(&identity_address);
        assert!(precompile.is_some(), "Identity precompile should exist");

        let result = precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                target_address: identity_address,
                bytecode_address: identity_address,
                internals: EvmInternals::from_context(&mut ctx),
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Identity precompile should return the input data");

        let nonexistent_address = address!("0x0000000000000000000000000000000000000099");
        assert!(
            spec_precompiles.get(&nonexistent_address).is_none(),
            "Non-existent precompile should not be found"
        );

        let mut dynamic_precompiles = spec_precompiles;
        dynamic_precompiles.ensure_dynamic_precompiles();

        let dyn_precompile = dynamic_precompiles.get(&identity_address);
        assert!(
            dyn_precompile.is_some(),
            "Identity precompile should exist after conversion to dynamic"
        );

        let result = dyn_precompile
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: identity_address,
                bytecode_address: identity_address,
            })
            .unwrap();
        assert_eq!(
            result.bytes, test_input,
            "Identity precompile should return the input data after conversion to dynamic"
        );
    }

    #[test]
    fn test_move_precompiles() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());

        // Identity precompile at address 0x04
        let identity_address = address!("0x0000000000000000000000000000000000000004");
        let new_address = address!("0x0000000000000000000000000000000000001000");
        let test_input = Bytes::from_static(b"test data");
        let gas_limit = 1000;

        // Verify the precompile exists at original address
        assert!(spec_precompiles.get(&identity_address).is_some());
        assert!(spec_precompiles.get(&new_address).is_none());

        // Move the precompile
        spec_precompiles.move_precompiles([(identity_address, new_address)]).unwrap();

        // Verify the precompile moved
        assert!(
            spec_precompiles.get(&identity_address).is_none(),
            "Precompile should no longer exist at original address"
        );
        assert!(
            spec_precompiles.get(&new_address).is_some(),
            "Precompile should exist at new address"
        );

        // Verify the moved precompile works correctly
        let result = spec_precompiles
            .get(&new_address)
            .unwrap()
            .call(PrecompileInput {
                data: &test_input,
                gas: gas_limit,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                is_static: false,
                internals: EvmInternals::from_context(&mut ctx),
                target_address: new_address,
                bytecode_address: new_address,
            })
            .unwrap();
        assert_eq!(result.bytes, test_input, "Moved identity precompile should return input data");
    }

    #[test]
    fn test_move_precompiles_not_a_precompile() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let non_precompile = address!("0x0000000000000000000000000000000000000099");
        let dest = address!("0x0000000000000000000000000000000000001000");

        let result = spec_precompiles.move_precompiles([(non_precompile, dest)]);
        assert_eq!(result, Err(MovePrecompileError::NotAPrecompile(non_precompile)));
    }

    #[test]
    fn test_move_precompiles_same_address_noop() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let identity_address = address!("0x0000000000000000000000000000000000000004");

        // Moving to same address should be a no-op and not error
        spec_precompiles.move_precompiles([(identity_address, identity_address)]).unwrap();

        // Precompile should still exist
        assert!(spec_precompiles.get(&identity_address).is_some());
    }

    #[test]
    fn test_move_precompiles_multiple() {
        let eth_precompiles = EthPrecompiles::new(SpecId::default());
        let mut spec_precompiles = PrecompilesMap::from(eth_precompiles);

        let ecrecover = address!("0x0000000000000000000000000000000000000001");
        let sha256 = address!("0x0000000000000000000000000000000000000002");
        let new_ecrecover = address!("0x0000000000000000000000000000000000001001");
        let new_sha256 = address!("0x0000000000000000000000000000000000001002");

        spec_precompiles
            .move_precompiles([(ecrecover, new_ecrecover), (sha256, new_sha256)])
            .unwrap();

        // Original addresses should be empty
        assert!(spec_precompiles.get(&ecrecover).is_none());
        assert!(spec_precompiles.get(&sha256).is_none());

        // New addresses should have the precompiles
        assert!(spec_precompiles.get(&new_ecrecover).is_some());
        assert!(spec_precompiles.get(&new_sha256).is_some());
    }
}
