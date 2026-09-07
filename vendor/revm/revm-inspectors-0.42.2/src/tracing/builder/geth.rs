//! Geth trace builder
use crate::tracing::{
    types::{CallKind, CallTraceNode, CallTraceStepStackItem},
    utils::load_account_code,
};
use alloc::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    format, vec,
    vec::Vec,
};
use alloy_primitives::{
    map::{Entry, HashMap},
    Address, Bytes, B256, U256,
};
use alloy_rpc_types_trace::geth::{
    erc7562::{AccessedSlots, CallFrameType, ContractSize, Erc7562Config, Erc7562Frame},
    AccountChangeKind, AccountState, CallConfig, CallFrame, DefaultFrame, DiffMode,
    GethDefaultTracingOptions, PreStateConfig, PreStateFrame, PreStateMode, StructLog,
};
use revm::{
    bytecode::opcode,
    context_interface::result::{HaltReasonTr, ResultAndState},
    primitives::KECCAK_EMPTY,
    state::{AccountInfo, EvmState},
    DatabaseRef,
};

/// A type for creating geth style traces
#[derive(Clone, Debug)]
pub struct GethTraceBuilder<'a> {
    /// Recorded trace nodes.
    nodes: Cow<'a, [CallTraceNode]>,
}

impl GethTraceBuilder<'static> {
    /// Returns a new instance of the builder from [`Cow::Owned`]
    pub fn new(nodes: Vec<CallTraceNode>) -> GethTraceBuilder<'static> {
        Self { nodes: Cow::Owned(nodes) }
    }
}

