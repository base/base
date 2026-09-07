//! Type bindings for js tracing inspector

use crate::tracing::{
    js::builtins::{
        address_to_uint8_array, address_to_uint8_array_value, bytes_from_value, bytes_to_address,
        bytes_to_b256, to_bigint, to_uint8_array, to_uint8_array_value,
    },
    types::CallKind,
    TransactionContext,
};
use alloc::{
    boxed::Box,
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use alloy_primitives::{Address, Bytes, B256, U256};
use boa_engine::{
    js_string,
    native_function::NativeFunction,
    object::{builtins::JsUint8Array, FunctionObjectBuilder},
    Context, JsArgs, JsError, JsNativeError, JsObject, JsResult, JsValue,
};
use boa_gc::{empty_trace, Finalize, Trace};
use core::cell::RefCell;
use revm::{
    bytecode::opcode::{OpCode, PUSH0, PUSH32},
    context_interface::DBErrorMarker,
    interpreter::{SharedMemory, Stack},
    primitives::KECCAK_EMPTY,
    state::{AccountInfo, Bytecode, EvmState},
    Database, DatabaseRef,
};

/// Shared mutable state captured by JS native functions.
#[derive(Clone, Debug)]
struct Shared<T>(Rc<RefCell<T>>);

impl<T> Shared<T> {
    fn new(value: T) -> Self {
        Self(Rc::new(RefCell::new(value)))
    }

    fn replace(&self, value: T) {
        *self.0.borrow_mut() = value;
    }
}

impl<T> Finalize for Shared<T> {}

unsafe impl<T> Trace for Shared<T> {
    empty_trace!();
}

/// Reusable JS log object for opcode steps.
#[derive(Debug)]
pub(crate) struct ReusableStepLog {
    state: Shared<StepLogState>,
    object: JsObject,
}

impl ReusableStepLog {
    pub(crate) fn new(ctx: &mut Context) -> JsResult<Self> {
        let state = Shared::new(StepLogState::default());
        let object = JsObject::with_object_proto(ctx.intrinsics());

        object.set(js_string!("op"), build_step_op_object(state.clone(), ctx)?, false, ctx)?;
        object.set(
            js_string!("memory"),
            build_step_memory_object(state.clone(), ctx)?,
            false,
            ctx,
        )?;
        object.set(
            js_string!("stack"),
            build_step_stack_object(state.clone(), ctx)?,
            false,
            ctx,
        )?;
        object.set(
            js_string!("contract"),
            build_step_contract_object(state.clone(), ctx)?,
            false,
            ctx,
        )?;

        let get_pc = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().pc)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_gas = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().gas_remaining)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_cost = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().cost)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_depth = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().depth)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_refund = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().refund)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_error = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| {
                    Ok(state
                        .0
                        .borrow()
                        .error
                        .as_ref()
                        .map(|error| JsValue::from(js_string!(error.as_str())))
                        .unwrap_or_else(JsValue::undefined))
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();

        object.set(js_string!("getPC"), get_pc, false, ctx)?;
        object.set(js_string!("getError"), get_error, false, ctx)?;
        object.set(js_string!("getGas"), get_gas, false, ctx)?;
        object.set(js_string!("getCost"), get_cost, false, ctx)?;
        object.set(js_string!("getDepth"), get_depth, false, ctx)?;
        object.set(js_string!("getRefund"), get_refund, false, ctx)?;

        Ok(Self { state, object })
    }

    pub(crate) fn update(&self, step: StepLog) {
        self.state.replace(step.into());
    }

    pub(crate) fn value(&self) -> JsValue {
        self.object.clone().into()
    }
}

/// Reusable JS call frame object for enter callbacks.
#[derive(Debug)]
pub(crate) struct ReusableCallFrame {
    state: Shared<CallFrameState>,
    object: JsObject,
}

impl ReusableCallFrame {
    pub(crate) fn new(ctx: &mut Context) -> JsResult<Self> {
        let state = Shared::new(CallFrameState::default());
        let object = JsObject::with_object_proto(ctx.intrinsics());

        let get_from = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, ctx| {
                    address_to_uint8_array_value(state.0.borrow().caller, ctx)
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_to = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, ctx| {
                    address_to_uint8_array_value(state.0.borrow().contract, ctx)
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_value = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| to_bigint(state.0.borrow().value),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_input = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, ctx| {
                    to_uint8_array_value(state.0.borrow().input.clone(), ctx)
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_gas = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().gas)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_type = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| {
                    Ok(JsValue::from(js_string!(state.0.borrow().kind.as_str())))
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();

        object.set(js_string!("getFrom"), get_from, false, ctx)?;
        object.set(js_string!("getTo"), get_to, false, ctx)?;
        object.set(js_string!("getValue"), get_value, false, ctx)?;
        object.set(js_string!("getInput"), get_input, false, ctx)?;
        object.set(js_string!("getGas"), get_gas, false, ctx)?;
        object.set(js_string!("getType"), get_type, false, ctx)?;

        Ok(Self { state, object })
    }

    pub(crate) fn update(&self, frame: CallFrame) {
        self.state.replace(frame.into());
    }

    pub(crate) fn value(&self) -> JsValue {
        self.object.clone().into()
    }
}

/// Reusable JS frame result object for exit callbacks.
#[derive(Debug)]
pub(crate) struct ReusableFrameResult {
    state: Shared<FrameResultState>,
    object: JsObject,
}

impl ReusableFrameResult {
    pub(crate) fn new(ctx: &mut Context) -> JsResult<Self> {
        let state = Shared::new(FrameResultState::default());
        let object = JsObject::with_object_proto(ctx.intrinsics());

        let get_gas_used = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().gas_used)),
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_output = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, ctx| {
                    to_uint8_array_value(state.0.borrow().output.clone(), ctx)
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();
        let get_error = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, _args, state, _ctx| {
                    Ok(state
                        .0
                        .borrow()
                        .error
                        .as_ref()
                        .map(|error| JsValue::from(js_string!(error.as_str())))
                        .unwrap_or_else(JsValue::undefined))
                },
                state.clone(),
            ),
        )
        .length(0)
        .build();

        object.set(js_string!("getGasUsed"), get_gas_used, false, ctx)?;
        object.set(js_string!("getOutput"), get_output, false, ctx)?;
        object.set(js_string!("getError"), get_error, false, ctx)?;

        Ok(Self { state, object })
    }

