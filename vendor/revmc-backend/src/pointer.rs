use crate::Builder;

/// A pointer to a value.
#[derive(Clone, Copy, Debug)]
pub struct Pointer<B: Builder> {
    /// The type of the pointee.
    pub ty: B::Type,
    /// The base of the pointer. Either an address or a stack slot.
    pub base: PointerBase<B>,
}

/// The base of a pointer. Either an address or a stack slot.
#[derive(Clone, Copy, Debug)]
pub enum PointerBase<B: Builder> {
    /// An address.
    Address(B::Value),
    /// A stack slot.
    StackSlot(B::StackSlot),
}

impl<B: Builder> Pointer<B> {
    /// Creates a new stack-allocated pointer.
    pub fn new_stack_slot(bcx: &mut B, ty: B::Type, name: &str) -> Self {
        let slot = bcx.new_stack_slot_raw(ty, name);
        Self { ty, base: PointerBase::StackSlot(slot) }
    }

    /// Creates a new address pointer.
    pub fn new_address(ty: B::Type, value: B::Value) -> Self {
        Self { ty, base: PointerBase::Address(value) }
    }

    /// Returns `true` if the pointer is an address.
    pub fn is_address(&self) -> bool {
        matches!(self.base, PointerBase::Address(_))
    }

    /// Returns `true` if the pointer is a stack slot.
    pub fn is_stack_slot(&self) -> bool {
        matches!(self.base, PointerBase::StackSlot(_))
    }

    /// Converts the pointer to an address.
    pub fn into_address(self) -> Option<B::Value> {
        match self.base {
            PointerBase::Address(ptr) => Some(ptr),
            PointerBase::StackSlot(_) => None,
        }
    }

    /// Converts the pointer to a stack slot.
    pub fn into_stack_slot(self) -> Option<B::StackSlot> {
        match self.base {
            PointerBase::Address(_) => None,
            PointerBase::StackSlot(slot) => Some(slot),
        }
    }

    /// Loads the value from the pointer.
    pub fn load(&self, bcx: &mut B, name: &str) -> B::Value {
        match self.base {
            PointerBase::Address(ptr) => bcx.load(self.ty, ptr, name),
            PointerBase::StackSlot(slot) => bcx.stack_load(self.ty, slot, name),
        }
    }

    /// Stores the value to the pointer.
    pub fn store(&self, bcx: &mut B, value: B::Value) {
        match self.base {
            PointerBase::Address(ptr) => bcx.store(value, ptr),
            PointerBase::StackSlot(slot) => bcx.stack_store(value, slot),
        }
    }

    /// Stores the value to the pointer.
    pub fn store_imm(&self, bcx: &mut B, value: i64) {
        let value = bcx.iconst(self.ty, value);
        self.store(bcx, value)
    }

    /// Gets the address of the pointer.
    pub fn addr(&self, bcx: &mut B) -> B::Value {
        match self.base {
            PointerBase::Address(ptr) => ptr,
            PointerBase::StackSlot(slot) => bcx.stack_addr(self.ty, slot),
        }
    }
}