impl<'a> GethTraceBuilder<'a> {
    /// Returns a new instance of the builder from [`Cow::Borrowed`]
    pub fn new_borrowed(nodes: &'a [CallTraceNode]) -> GethTraceBuilder<'a> {
        Self { nodes: Cow::Borrowed(nodes) }
    }

    /// Consumes the builder and returns the recorded trace nodes.
    pub fn to_owned(self) -> Vec<CallTraceNode> {
        self.nodes.into_owned()
    }

    /// Returns the sum of all steps in the recorded node traces.
    fn trace_step_count(&self) -> usize {
        self.nodes.iter().map(|node| node.trace.steps.len()).sum()
    }

    /// Fill in the geth trace with all steps of the trace and its children traces in the order they
    /// appear in the transaction.
    fn fill_geth_trace(
        &self,
        main_trace_node: &CallTraceNode,
        opts: &GethDefaultTracingOptions,
        storage: &mut HashMap<Address, BTreeMap<B256, B256>>,
        struct_logs: &mut Vec<StructLog>,
    ) {
        // A stack with all the steps of the trace and all its children's steps.
        // This is used to process the steps in the order they appear in the transactions.
        // Steps are grouped by their Call Trace Node, in order to process them all in the order
        // they appear in the transaction, we need to process steps of call nodes when they appear.
        // When we find a call step, we push all the steps of the child trace on the stack, so they
        // are processed next. The very next step is the last item on the stack
        let mut step_stack = VecDeque::with_capacity(main_trace_node.trace.steps.len());

        main_trace_node.push_steps_on_stack(&mut step_stack);

        // Iterate over the steps inside the given trace
        while let Some(CallTraceStepStackItem { trace_node, step, call_child_id }) =
            step_stack.pop_back()
        {
            // We increment the depth by one because steps that are part of call at depth N should
            // have depth N + 1. For example, steps inside of a top-level call should
            // have depth 1.
            let mut log = step.convert_to_geth_struct_log(opts, trace_node.trace.depth as u64 + 1);

            // Fill in memory and storage depending on the options
            if opts.is_storage_enabled() {
                let contract_storage = storage.entry(trace_node.execution_address()).or_default();
                if let Some(change) = &step.storage_change {
                    contract_storage.insert(change.key.into(), change.value.into());
                }

                if matches!(step.op.get(), opcode::SLOAD | opcode::SSTORE) {
                    log.storage = Some(contract_storage.clone());
                }
            }

            if opts.is_return_data_enabled() {
                log.return_data = Some(step.returndata.clone());
            }

            // Add step to geth trace
            struct_logs.push(log);

            // If the step is a call, we first push all the steps of the child trace on the stack,
            // so they are processed next
            if let Some(call_child_id) = call_child_id {
                let child_trace = &self.nodes[call_child_id];
                child_trace.push_steps_on_stack(&mut step_stack);
            }
        }
    }

    /// Generate a geth-style trace e.g. for `debug_traceTransaction`
    ///
    /// This expects the gas used and return value for the
    /// [[revm::context::result::ExecutionResult]] of the executed
    /// transaction.
    pub fn geth_traces(
        &self,
        receipt_gas_used: u64,
        return_value: Bytes,
        opts: GethDefaultTracingOptions,
    ) -> DefaultFrame {
        if self.nodes.is_empty() {
            return Default::default();
        }
        // Fetch top-level trace
        let main_trace_node = &self.nodes[0];
        let main_trace = &main_trace_node.trace;

        let mut struct_logs = Vec::with_capacity(self.trace_step_count());
        let mut storage = HashMap::default();
        self.fill_geth_trace(main_trace_node, &opts, &mut storage, &mut struct_logs);

        #[allow(clippy::needless_update)]
        DefaultFrame {
            // If the top-level trace succeeded, then it was a success
            failed: !main_trace.success,
            gas: receipt_gas_used,
            return_value,
            struct_logs,
            ..Default::default()
        }
    }

    /// Generate a geth-style traces for the call tracer.
    ///
    /// This decodes all call frames from the recorded traces.
    ///
    /// This expects the gas used and return value for the
    /// [revm::context::result::ExecutionResult] of the executed
    /// transaction.
    pub fn geth_call_traces(&self, opts: CallConfig, gas_used: u64) -> CallFrame {
        if self.nodes.is_empty() {
            return Default::default();
        }

        let include_logs = opts.with_log.unwrap_or_default();
        // first fill up the root
        let main_trace_node = &self.nodes[0];
        let mut root_call_frame = main_trace_node.geth_empty_call_frame(include_logs);
        root_call_frame.gas_used = U256::from(gas_used);

        // selfdestructs are not recorded as individual call traces but are derived from
        // the call trace and are added as additional `CallFrame` objects to the parent call
        if let Some(selfdestruct) = main_trace_node.geth_selfdestruct_call_trace() {
            root_call_frame.calls.push(selfdestruct);
        }

        if opts.only_top_call.unwrap_or_default() {
            return root_call_frame;
        }

        // fill all the call frames in the root call frame with the recorded traces.
        // traces are identified by their index in the arena
        // so we can populate the call frame tree by walking up the call tree
        let mut call_frames = Vec::with_capacity(self.nodes.len());
        call_frames.push((0, root_call_frame));

        for (idx, trace) in self.nodes.iter().enumerate().skip(1) {
            // include logs only if call and all its parents were successful
            let include_logs = include_logs && !self.call_or_parent_failed(trace);
            call_frames.push((idx, trace.geth_empty_call_frame(include_logs)));

            // selfdestructs are not recorded as individual call traces but are derived from
            // the call trace and are added as additional `CallFrame` objects
            // becoming the first child of the derived call
            if let Some(selfdestruct) = trace.geth_selfdestruct_call_trace() {
                call_frames.last_mut().expect("not empty").1.calls.push(selfdestruct);
            }
        }

        // pop the _children_ calls frame and move it to the parent
        // this will roll up the child frames to their parent; this works because `child idx >
        // parent idx`
        loop {
            let (idx, call) = call_frames.pop().expect("call frames not empty");
            let node = &self.nodes[idx];
            if let Some(parent) = node.parent {
                let parent_frame = &mut call_frames[parent];
                // we need to ensure that calls are in order they are called: the last child node is
                // the last call, but since we walk up the tree, we need to always
                // insert at position 0
                parent_frame.1.calls.insert(0, call);
            } else {
                debug_assert!(call_frames.is_empty(), "only one root node has no parent");
                return call;
            }
        }
    }

    /// Returns true if the given trace or any of its parents failed.
    fn call_or_parent_failed(&self, node: &CallTraceNode) -> bool {
        if node.trace.is_error() {
            return true;
        }

        let mut parent_idx = node.parent;
        while let Some(idx) = parent_idx {
            let next = &self.nodes[idx];
            if next.trace.is_error() {
                return true;
            }

            parent_idx = next.parent;
        }
        false
    }

    ///  Returns the accounts necessary for transaction execution.
    ///
    /// The prestate mode returns the accounts necessary to execute a given transaction.
    /// diff_mode returns the differences between the transaction's pre and post-state.
    ///
    /// * `state` - The state post-transaction execution.
    /// * `diff_mode` - if prestate is in diff or prestate mode.
    /// * `db` - The database to fetch state pre-transaction execution.
    pub fn geth_prestate_traces<DB: DatabaseRef>(
        &self,
        ResultAndState { state, .. }: &ResultAndState<impl HaltReasonTr>,
        prestate_config: &PreStateConfig,
        db: DB,
    ) -> Result<PreStateFrame, DB::Error> {
        let code_enabled = prestate_config.code_enabled();
        let storage_enabled = prestate_config.storage_enabled();
        if prestate_config.is_diff_mode() {
            self.geth_prestate_diff_traces(state, db, code_enabled, storage_enabled)
        } else {
            self.geth_prestate_pre_traces(state, db, code_enabled, storage_enabled)
        }
    }

    fn geth_prestate_pre_traces<DB: DatabaseRef>(
        &self,
        state: &EvmState,
        db: DB,
        code_enabled: bool,
        storage_enabled: bool,
    ) -> Result<PreStateFrame, DB::Error> {
        let account_diffs = state.iter().map(|(addr, acc)| (*addr, acc));
        let mut prestate = PreStateMode::default();

        // we only want changed accounts for things like balance changes etc
        for (addr, changed_acc) in account_diffs {
            let db_acc = db.basic_ref(addr)?.unwrap_or_default();
            let code = code_enabled.then(|| load_account_code(&db, &db_acc)).flatten();
            let mut acc_state = AccountState::from_account_info(db_acc.nonce, db_acc.balance, code);

            // insert the original value of all modified storage slots
            if storage_enabled {
                for (key, slot) in changed_acc.storage.iter() {
                    acc_state.storage.insert((*key).into(), slot.original_value.into());
                }
            }

            prestate.0.insert(addr, acc_state);
        }

        Ok(PreStateFrame::Default(prestate))
    }

    fn geth_prestate_diff_traces<DB: DatabaseRef>(
        &self,
        state: &EvmState,
        db: DB,
        code_enabled: bool,
        storage_enabled: bool,
    ) -> Result<PreStateFrame, DB::Error> {
        let account_diffs = state.iter().map(|(addr, acc)| (*addr, acc));
        let mut state_diff = DiffMode::default();
        let mut account_change_kinds =
            HashMap::with_capacity_and_hasher(account_diffs.len(), Default::default());
        for (addr, changed_acc) in account_diffs {
            let db_acc = db.basic_ref(addr)?.unwrap_or_default();

            let pre_code = code_enabled.then(|| load_account_code(&db, &db_acc)).flatten();

            let mut post_state = AccountState::from_account_info(
                changed_acc.info.nonce,
                changed_acc.info.balance,
                code_enabled
                    .then(|| {
                        if changed_acc.info.code_hash == db_acc.code_hash {
                            pre_code.clone()
                        } else {
                            // Note: the changed account from the state output always holds the code
                            changed_acc.info.code.as_ref().map(|code| code.original_bytes())
                        }
                    })
                    .flatten(),
            );

            let mut pre_state =
                AccountState::from_account_info(db_acc.nonce, db_acc.balance, pre_code);

            // handle storage changes
            if storage_enabled {
                for (key, slot) in changed_acc.storage.iter().filter(|(_, slot)| slot.is_changed())
                {
                    pre_state.storage.insert((*key).into(), slot.original_value.into());
                    post_state.storage.insert((*key).into(), slot.present_value.into());
                }
            }

            state_diff.pre.insert(addr, pre_state);

            // determine the change type
            // if the account was created _and_ not empty we need to treat it as modified,
            // so that it is retained later in `diff_traces`
            // See <https://etherscan.io/tx/0x391f4b6a382d3bcc3120adc2ea8c62003e604e487d97281129156fd284a1a89d>
            // <https://github.com/paradigmxyz/reth/issues/19703#issuecomment-3527067849>
            let pre_change = if changed_acc.is_created() && account_was_empty(&db_acc) {
                AccountChangeKind::Create
            } else {
                AccountChangeKind::Modify
            };
            let post_change = if changed_acc.is_selfdestructed() {
                AccountChangeKind::SelfDestruct
            } else {
                AccountChangeKind::Modify
            };

            account_change_kinds.insert(addr, (pre_change, post_change));

            // Don't insert selfdestructed accounts into post state
            if !changed_acc.is_selfdestructed() {
                state_diff.post.insert(addr, post_state);
            }
        }

        // ensure we're only keeping changed entries
        state_diff.retain_changed().remove_zero_storage_values();

        self.diff_traces(&mut state_diff.pre, &mut state_diff.post, account_change_kinds);
        Ok(PreStateFrame::Diff(state_diff))
    }

    /// Returns the difference between the pre and post state of the transaction depending on the
    /// kind of changes of that account (pre,post)
    fn diff_traces(
        &self,
        pre: &mut BTreeMap<Address, AccountState>,
        post: &mut BTreeMap<Address, AccountState>,
        change_type: HashMap<Address, (AccountChangeKind, AccountChangeKind)>,
    ) {
        post.retain(|addr, post_state| {
            // Don't keep destroyed accounts in the post state
            if change_type.get(addr).map(|ty| ty.1.is_selfdestruct()).unwrap_or(false) {
                return false;
            }
            if let Some(pre_state) = pre.get(addr) {
                // remove any unchanged account info
                post_state.remove_matching_account_info(pre_state);
            }

            true
        });

        // Don't keep created accounts the pre state
        pre.retain(|addr, _pre_state| {
            // only keep accounts that are not created
            change_type.get(addr).map(|ty| !ty.0.is_created()).unwrap_or(true)
        });
    }

    /// Traces ERC-7562 calls using the call tracer.
    pub fn geth_erc7562_traces<DB: DatabaseRef>(
        &self,
        opts: Erc7562Config,
        gas_used: u64,
        db: DB,
    ) -> Erc7562Frame {
        if self.nodes.is_empty() {
            return Default::default();
        }

        let include_logs = opts.with_log.unwrap_or_default();
        let call_config = CallConfig { only_top_call: None, with_log: Some(include_logs) };

        let mut top_call = Some(self.geth_call_traces(call_config, gas_used));

        let mut frames: Vec<(usize, Erc7562Frame)> = Vec::with_capacity(self.nodes.len());

        for (idx, node) in self.nodes.iter().enumerate() {
            let trace = &node.trace;

            let mut accessed_slots = AccessedSlots::default();
            let mut used_opcodes = HashMap::default();
            let mut contract_size = HashMap::default();
            let mut ext_code_access_info = Vec::new();
            let mut keccak = Vec::new();
            let mut out_of_gas = false;

            for step in &trace.steps {
                let op = step.op.get();

                // Skip if opcode is ignored
                if opts.ignored_opcodes.contains(&op) {
                    continue;
                }

                // Count used opcodes
                *used_opcodes.entry(op).or_insert(0) += 1;

                // Accessed storage slots
                match op {
                    opcode::SLOAD => {
                        if let Some(stack) = &step.stack {
                            if let Some(slot) = stack.get(stack.len().saturating_sub(1)) {
                                let slot: B256 = (*slot).into();
                                let already_read = accessed_slots.reads.contains_key(&slot);
                                let already_written = accessed_slots.writes.contains_key(&slot);
                                if !already_read && !already_written {
                                    if let Some(change) = &step.storage_change {
                                        let value: B256 = change.value.into();
                                        accessed_slots.reads.entry(slot).or_default().push(value);
                                    }
                                }
                            }
                        }
                    }
                    opcode::SSTORE => {
                        if let Some(stack) = &step.stack {
                            if let Some(slot) = stack.get(stack.len().saturating_sub(1)) {
                                let slot: B256 = (*slot).into();
                                *accessed_slots.writes.entry(slot).or_insert(0) += 1;
                            }
                        }
                    }
                    opcode::TLOAD => {
                        if let Some(stack) = &step.stack {
                            if let Some(slot) = stack.get(stack.len().saturating_sub(1)) {
                                let slot: B256 = (*slot).into();
                                *accessed_slots.transient_reads.entry(slot).or_insert(0) += 1;
                            }
                        }
                    }
                    opcode::TSTORE => {
                        if let Some(stack) = &step.stack {
                            if let Some(slot) = stack.get(stack.len().saturating_sub(1)) {
                                let slot: B256 = (*slot).into();
                                *accessed_slots.transient_writes.entry(slot).or_insert(0) += 1;
                            }
                        }
                    }
                    _ => {}
                }

                if let Some(status) = &step.status {
                    if *status == revm::interpreter::InstructionResult::OutOfGas {
                        out_of_gas = true;
                    }
                }

                if matches!(op, opcode::EXTCODESIZE | opcode::EXTCODECOPY | opcode::EXTCODEHASH) {
                    if let Some(stack) = &step.stack {
                        if let Some(item) = stack.get(stack.len().saturating_sub(1)) {
                            let address = Address::from_word((*item).into());
                            ext_code_access_info.push(format!("{address:?}"));
                            if let Entry::Vacant(e) = contract_size.entry(address) {
                                if let Ok(Some(account)) = db.basic_ref(address) {
                                    if account.code_hash != KECCAK_EMPTY {
                                        if let Ok(bytecode) = db.code_by_hash_ref(account.code_hash)
                                        {
                                            e.insert(ContractSize {
                                                contract_size: bytecode.original_bytes().len()
                                                    as u64,
                                                opcode: op,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // KECCAK preimages from returndata
                if op == opcode::KECCAK256 && !out_of_gas {
                    if let (Some(stack), Some(memory)) = (&step.stack, &step.memory) {
                        if stack.len() >= 2 {
                            let offset = stack[stack.len() - 1];
                            let len = stack[stack.len() - 2];
                            if let (Ok(offset), Ok(len)) =
                                (usize::try_from(offset), usize::try_from(len))
                            {
                                let mut data = vec![0; len];
                                if offset < memory.0.len() {
                                    let end = (offset + len).min(memory.0.len());
                                    let copy_len = end - offset;
                                    data[..copy_len].copy_from_slice(&memory.0[offset..end]);
                                }
                                keccak.push(Bytes::from(data));
                            }
                        }
                    }
                }
            }

            let call_frame = if idx == 0 {
                top_call.take().unwrap()
            } else {
                let include_logs = include_logs && !self.call_or_parent_failed(node);
                self.nodes[idx].geth_empty_call_frame(include_logs)
            };

            let call_frame_type = Self::convert_call_kind(node.kind());

            frames.push((
                idx,
                Erc7562Frame {
                    call_frame_type,
                    from: call_frame.from,
                    gas: call_frame.gas.to(),
                    gas_used: call_frame.gas_used.to(),
                    to: call_frame.to,
                    input: call_frame.input,
                    output: call_frame.output,
                    error: call_frame.error,
                    revert_reason: call_frame.revert_reason,
                    logs: call_frame.logs,
                    value: call_frame.value,
                    accessed_slots,
                    ext_code_access_info,
                    used_opcodes,
                    contract_size,
                    out_of_gas,
                    keccak,
                    calls: vec![],
                },
            ));
        }

        // Assemble tree
        loop {
            let (idx, frame) = frames.pop().expect("call frames not empty");
            let node = &self.nodes[idx];
            if let Some(parent) = node.parent {
                let parent_frame = &mut frames[parent];
                parent_frame.1.calls.insert(0, frame);
            } else {
                debug_assert!(frames.is_empty(), "only one root node has no parent");
                return frame;
            }
        }
    }

    /// Converts a CallKind to a CallFrameType.
    pub fn convert_call_kind(kind: CallKind) -> CallFrameType {
        match kind {
            CallKind::Call => CallFrameType::Call,
            CallKind::CallCode => CallFrameType::CallCode,
            CallKind::DelegateCall => CallFrameType::DelegateCall,
            CallKind::StaticCall => CallFrameType::StaticCall,
            CallKind::Create => CallFrameType::Create,
            CallKind::Create2 => CallFrameType::Create2,
            CallKind::AuthCall => CallFrameType::Call,
        }
    }
}

fn account_was_empty(account: &AccountInfo) -> bool {
    account.balance.is_zero() && account.nonce == 0 && account.code_hash == KECCAK_EMPTY
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use revm::{
        database::CacheDB,
        database_interface::EmptyDB,
        state::{Account, AccountInfo},
    };

    #[test]
    fn prestate_diff_keeps_prefunded_created_accounts() {
        let mut state = EvmState::default();
        let prefunded_addr = address!("1000000000000000000000000000000000000001");
        let empty_addr = address!("2000000000000000000000000000000000000002");

        let mut prefunded_account = Account::default();
        prefunded_account.mark_created();
        prefunded_account.info.balance = U256::from(1);
        prefunded_account.info.nonce = 1;
        state.insert(prefunded_addr, prefunded_account);

        let mut empty_account = Account::default();
        empty_account.mark_created();
        empty_account.info.nonce = 1;
        state.insert(empty_addr, empty_account);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            prefunded_addr,
            AccountInfo { balance: U256::from(10), ..Default::default() },
        );

        let builder = GethTraceBuilder::new(Vec::new());
        let frame =
            builder.geth_prestate_diff_traces(&state, db, false, false).expect("diff frame");

        match frame {
            PreStateFrame::Diff(diff) => {
                assert!(
                    diff.pre.contains_key(&prefunded_addr),
                    "prefunded contract must remain in prestate diff"
                );
                assert!(
                    !diff.pre.contains_key(&empty_addr),
                    "contracts created on empty addresses are still filtered out"
                );
            }
            _ => panic!("expected diff prestate frame"),
        }
    }
}