    pub(crate) fn update(&self, frame: FrameResult) {
        self.state.replace(frame.into());
    }

    pub(crate) fn value(&self) -> JsValue {
        self.object.clone().into()
    }
}

/// Reusable JS database object for step and fault callbacks.
#[derive(Debug)]
pub(crate) struct ReusableEvmDb {
    state: Shared<EvmDbState>,
    object: JsObject,
}

impl ReusableEvmDb {
    pub(crate) fn new(ctx: &mut Context) -> JsResult<Self> {
        let state = Shared::new(EvmDbState::default());
        let object = JsObject::with_object_proto(ctx.intrinsics());

        let exists = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, state, ctx| {
                    let db = current_db(state)?;
                    let acc = db.read_basic(args.get_or_undefined(0).clone(), ctx)?;
                    Ok(JsValue::from(acc.is_some()))
                },
                state.clone(),
            ),
        )
        .length(1)
        .build();
        let get_balance = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, state, ctx| {
                    let db = current_db(state)?;
                    let balance = db
                        .read_basic(args.get_or_undefined(0).clone(), ctx)?
                        .map(|acc| acc.balance)
                        .unwrap_or_default();
                    to_bigint(balance)
                },
                state.clone(),
            ),
        )
        .length(1)
        .build();
        let get_nonce = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, state, ctx| {
                    let db = current_db(state)?;
                    let nonce = db
                        .read_basic(args.get_or_undefined(0).clone(), ctx)?
                        .map(|acc| acc.nonce)
                        .unwrap_or_default();
                    Ok(JsValue::from(nonce))
                },
                state.clone(),
            ),
        )
        .length(1)
        .build();
        let get_code = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, state, ctx| {
                    let db = current_db(state)?;
                    Ok(db.read_code(args.get_or_undefined(0).clone(), ctx)?.into())
                },
                state.clone(),
            ),
        )
        .length(1)
        .build();
        let get_state = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, state, ctx| {
                    let db = current_db(state)?;
                    Ok(db
                        .read_state(
                            args.get_or_undefined(0).clone(),
                            args.get_or_undefined(1).clone(),
                            ctx,
                        )?
                        .into())
                },
                state.clone(),
            ),
        )
        .length(2)
        .build();

        object.set(js_string!("getBalance"), get_balance, false, ctx)?;
        object.set(js_string!("getNonce"), get_nonce, false, ctx)?;
        object.set(js_string!("getCode"), get_code, false, ctx)?;
        object.set(js_string!("getState"), get_state, false, ctx)?;
        object.set(js_string!("exists"), exists, false, ctx)?;

        Ok(Self { state, object })
    }

    pub(crate) fn update(&self, db: EvmDbRef) {
        self.state.replace(EvmDbState { current: Some(db) });
    }

    pub(crate) fn value(&self) -> JsValue {
        self.object.clone().into()
    }
}

#[derive(Clone, Debug, Default)]
struct EvmDbState {
    current: Option<EvmDbRef>,
}

fn current_db(state: &Shared<EvmDbState>) -> JsResult<EvmDbRef> {
    state.0.borrow().current.clone().ok_or_else(|| {
        JsError::from_native(
            JsNativeError::typ().with_message("tracer accessed db before it was initialized"),
        )
    })
}

#[derive(Clone, Debug, Default)]
struct StepLogState {
    stack: Option<StackRef>,
    op: u8,
    memory: Option<MemoryRef>,
    pc: u64,
    gas_remaining: u64,
    cost: u64,
    depth: u64,
    refund: u64,
    error: Option<String>,
    contract: Contract,
}

impl From<StepLog> for StepLogState {
    fn from(step: StepLog) -> Self {
        Self {
            stack: Some(step.stack),
            op: step.op.0,
            memory: Some(step.memory),
            pc: step.pc,
            gas_remaining: step.gas_remaining,
            cost: step.cost,
            depth: step.depth,
            refund: step.refund,
            error: step.error,
            contract: step.contract,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CallFrameState {
    caller: Address,
    contract: Address,
    value: U256,
    input: Bytes,
    gas: u64,
    kind: String,
}

impl From<CallFrame> for CallFrameState {
    fn from(frame: CallFrame) -> Self {
        Self {
            caller: frame.contract.caller,
            contract: frame.contract.contract,
            value: frame.contract.value,
            input: frame.contract.input,
            gas: frame.gas,
            kind: frame.kind.to_string(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FrameResultState {
    gas_used: u64,
    output: Bytes,
    error: Option<String>,
}

impl From<FrameResult> for FrameResultState {
    fn from(frame: FrameResult) -> Self {
        Self { gas_used: frame.gas_used, output: frame.output, error: frame.error }
    }
}

fn build_step_op_object(state: Shared<StepLogState>, context: &mut Context) -> JsResult<JsObject> {
    let obj = JsObject::with_object_proto(context.intrinsics());
    let to_number = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| Ok(JsValue::from(state.0.borrow().op)),
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let is_push = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| {
                Ok(JsValue::from((PUSH0..=PUSH32).contains(&state.0.borrow().op)))
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let to_string = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| {
                let value = state.0.borrow().op;
                if let Some(op) = OpCode::new(value) {
                    Ok(JsValue::from(js_string!(op.as_str())))
                } else {
                    Ok(JsValue::from(js_string!(format!("opcode {:x} not defined", value))))
                }
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();

    obj.set(js_string!("toNumber"), to_number, false, context)?;
    obj.set(js_string!("toString"), to_string, false, context)?;
    obj.set(js_string!("isPush"), is_push, false, context)?;
    Ok(obj)
}

fn build_step_stack_object(
    state: Shared<StepLogState>,
    context: &mut Context,
) -> JsResult<JsObject> {
    let obj = JsObject::with_object_proto(context.intrinsics());
    let length = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| {
                let len = state
                    .0
                    .borrow()
                    .stack
                    .as_ref()
                    .and_then(|stack| stack.0.with_inner(Stack::len))
                    .unwrap_or_default();
                Ok(JsValue::from(len))
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let peek = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, args, state, ctx| {
                let stack = state.0.borrow().stack.clone().ok_or_else(|| {
                    JsError::from_native(
                        JsNativeError::typ()
                            .with_message("tracer accessed stack before it was initialized"),
                    )
                })?;
                let len = stack.0.with_inner(Stack::len).unwrap_or_default();
                let idx = StackRef::parse_index(args.get_or_undefined(0), len, ctx)?;
                if len <= idx {
                    return Err(JsError::from_native(JsNativeError::typ().with_message(format!(
                        "tracer accessed out of bound stack: size {len}, index {idx}"
                    ))));
                }
                stack.peek(idx)
            },
            state.clone(),
        ),
    )
    .length(1)
    .build();

    obj.set(js_string!("length"), length, false, context)?;
    obj.set(js_string!("peek"), peek, false, context)?;
    Ok(obj)
}

fn build_step_memory_object(
    state: Shared<StepLogState>,
    context: &mut Context,
) -> JsResult<JsObject> {
    let obj = JsObject::with_object_proto(context.intrinsics());
    let length = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| {
                let len = state.0.borrow().memory.as_ref().map(MemoryRef::len).unwrap_or_default();
                Ok(JsValue::from(len as u64))
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let slice = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, args, state, ctx| {
                let memory = state.0.borrow().memory.clone().ok_or_else(|| {
                    JsError::from_native(
                        JsNativeError::typ()
                            .with_message("tracer accessed memory before it was initialized"),
                    )
                })?;
                let len = memory.len();
                let start = MemoryRef::parse_index(args.get_or_undefined(0), "start", len, ctx)?;
                let end = MemoryRef::parse_index(args.get_or_undefined(1), "end", len, ctx)?;
                if end < start || end > len {
                    return Err(MemoryRef::out_of_bounds_error(
                        len,
                        start,
                        end.saturating_sub(start),
                    ));
                }
                let slice = memory
                    .0
                    .with_inner(|mem| mem.slice_len(start, end - start).to_vec())
                    .unwrap_or_default();
                to_uint8_array_value(slice, ctx)
            },
            state.clone(),
        ),
    )
    .length(2)
    .build();
    let get_uint = FunctionObjectBuilder::new(
        context.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, args, state, ctx| {
                let memory = state.0.borrow().memory.clone().ok_or_else(|| {
                    JsError::from_native(
                        JsNativeError::typ()
                            .with_message("tracer accessed memory before it was initialized"),
                    )
                })?;
                let len = memory.len();
                let offset = MemoryRef::parse_index(args.get_or_undefined(0), "offset", len, ctx)?;
                let Some(end) = offset.checked_add(32) else {
                    return Err(MemoryRef::out_of_bounds_error(len, offset, 32));
                };
                if end > len {
                    return Err(MemoryRef::out_of_bounds_error(len, offset, 32));
                }
                let slice = memory
                    .0
                    .with_inner(|mem| mem.slice_len(offset, 32).to_vec())
                    .unwrap_or_default();
                to_uint8_array_value(slice, ctx)
            },
            state.clone(),
        ),
    )
    .length(1)
    .build();

    obj.set(js_string!("slice"), slice, false, context)?;
    obj.set(js_string!("getUint"), get_uint, false, context)?;
    obj.set(js_string!("length"), length, false, context)?;
    Ok(obj)
}

fn build_step_contract_object(
    state: Shared<StepLogState>,
    ctx: &mut Context,
) -> JsResult<JsObject> {
    let obj = JsObject::with_object_proto(ctx.intrinsics());
    let get_caller = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, ctx| {
                address_to_uint8_array_value(state.0.borrow().contract.caller, ctx)
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let get_address = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, ctx| {
                address_to_uint8_array_value(state.0.borrow().contract.contract, ctx)
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let get_value = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, _ctx| to_bigint(state.0.borrow().contract.value),
            state.clone(),
        ),
    )
    .length(0)
    .build();
    let get_input = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            move |_this, _args, state, ctx| {
                to_uint8_array_value(state.0.borrow().contract.input.clone(), ctx)
            },
            state.clone(),
        ),
    )
    .length(0)
    .build();

    obj.set(js_string!("getCaller"), get_caller, false, ctx)?;
    obj.set(js_string!("getAddress"), get_address, false, ctx)?;
    obj.set(js_string!("getValue"), get_value, false, ctx)?;
    obj.set(js_string!("getInput"), get_input, false, ctx)?;
    Ok(obj)
}

/// A wrapper for a value that can be garbage collected, but will not give access to the value if
/// it has been dropped via its guard.
///
/// This is used to allow the JS tracer functions to access values at a certain point during
/// inspection by ref without having to clone them and capture them in the js object.
///
/// JS tracer functions get access to evm internals via objects or function arguments, for example
/// `function step(log,evm)` where log has an object `stack` that has a function `peek(number)` that
/// returns a value from the stack.
///
/// These functions could get garbage collected, however the data accessed by the function is
/// supposed to be ephemeral and only valid for the duration of the function call.
///
/// This type supports garbage collection of (rust) references and prevents access to the value if
/// it has been dropped.
#[derive(Clone, Debug)]
struct GuardedNullableGc<Val: 'static> {
    /// The lifetime is a lie to make it possible to use a reference in boa which requires 'static
    inner: Rc<RefCell<Option<Guarded<'static, Val>>>>,
}

impl<Val: 'static> GuardedNullableGc<Val> {
    /// Creates a garbage collectible value to the given reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the guard is dropped before the value is dropped.
    fn new_ref(val: &Val) -> (Self, GcGuard<'_, Val>) {
        Self::new(Guarded::Ref(val))
    }

    /// Creates a garbage collectible value to the given reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the guard is dropped before the value is dropped.
    fn new_owned<'a>(val: Val) -> (Self, GcGuard<'a, Val>) {
        Self::new(Guarded::Owned(val))
    }

    fn new(val: Guarded<'_, Val>) -> (Self, GcGuard<'_, Val>) {
        let inner = Rc::new(RefCell::new(Some(val)));
        let guard = GcGuard { inner: Rc::clone(&inner) };

        // SAFETY: guard enforces that the value is removed from the refcell before it is dropped.
        #[allow(clippy::missing_transmute_annotations)]
        let this = Self { inner: unsafe { core::mem::transmute(inner) } };

        (this, guard)
    }

    /// Executes the given closure with a reference to the inner value if it is still present.
    fn with_inner<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Val) -> R,
    {
        self.inner.borrow().as_ref().map(|guard| f(guard.as_ref()))
    }
}

impl<Val: 'static> Finalize for GuardedNullableGc<Val> {}

unsafe impl<Val: 'static> Trace for GuardedNullableGc<Val> {
    empty_trace!();
}

/// A value that is either a reference or an owned value.
#[derive(Debug)]
enum Guarded<'a, T> {
    Ref(&'a T),
    Owned(T),
}

impl<T> Guarded<'_, T> {
    #[inline]
    fn as_ref(&self) -> &T {
        match self {
            Guarded::Ref(val) => val,
            Guarded::Owned(val) => val,
        }
    }
}

/// Guard the inner value, once this value is dropped the inner value is also removed.
///
/// This type guarantees that it never outlives the wrapped value.
#[derive(Debug)]
#[must_use]
pub(crate) struct GcGuard<'a, Val> {
    inner: Rc<RefCell<Option<Guarded<'a, Val>>>>,
}

impl<Val> Drop for GcGuard<'_, Val> {
    fn drop(&mut self) {
        self.inner.borrow_mut().take();
    }
}

/// The Log object that is passed to the javascript inspector.
#[derive(Debug)]
pub(crate) struct StepLog {
    /// Stack before step execution
    pub(crate) stack: StackRef,
    /// Opcode to be executed
    pub(crate) op: OpObj,
    /// All allocated memory in a step
    pub(crate) memory: MemoryRef,
    /// Program counter before step execution
    pub(crate) pc: u64,
    /// Remaining gas before step execution
    pub(crate) gas_remaining: u64,
    /// Gas cost of step execution
    pub(crate) cost: u64,
    /// Call depth
    pub(crate) depth: u64,
    /// Gas refund counter before step execution
    pub(crate) refund: u64,
    /// returns information about the error if one occurred, otherwise returns undefined
    pub(crate) error: Option<String>,
    /// The contract object available to the js inspector
    pub(crate) contract: Contract,
}

/// An owned snapshot of memory contents.
///
/// Uses `Rc` internally so cloning is cheap (reference count bump) rather than
/// copying the entire memory buffer on every step.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemorySnapshot(Rc<Vec<u8>>);

impl MemorySnapshot {
    /// Creates a snapshot by copying the context memory from `SharedMemory`.
    pub(crate) fn from_shared_memory(mem: &SharedMemory) -> Self {
        Self(Rc::new(mem.context_memory().to_vec()))
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn slice_len(&self, offset: usize, size: usize) -> &[u8] {
        &self.0[offset..offset + size]
    }
}

/// Represents the memory object
#[derive(Clone, Debug)]
pub(crate) struct MemoryRef(GuardedNullableGc<MemorySnapshot>);

impl MemoryRef {
    /// Creates a new memory reference from an owned snapshot.
    pub(crate) fn new_owned(mem: MemorySnapshot) -> (Self, GcGuard<'static, MemorySnapshot>) {
        let (inner, guard) = GuardedNullableGc::new_owned(mem);
        (Self(inner), guard)
    }

    fn len(&self) -> usize {
        self.0.with_inner(|mem| mem.len()).unwrap_or_default()
    }

    fn invalid_index_error(name: &str, index: impl core::fmt::Display) -> JsError {
        JsError::from_native(
            JsNativeError::typ().with_message(format!("invalid memory {name}: {index}")),
        )
    }

    fn check_index_bounds(index: usize, name: &str, len: usize) -> JsResult<usize> {
        if index > len {
            return Err(Self::invalid_index_error(name, index));
        }
        Ok(index)
    }

    fn parse_index(value: &JsValue, name: &str, len: usize, ctx: &mut Context) -> JsResult<usize> {
        if value.is_undefined() {
            return Err(Self::invalid_index_error(name, "undefined"));
        }
        if let Some(index) = value.as_number() {
            if !index.is_finite() || index < 0. {
                return Err(Self::invalid_index_error(name, index));
            }
        }
        // Boa's `ToIndex` rejects BigInt, but stack-derived tracer values are BigInt.
        if let Some(index) = value.as_bigint() {
            let index = index.to_string();
            let index =
                index.parse::<usize>().map_err(|_| Self::invalid_index_error(name, &index))?;
            return Self::check_index_bounds(index, name, len);
        }

        let index = value.to_index(ctx)?;
        let index = usize::try_from(index).map_err(|_| Self::invalid_index_error(name, index))?;
        Self::check_index_bounds(index, name, len)
    }

    fn out_of_bounds_error(len: usize, offset: usize, size: usize) -> JsError {
        JsError::from_native(JsNativeError::typ().with_message(format!(
            "tracer accessed out of bound memory: available {len}, offset {offset}, size {size}"
        )))
    }
}

impl Finalize for MemoryRef {}

unsafe impl Trace for MemoryRef {
    empty_trace!();
}

/// Represents the state object
#[derive(Clone, Debug)]
pub(crate) struct StateRef(GuardedNullableGc<EvmState>);

impl StateRef {
    /// Creates a new stack reference
    pub(crate) fn new(state: &EvmState) -> (Self, GcGuard<'_, EvmState>) {
        let (inner, guard) = GuardedNullableGc::new_ref(state);
        (Self(inner), guard)
    }

    fn get_account(&self, address: &Address) -> Option<AccountInfo> {
        self.0.with_inner(|state| state.get(address).map(|acc| acc.info.clone()))?
    }
}

impl Finalize for StateRef {}

unsafe impl Trace for StateRef {
    empty_trace!();
}

/// Represents the database
#[derive(Clone, Debug)]
pub(crate) struct GcDb<DB: 'static>(GuardedNullableGc<DB>);

impl<DB> GcDb<DB>
where
    DB: DatabaseRef + 'static,
{
    /// Creates a new stack reference
    fn new<'a>(db: DB) -> (Self, GcGuard<'a, DB>) {
        let (inner, guard) = GuardedNullableGc::new_owned(db);
        (Self(inner), guard)
    }
}

impl<DB: 'static> Finalize for GcDb<DB> {}

unsafe impl<DB: 'static> Trace for GcDb<DB> {
    empty_trace!();
}

/// Represents the opcode object
#[derive(Debug)]
pub(crate) struct OpObj(pub(crate) u8);

impl From<u8> for OpObj {
    fn from(op: u8) -> Self {
        Self(op)
    }
}

/// Represents the stack object
#[derive(Clone, Debug)]
pub(crate) struct StackRef(GuardedNullableGc<Stack>);

impl StackRef {
    /// Creates a new stack reference from an owned stack.
    pub(crate) fn new_owned(stack: Stack) -> (Self, GcGuard<'static, Stack>) {
        let (inner, guard) = GuardedNullableGc::new_owned(stack);
        (Self(inner), guard)
    }

    fn parse_index(value: &JsValue, len: usize, ctx: &mut Context) -> JsResult<usize> {
        let index = value.to_numeric_number(ctx)?;
        if !index.is_finite() || index < 0. || index > usize::MAX as f64 {
            return Err(JsError::from_native(JsNativeError::typ().with_message(format!(
                "tracer accessed out of bound stack: size {len}, index {index}"
            ))));
        }
        Ok(index as usize)
    }

    fn peek(&self, idx: usize) -> JsResult<JsValue> {
        self.0
            .with_inner(|stack| {
                stack
                    .peek(idx)
                    .map_err(|_| {
                        JsError::from_native(JsNativeError::typ().with_message(format!(
                            "tracer accessed out of bound stack: size {}, index {}",
                            stack.len(),
                            idx
                        )))
                    })
                    .and_then(to_bigint)
            })
            .ok_or_else(|| {
                JsError::from_native(
                    JsNativeError::typ()
                        .with_message("tracer accessed stack after it was dropped".to_string()),
                )
            })?
    }
}

impl Finalize for StackRef {}

unsafe impl Trace for StackRef {
    empty_trace!();
}

/// Represents the contract object
#[derive(Clone, Debug, Default)]
pub(crate) struct Contract {
    pub(crate) caller: Address,
    pub(crate) contract: Address,
    pub(crate) value: U256,
    pub(crate) input: Bytes,
}

/// Represents the call frame object for exit functions
pub(crate) struct FrameResult {
    pub(crate) gas_used: u64,
    pub(crate) output: Bytes,
    pub(crate) error: Option<String>,
}

/// Represents the call frame object for enter functions
pub(crate) struct CallFrame {
    pub(crate) contract: Contract,
    pub(crate) kind: CallKind,
    pub(crate) gas: u64,
}

/// The `ctx` object that represents the context in which the transaction is executed.
pub(crate) struct JsEvmContext {
    /// String, one of the two values CALL and CREATE
    pub(crate) r#type: String,
    /// Sender of the transaction
    pub(crate) from: Address,
    /// Target of the transaction
    pub(crate) to: Option<Address>,
    pub(crate) input: Bytes,
    /// Gas limit
    pub(crate) gas: u64,
    /// Number, amount of gas used in executing the transaction (excludes txdata costs)
    pub(crate) gas_used: u64,
    /// Number, gas price configured in the transaction being executed
    pub(crate) gas_price: u64,
    /// Number, intrinsic gas for the transaction being executed
    pub(crate) intrinsic_gas: u64,
    /// big.int Amount to be transferred in wei
    pub(crate) value: U256,
    /// Number, block number
    pub(crate) block: u64,
    /// Address, miner of the block
    pub(crate) coinbase: Address,
    pub(crate) output: Bytes,
    /// Number, block timestamp
    pub(crate) time: String,
    pub(crate) transaction_ctx: TransactionContext,
    /// returns information about the error if one occurred, otherwise returns undefined
    pub(crate) error: Option<String>,
}

impl JsEvmContext {
    pub(crate) fn into_js_object(self, ctx: &mut Context) -> JsResult<JsObject> {
        let Self {
            r#type,
            from,
            to,
            input,
            gas,
            gas_used,
            gas_price,
            intrinsic_gas,
            value,
            block,
            coinbase,
            output,
            time,
            transaction_ctx,
            error,
        } = self;
        let obj = JsObject::with_object_proto(ctx.intrinsics());

        // add properties

        obj.set(js_string!("type"), js_string!(r#type), false, ctx)?;
        obj.set(js_string!("from"), address_to_uint8_array(from, ctx)?, false, ctx)?;
        if let Some(to) = to {
            obj.set(js_string!("to"), address_to_uint8_array(to, ctx)?, false, ctx)?;
        } else {
            obj.set(js_string!("to"), JsValue::null(), false, ctx)?;
        }

        obj.set(js_string!("input"), to_uint8_array(input, ctx)?, false, ctx)?;
        obj.set(js_string!("gas"), gas, false, ctx)?;
        obj.set(js_string!("gasUsed"), gas_used, false, ctx)?;
        obj.set(js_string!("gasPrice"), gas_price, false, ctx)?;
        obj.set(js_string!("intrinsicGas"), intrinsic_gas, false, ctx)?;
        obj.set(js_string!("value"), to_bigint(value)?, false, ctx)?;
        obj.set(js_string!("block"), block, false, ctx)?;
        obj.set(js_string!("coinbase"), address_to_uint8_array(coinbase, ctx)?, false, ctx)?;
        obj.set(js_string!("output"), to_uint8_array(output, ctx)?, false, ctx)?;
        obj.set(js_string!("time"), js_string!(time), false, ctx)?;
        if let Some(block_hash) = transaction_ctx.block_hash {
            obj.set(js_string!("blockHash"), to_uint8_array(block_hash, ctx)?, false, ctx)?;
        }
        if let Some(tx_index) = transaction_ctx.tx_index {
            obj.set(js_string!("txIndex"), tx_index as u64, false, ctx)?;
        }
        if let Some(tx_hash) = transaction_ctx.tx_hash {
            obj.set(js_string!("txHash"), to_uint8_array(tx_hash, ctx)?, false, ctx)?;
        }
        if let Some(error) = error {
            obj.set(js_string!("error"), js_string!(error), false, ctx)?;
        }

        Ok(obj)
    }
}

/// DB is the object that allows the js inspector to interact with the database.
#[derive(Clone)]
pub(crate) struct EvmDbRef {
    inner: Rc<EvmDbRefInner>,
}

impl core::fmt::Debug for EvmDbRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EvmDbRef").finish_non_exhaustive()
    }
}

impl EvmDbRef {
    /// Creates a new evm and db JS object.
    pub(crate) fn new<'a, 'b, DB>(state: &'a EvmState, db: &'b mut DB) -> (Self, EvmDbGuard<'a, 'b>)
    where
        DB: Database,
        DB::Error: core::fmt::Display,
    {
        let (state, state_guard) = StateRef::new(state);

        // SAFETY:
        //
        // boa requires 'static lifetime for all objects.
        // As mentioned in the `Safety` section of [GuardedNullableGc] the caller of this function
        // needs to guarantee that the passed-in lifetime is sufficiently long for the lifetime of
        // the guard.
        let db = JsDb(RefCell::new(db));
        let js_db = unsafe {
            core::mem::transmute::<
                Box<dyn DatabaseRef<Error = StringError> + '_>,
                Box<dyn DatabaseRef<Error = StringError> + 'static>,
            >(Box::new(db))
        };

        let (db, db_guard) = GcDb::new(js_db);

        let inner = EvmDbRefInner { state, db };
        let this = Self { inner: Rc::new(inner) };
        let guard = EvmDbGuard { _state_guard: state_guard, _db_guard: db_guard };
        (this, guard)
    }

    fn read_basic(&self, address: JsValue, ctx: &mut Context) -> JsResult<Option<AccountInfo>> {
        let buf = bytes_from_value(address, ctx)?;
        let address = bytes_to_address(&buf);
        if let acc @ Some(_) = self.inner.state.get_account(&address) {
            return Ok(acc);
        }

        let res = self.inner.db.0.with_inner(|db| db.basic_ref(address));
        match res {
            Some(Ok(maybe_acc)) => Ok(maybe_acc),
            _ => Err(JsError::from_native(
                JsNativeError::error()
                    .with_message(format!("Failed to read address {address:?} from database",)),
            )),
        }
    }

    fn read_code(&self, address: JsValue, ctx: &mut Context) -> JsResult<JsUint8Array> {
        let acc = self.read_basic(address, ctx)?;
        let code_hash = acc.as_ref().map(|acc| acc.code_hash).unwrap_or(KECCAK_EMPTY);
        if code_hash == KECCAK_EMPTY {
            return JsUint8Array::from_iter(core::iter::empty(), ctx);
        }

        if let Some(bytecode) = acc.as_ref().and_then(|acc| acc.code.as_ref()) {
            return to_uint8_array(bytecode.original_bytes().to_vec(), ctx);
        }

        let Some(Ok(bytecode)) = self.inner.db.0.with_inner(|db| db.code_by_hash_ref(code_hash))
        else {
            return Err(JsError::from_native(
                JsNativeError::error()
                    .with_message(format!("Failed to read code hash {code_hash:?} from database")),
            ));
        };

        to_uint8_array(bytecode.original_bytes().to_vec(), ctx)
    }

    fn read_state(
        &self,
        address: JsValue,
        slot: JsValue,
        ctx: &mut Context,
    ) -> JsResult<JsUint8Array> {
        let buf = bytes_from_value(address, ctx)?;
        let address = bytes_to_address(&buf);

        let buf = bytes_from_value(slot, ctx)?;
        let slot = bytes_to_b256(&buf);

        let res = self.inner.db.0.with_inner(|db| db.storage_ref(address, slot.into()));

        let value = match res {
            Some(Ok(value)) => value,
            _ => {
                return Err(JsError::from_native(JsNativeError::error().with_message(format!(
                    "Failed to read state for {address:?} at {slot:?} from database",
                ))))
            }
        };
        to_uint8_array(B256::from(value), ctx)
    }

    pub(crate) fn into_js_object(self, ctx: &mut Context) -> JsResult<JsObject> {
        let obj = JsObject::with_object_proto(ctx.intrinsics());
        let exists = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, db, ctx| {
                    let val = args.get_or_undefined(0).clone();
                    let acc = db.read_basic(val, ctx)?;
                    let exists = acc.is_some();
                    Ok(JsValue::from(exists))
                },
                self.clone(),
            ),
        )
        .length(1)
        .build();

        let get_balance = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, db, ctx| {
                    let val = args.get_or_undefined(0).clone();
                    let acc = db.read_basic(val, ctx)?;
                    let balance = acc.map(|acc| acc.balance).unwrap_or_default();
                    to_bigint(balance)
                },
                self.clone(),
            ),
        )
        .length(1)
        .build();

        let get_nonce = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, db, ctx| {
                    let val = args.get_or_undefined(0).clone();
                    let acc = db.read_basic(val, ctx)?;
                    let nonce = acc.map(|acc| acc.nonce).unwrap_or_default();
                    Ok(JsValue::from(nonce))
                },
                self.clone(),
            ),
        )
        .length(1)
        .build();

        let get_code = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, db, ctx| {
                    let val = args.get_or_undefined(0).clone();
                    Ok(db.read_code(val, ctx)?.into())
                },
                self.clone(),
            ),
        )
        .length(1)
        .build();

        let get_state = FunctionObjectBuilder::new(
            ctx.realm(),
            NativeFunction::from_copy_closure_with_captures(
                move |_this, args, db, ctx| {
                    let addr = args.get_or_undefined(0).clone();
                    let slot = args.get_or_undefined(1).clone();
                    Ok(db.read_state(addr, slot, ctx)?.into())
                },
                self,
            ),
        )
        .length(2)
        .build();

        obj.set(js_string!("getBalance"), get_balance, false, ctx)?;
        obj.set(js_string!("getNonce"), get_nonce, false, ctx)?;
        obj.set(js_string!("getCode"), get_code, false, ctx)?;
        obj.set(js_string!("getState"), get_state, false, ctx)?;
        obj.set(js_string!("exists"), exists, false, ctx)?;
        Ok(obj)
    }
}

impl Finalize for EvmDbRef {}

unsafe impl Trace for EvmDbRef {
    empty_trace!();
}

/// DB is the object that allows the js inspector to interact with the database.
struct EvmDbRefInner {
    state: StateRef,
    db: GcDb<Box<dyn DatabaseRef<Error = StringError> + 'static>>,
}

/// Guard the inner references, once this value is dropped the inner reference is also removed.
///
/// This ensures that the guards are dropped within the lifetime of the borrowed values.
#[must_use]
pub(crate) struct EvmDbGuard<'a, 'b> {
    _state_guard: GcGuard<'a, EvmState>,
    _db_guard: GcGuard<'b, Box<dyn DatabaseRef<Error = StringError> + 'static>>,
}

/// A wrapper Database for the JS context.
///
/// Wraps a `&mut DB: Database` in a `RefCell` so that the `&self` methods of
/// [`DatabaseRef`] can drive the underlying `Database` (which requires `&mut self`).
pub(crate) struct JsDb<'a, DB>(RefCell<&'a mut DB>);

#[derive(Clone, Debug)]
pub(crate) struct StringError(pub String);

impl core::error::Error for StringError {}
impl DBErrorMarker for StringError {}

impl core::fmt::Display for StringError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for StringError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl<DB> DatabaseRef for JsDb<'_, DB>
where
    DB: Database,
    DB::Error: core::fmt::Display,
{
    type Error = StringError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.borrow_mut().basic(address).map_err(|e| e.to_string().into())
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.borrow_mut().code_by_hash(code_hash).map_err(|e| e.to_string().into())
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.0.borrow_mut().storage(address, index).map_err(|e| e.to_string().into())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.0.borrow_mut().block_hash(number).map_err(|e| e.to_string().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing::js::builtins::{json_stringify, register_builtins, to_serde_value};
    use boa_engine::Source;
    use revm::{database::CacheDB, database_interface::EmptyDB};

    #[test]
    fn test_contract() {
        let mut ctx = Context::default();
        let contract = Contract {
            caller: Address::ZERO,
            contract: Address::ZERO,
            value: U256::from(1337u64),
            input: vec![0x01, 0x02, 0x03].into(),
        };
        register_builtins(&mut ctx).unwrap();

        let stack = Stack::new();
        let (stack_ref, _stack_guard) = StackRef::new_owned(stack);
        let mem = MemorySnapshot::default();
        let (mem_ref, _mem_guard) = MemoryRef::new_owned(mem);
        let step = StepLog {
            stack: stack_ref,
            op: OpObj(0),
            memory: mem_ref,
            pc: 0,
            gas_remaining: 0,
            cost: 0,
            depth: 0,
            refund: 0,
            error: None,
            contract: contract.clone(),
        };
        let reusable_step = ReusableStepLog::new(&mut ctx).unwrap();
        reusable_step.update(step);

        let s = "({
                caller: function(log) { return log.contract.getCaller(); },
                value: function(log) { return log.contract.getValue(); },
                address: function(log) { return log.contract.getAddress(); },
                input: function(log) { return log.contract.getInput(); }
        })";

        let log_arg = reusable_step.value();
        let eval_obj = ctx.eval(Source::from_bytes(s)).unwrap();
        let call = eval_obj.as_object().unwrap().get(js_string!("caller"), &mut ctx).unwrap();
        let res = call
            .as_callable()
            .unwrap()
            .call(&JsValue::undefined(), core::slice::from_ref(&log_arg), &mut ctx)
            .unwrap();
        assert!(res.is_object());
        let obj = res.as_object().unwrap();
        let array_buf = JsUint8Array::from_object(obj.clone());
        assert!(array_buf.is_ok());

        let get_address =
            eval_obj.as_object().unwrap().get(js_string!("address"), &mut ctx).unwrap();
        let res = get_address
            .as_callable()
            .unwrap()
            .call(&JsValue::undefined(), core::slice::from_ref(&log_arg), &mut ctx)
            .unwrap();
        assert!(res.is_object());

        let buf = bytes_from_value(res, &mut ctx).unwrap();
        assert_eq!(buf, contract.contract.as_slice());

        let call = eval_obj.as_object().unwrap().get(js_string!("value"), &mut ctx).unwrap();
        let res = call
            .as_callable()
            .unwrap()
            .call(&JsValue::undefined(), core::slice::from_ref(&log_arg), &mut ctx)
            .unwrap();
        assert_eq!(
            res.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            contract.value.to_string()
        );

        let call = eval_obj.as_object().unwrap().get(js_string!("input"), &mut ctx).unwrap();
        let res =
            call.as_callable().unwrap().call(&JsValue::undefined(), &[log_arg], &mut ctx).unwrap();

        let buf = bytes_from_value(res, &mut ctx).unwrap();
        assert_eq!(buf, contract.input);
    }

    #[test]
    fn test_evm_db_gc() {
        let mut context = Context::default();

        let result = context
            .eval(Source::from_bytes(
                "(
                    function(db, addr) {return db.exists(addr) }
            )
        "
                .to_string()
                .as_bytes(),
            ))
            .unwrap();
        assert!(result.is_callable());

        let f = result.as_callable().unwrap();

        let mut db = CacheDB::new(EmptyDB::new());
        let state = EvmState::default();
        let reusable_db = ReusableEvmDb::new(&mut context).unwrap();
        {
            let (db, guard) = EvmDbRef::new(&state, &mut db);
            let addr = Address::default();
            let addr = JsValue::from(js_string!(addr.to_string()));
            reusable_db.update(db);
            let db = reusable_db.value();
            let res = f.call(&result, &[db.clone(), addr.clone()], &mut context).unwrap();
            assert!(!res.as_boolean().unwrap());

            // drop the db which also drops any GC values
            drop(guard);
            let res = f.call(&result, &[db.clone(), addr.clone()], &mut context);
            assert!(res.is_err());
        }
        let addr = Address::default();
        db.insert_account_info(addr, Default::default());

        {
            let (db, guard) = EvmDbRef::new(&state, &mut db);
            let addr = JsValue::from(js_string!(addr.to_string()));
            reusable_db.update(db);
            let db = reusable_db.value();
            let res = f.call(&result, &[db.clone(), addr.clone()], &mut context).unwrap();

            // account exists
            assert!(res.as_boolean().unwrap());

            // drop the db which also drops any GC values
            drop(guard);
            let res = f.call(&result, &[db.clone(), addr.clone()], &mut context);
            assert!(res.is_err());
        }
    }

    #[test]
    fn test_evm_db_gc_captures() {
        let mut context = Context::default();

        let res = context
            .eval(Source::from_bytes(
                r"({
                 setup: function(db) {this.db = db;},
                 result: function(addr) {return this.db.exists(addr) }
            })
        "
                .to_string()
                .as_bytes(),
            ))
            .unwrap();

        let obj = res.as_object().unwrap();

        let result_fn = obj.get(js_string!("result"), &mut context).unwrap().as_object().unwrap();
        let setup_fn = obj.get(js_string!("setup"), &mut context).unwrap().as_object().unwrap();

        let mut db = CacheDB::new(EmptyDB::new());
        let state = EvmState::default();
        {
            let (db_ref, guard) = EvmDbRef::new(&state, &mut db);
            let reusable_db = ReusableEvmDb::new(&mut context).unwrap();
            reusable_db.update(db_ref);
            let _res =
                setup_fn.call(&(obj.clone().into()), &[reusable_db.value()], &mut context).unwrap();
            assert!(obj.get(js_string!("db"), &mut context).unwrap().is_object());

            let addr = Address::default();
            let addr = JsValue::from(js_string!(addr.to_string()));
            let res = result_fn
                .call(&(obj.clone().into()), core::slice::from_ref(&addr), &mut context)
                .unwrap();
            assert!(!res.as_boolean().unwrap());

            // drop the guard which also drops any GC values
            drop(guard);
            let res = result_fn.call(&(obj.clone().into()), &[addr], &mut context);
            assert!(res.is_err());
        }
    }

    #[test]
    fn test_big_int() {
        let mut context = Context::default();
        register_builtins(&mut context).unwrap();

        let eval = context
            .eval(Source::from_bytes(
                r#"({data: [], fault: function(log) {}, step: function(log) { this.data.push({ value: log.stack.peek(2) }) }, result: function() { return this.data; }})"#
                .to_string()
                .as_bytes(),
            ))
            .unwrap();

        let obj = eval.as_object().unwrap();

        let result_fn = obj.get(js_string!("result"), &mut context).unwrap().as_object().unwrap();
        let step_fn = obj.get(js_string!("step"), &mut context).unwrap().as_object().unwrap();

        let mut stack = Stack::new();
        let _ = stack.push(U256::from(35000));
        let _ = stack.push(U256::from(35000));
        let _ = stack.push(U256::from(35000));
        let (stack_ref, _stack_guard) = StackRef::new_owned(stack);
        let mem = MemorySnapshot::default();
        let (mem_ref, _mem_guard) = MemoryRef::new_owned(mem);

        let step = StepLog {
            stack: stack_ref,
            op: OpObj(0),
            memory: mem_ref,
            pc: 0,
            gas_remaining: 0,
            cost: 0,
            depth: 0,
            refund: 0,
            error: None,
            contract: Default::default(),
        };

        let reusable_step = ReusableStepLog::new(&mut context).unwrap();
        reusable_step.update(step);

        let _ = step_fn.call(&eval, &[reusable_step.value()], &mut context).unwrap();

        let res = result_fn.call(&eval, &[], &mut context).unwrap();
        let val = json_stringify(res.clone(), &mut context).unwrap().to_std_string().unwrap();
        assert_eq!(val, r#"[{"value":"35000"}]"#);

        let val = to_serde_value(res, &mut context).unwrap();
        assert!(val.is_array());
        let s = val.to_string();
        assert_eq!(s, r#"[{"value":"35000"}]"#);
    }

    #[test]
    fn test_object_functions() {
        let mut context = Context::default();
        register_builtins(&mut context).unwrap();

        let eval = context
            .eval(Source::from_bytes(
                r#"(
    {
        retVal: [],
        callStack: [],
        byte2Hex: function (byte) {
            if (byte < 0x10) return "0" + byte.toString(16);
            return byte.toString(16);
        },
        array2Hex: function (arr) {
            var retVal = "";
            for (var i = 0; i < arr.length; i++) retVal += this.byte2Hex(arr[i]);
            return retVal;
        },
        getAddr: function (log) {
            return this.array2Hex(log.contract.getAddress());
        },
        step: function (log, db) {
            var opcode = log.op.toNumber();
            if (opcode == 0x54) {
                this.retVal.push(this.getAddr(log) + ":" + log.stack.peek(0).toString(16));
            }
            if (opcode == 0x55)
                this.retVal.push(
                    this.getAddr(log) +
                        ":" +
                        log.stack.peek(0).toString(16) +
                        ";" +
                        log.stack.peek(1).toString(16)
                );
        },
        fault: function (log, db) {
            this.retVal.push("FAULT: ");
        },
        result: function (ctx, db) {
            return this.retVal;
        },
   }
)"#
                .to_string()
                .as_bytes(),
            ))
            .unwrap();

        let obj = eval.as_object().unwrap();

        let result_fn = obj.get(js_string!("result"), &mut context).unwrap().as_object().unwrap();
        let step_fn = obj.get(js_string!("step"), &mut context).unwrap().as_object().unwrap();

        let mut stack = Stack::new();
        let _ = stack.push(U256::from(35000));
        let _ = stack.push(U256::from(35000));
        let _ = stack.push(U256::from(35000));
        let (stack_ref, _stack_guard) = StackRef::new_owned(stack);
        let mem = MemorySnapshot::default();
        let (mem_ref, _mem_guard) = MemoryRef::new_owned(mem);

        let step = StepLog {
            stack: stack_ref,
            op: OpObj(85),
            memory: mem_ref,
            pc: 0,
            gas_remaining: 0,
            cost: 0,
            depth: 0,
            refund: 0,
            error: None,
            contract: Default::default(),
        };

        let reusable_step = ReusableStepLog::new(&mut context).unwrap();
        reusable_step.update(step);

        let _ = step_fn.call(&eval, &[reusable_step.value()], &mut context).unwrap();

        let res = result_fn.call(&eval, &[], &mut context).unwrap();
        let val = json_stringify(res.clone(), &mut context).unwrap().to_std_string().unwrap();
        assert_eq!(val, r#"["0000000000000000000000000000000000000000:88b8;88b8"]"#);
    }
}
