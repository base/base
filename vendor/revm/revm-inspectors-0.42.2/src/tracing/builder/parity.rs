use super::walker::CallTraceNodeWalkerBF;
use crate::tracing::{
    types::{CallTraceNode, CallTraceStep},
    utils::load_account_code,
    TracingInspectorConfig,
};
use alloc::{collections::VecDeque, string::ToString, vec, vec::Vec};
use alloy_primitives::{map::HashSet, Address, U256, U64};
use alloy_rpc_types_eth::TransactionInfo;
use alloy_rpc_types_trace::parity::*;
use core::iter::Peekable;
use revm::{
    context_interface::result::{ExecutionResult, HaltReasonTr, ResultAndState},
    primitives::{hardfork::SpecId, KECCAK_EMPTY},
    state::Account,
    DatabaseRef,
};

/// A type for creating parity style traces
///
/// Note: Parity style traces always ignore calls to precompiles.
#[derive(Clone, Debug)]
pub struct ParityTraceBuilder {
    /// Recorded trace nodes
    nodes: Vec<CallTraceNode>,
}

impl ParityTraceBuilder {
    /// Returns a new instance of the builder
    pub fn new(
        nodes: Vec<CallTraceNode>,
        _spec_id: Option<SpecId>,
        _config: TracingInspectorConfig,
    ) -> Self {
        Self { nodes }
    }

    /// Returns a list of all addresses that appeared as callers.
    pub fn callers(&self) -> HashSet<Address> {
        self.nodes.iter().map(|node| node.trace.caller).collect()
    }

    /// Manually set the gas used of the root trace.
    ///
    /// The root trace's gasUsed should mirror the actual gas used by the transaction.
    ///
    /// This allows setting it manually by consuming the execution result's gas for example.
    #[inline]
    pub fn set_transaction_gas_used(&mut self, gas_used: u64) {
        if let Some(node) = self.nodes.first_mut() {
            node.trace.gas_used = gas_used;
        }
    }

    /// Convenience function for [ParityTraceBuilder::set_transaction_gas_used] that consumes the
    /// type.
    #[inline]
    pub fn with_transaction_gas_used(mut self, gas_used: u64) -> Self {
        self.set_transaction_gas_used(gas_used);
        self
    }

    /// Returns the trace addresses of all call nodes in the set
    ///
    /// Each entry in the returned vector represents the [Self::trace_address] of the corresponding
    /// node in the nodes set.
    ///
    /// CAUTION: This also includes precompiles, which have an empty trace address.
    fn trace_addresses(&self) -> Vec<Vec<usize>> {
        let mut all_addresses = Vec::with_capacity(self.nodes.len());
        for idx in 0..self.nodes.len() {
            all_addresses.push(self.trace_address(idx));
        }
        all_addresses
    }

    /// Returns the `traceAddress` of the node in the arena
    ///
    /// The `traceAddress` field of all returned traces, gives the exact location in the call trace
    /// [index in root, index in first CALL, index in second CALL, …].
    ///
    /// # Panics
    ///
    /// if the `idx` does not belong to a node
    ///
    /// Note: if the call node of `idx` is a precompile, the returned trace address will be empty.
    fn trace_address(&self, idx: usize) -> Vec<usize> {
        if idx == 0 {
            // root call has empty traceAddress
            return vec![];
        }
        let mut graph = vec![];
        let mut node = &self.nodes[idx];
        if node.is_precompile() {
            return graph;
        }
        while let Some(parent) = node.parent {
            // the index of the child call in the arena
            let child_idx = node.idx;
            node = &self.nodes[parent];
            // find the index of the child call in the parent node
            let call_idx = node
                .children
                .iter()
                .position(|child| *child == child_idx)
                .expect("non precompile child call exists in parent");
            graph.push(call_idx);
        }
        graph.reverse();
        graph
    }

    /// Returns an iterator over all nodes to trace
    ///
    /// This excludes nodes that represent calls to precompiles.
    fn iter_traceable_nodes(&self) -> impl Iterator<Item = &CallTraceNode> {
        self.nodes.iter().filter(|node| !node.is_precompile())
    }

    /// Returns an iterator over all recorded traces  for `trace_transaction`
    pub fn into_localized_transaction_traces_iter(
        self,
        info: TransactionInfo,
    ) -> impl Iterator<Item = LocalizedTransactionTrace> {
        self.into_transaction_traces_iter().map(move |trace| {
            let TransactionInfo { hash, index, block_hash, block_number, .. } = info;
            LocalizedTransactionTrace {
                trace,
                transaction_position: index,
                transaction_hash: hash,
                block_number,
                block_hash,
            }
        })
    }

    /// Returns all recorded traces for `trace_transaction`
    pub fn into_localized_transaction_traces(
        self,
        info: TransactionInfo,
    ) -> Vec<LocalizedTransactionTrace> {
        self.into_localized_transaction_traces_iter(info).collect()
    }

    /// Consumes the inspector and returns the trace results according to the configured trace
    /// types.
    ///
    /// Warning: If `trace_types` contains [TraceType::StateDiff] the returned [StateDiff] will not
    /// be filled. Use [ParityTraceBuilder::into_trace_results_with_state] or
    /// [populate_state_diff] to populate the balance and nonce changes for the [StateDiff]
    /// using the [DatabaseRef].
    pub fn into_trace_results(
        self,
        res: &ExecutionResult<impl HaltReasonTr>,
        trace_types: &HashSet<TraceType>,
    ) -> TraceResults {
        let output = res.output().cloned().unwrap_or_default();

        let (trace, vm_trace, state_diff) = self.into_trace_type_traces(trace_types);

        TraceResults { output, trace: trace.unwrap_or_default(), vm_trace, state_diff }
    }

    /// Consumes the inspector and returns the trace results according to the configured trace
    /// types.
    ///
    /// This also takes the [DatabaseRef] to populate the balance and nonce changes for the
    /// [StateDiff].
    ///
    /// Note: this is considered a convenience method that takes the state map of
    /// [ResultAndState] after inspecting a transaction
    /// with the [TracingInspector](crate::tracing::TracingInspector).
    pub fn into_trace_results_with_state<DB: DatabaseRef>(
        self,
        res: &ResultAndState<impl HaltReasonTr>,
        trace_types: &HashSet<TraceType>,
        db: DB,
    ) -> Result<TraceResults, DB::Error> {
        let ResultAndState { ref result, ref state } = res;

        let breadth_first_addresses = if trace_types.contains(&TraceType::VmTrace) {
            CallTraceNodeWalkerBF::new(&self.nodes)
                .map(|node| node.trace.address)
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        let mut trace_res = self.into_trace_results(result, trace_types);

        // check the state diff case
        if let Some(ref mut state_diff) = trace_res.state_diff {
            populate_state_diff(state_diff, &db, state.iter())?;
        }

        // check the vm trace case
        if let Some(ref mut vm_trace) = trace_res.vm_trace {
            populate_vm_trace_bytecodes(&db, vm_trace, breadth_first_addresses)?;
        }

        Ok(trace_res)
    }

    /// Returns the tracing types that are configured in the set.
    ///
    /// Warning: if [TraceType::StateDiff] is provided this does __not__ fill the state diff, since
    /// this requires access to the account diffs.
    ///
    /// See [Self::into_trace_results_with_state] and [populate_state_diff].
    pub fn into_trace_type_traces(
        self,
        trace_types: &HashSet<TraceType>,
    ) -> (Option<Vec<TransactionTrace>>, Option<VmTrace>, Option<StateDiff>) {
        if trace_types.is_empty() || self.nodes.is_empty() {
            return (None, None, None);
        }

        let with_diff = trace_types.contains(&TraceType::StateDiff);

        // early return for StateDiff-only case
        if trace_types.len() == 1 && with_diff {
            return (None, None, Some(StateDiff::default()));
        }

        let vm_trace = trace_types.contains(&TraceType::VmTrace).then(|| self.vm_trace());

        let traces = trace_types.contains(&TraceType::Trace).then(|| self.transaction_traces());

        let diff = with_diff.then(StateDiff::default);

        (traces, vm_trace, diff)
    }

    /// Returns all the ordered [`TransactionTrace`], including selfdestructs.
    ///
    /// Selfdestructs appear as individual [`TransactionTrace`] instance but selfdestructs are
    /// tracked as metadata of the recorded nodes.
    fn transaction_traces(&self) -> Vec<TransactionTrace> {
        let mut traces = Vec::with_capacity(self.nodes.len());
        // Boolean marker to track if sorting for selfdestruct is needed
        let mut sorting_selfdestruct = false;

        for node in self.iter_traceable_nodes() {
            let trace_address = self.trace_address(node.idx);
            let trace = node.parity_transaction_trace(trace_address);
            traces.push(trace);

            if node.is_selfdestruct() {
                // selfdestructs are not recorded as individual call traces but are derived from
                // the call trace and are added as additional `TransactionTrace` objects in the
                // trace array
                let addr = {
                    let last = traces.last_mut().expect("exists");
                    let mut addr = Vec::with_capacity(last.trace_address.len() + 1);
                    addr.extend_from_slice(&last.trace_address);
                    addr.push(last.subtraces);
                    last.subtraces += 1;
                    addr
                };

                if let Some(trace) = node.parity_selfdestruct_trace(addr) {
                    traces.push(trace);
                    sorting_selfdestruct = true;
                }
            }
        }

        // Sort the traces only if a selfdestruct trace was encountered
        if sorting_selfdestruct {
            traces.sort_unstable_by(|a, b| a.trace_address.cmp(&b.trace_address));
        }
        traces
    }

    /// Returns an iterator over all recorded traces  for `trace_transaction`
    pub fn into_transaction_traces_iter(self) -> impl Iterator<Item = TransactionTrace> {
        let trace_addresses = self.trace_addresses();
        TransactionTraceIter {
            next_selfdestructs: Default::default(),
            iter: self
                .nodes
                .into_iter()
                .zip(trace_addresses)
                .filter(|(node, _)| !node.is_precompile())
                .map(|(node, trace_address)| (node.parity_transaction_trace(trace_address), node))
                .peekable(),
        }
    }

    /// Returns the raw traces of the transaction
    pub fn into_transaction_traces(self) -> Vec<TransactionTrace> {
        self.into_transaction_traces_iter().collect()
    }

    /// Returns the last recorded step
    #[inline]
    fn last_step(&self) -> Option<&CallTraceStep> {
        self.nodes.last().and_then(|node| node.trace.steps.last())
    }

    /// Returns true if the last recorded step is a STOP
    #[inline]
    fn is_last_step_stop_op(&self) -> bool {
        self.last_step().map(|step| step.is_stop()).unwrap_or(false)
    }

    /// Creates a VM trace by walking over `CallTraceNode`s
    ///
    /// does not have the code fields filled in
    pub fn vm_trace(&self) -> VmTrace {
        self.nodes.first().map(|node| self.make_vm_trace(node)).unwrap_or_default()
    }

    /// Returns a VM trace without the code filled in
    ///
    /// Iteratively creates a VM trace by traversing the recorded nodes in the arena
    fn make_vm_trace(&self, start: &CallTraceNode) -> VmTrace {
        let mut child_idx_stack = Vec::with_capacity(self.nodes.len());
        let mut sub_stack = VecDeque::with_capacity(self.nodes.len());

        let mut current = start;
        let mut child_idx: usize = 0;

        // finds the deepest nested calls of each call frame and fills them up bottom to top
        let instructions = 'outer: loop {
            match current.children.get(child_idx) {
                Some(child) => {
                    child_idx_stack.push(child_idx + 1);

                    child_idx = 0;
                    current = self.nodes.get(*child).expect("there should be a child");
                }
                None => {
                    let mut instructions = Vec::with_capacity(current.trace.steps.len());

                    for step in &current.trace.steps {
                        let maybe_sub_call = if step.is_call_like_op() {
                            sub_stack.pop_front().flatten()
                        } else {
                            None
                        };

                        if step.is_stop() && instructions.is_empty() && self.is_last_step_stop_op()
                        {
                            // This is a special case where there's a single STOP which is
                            // "optimised away", transfers for example
                            break 'outer instructions;
                        }

                        instructions.push(self.make_instruction(step, maybe_sub_call));
                    }

                    match current.parent {
                        Some(parent) => {
                            sub_stack.push_back(Some(VmTrace {
                                code: Default::default(),
                                ops: instructions,
                            }));

                            child_idx = child_idx_stack.pop().expect("there should be a child idx");

                            current = self.nodes.get(parent).expect("there should be a parent");
                        }
                        None => break instructions,
                    }
                }
            }
        };

        VmTrace { code: Default::default(), ops: instructions }
    }

    /// Creates a VM instruction from a [CallTraceStep] and a [VmTrace] for the subcall if there is
    /// one
    fn make_instruction(
        &self,
        step: &CallTraceStep,
        maybe_sub_call: Option<VmTrace>,
    ) -> VmInstruction {
        let maybe_storage = step.storage_change.as_ref().map(|storage_change| StorageDelta {
            key: storage_change.key,
            val: storage_change.value,
        });

        let maybe_memory = step
            .memory
            .as_ref()
            .map(|memory| MemoryDelta { off: memory.len(), data: memory.as_bytes().clone() });

        let maybe_execution = Some(VmExecutedOperation {
            used: step.gas_remaining,
            push: step.push_stack.clone().unwrap_or_default().into(),
            mem: maybe_memory,
            store: maybe_storage,
        });

        VmInstruction {
            pc: step.pc,
            cost: step.gas_cost,
            ex: maybe_execution,
            sub: maybe_sub_call,
            op: Some(step.op.to_string()),
            idx: None,
        }
    }
}

/// An iterator for [TransactionTrace]s
struct TransactionTraceIter<Iter: Iterator> {
    /// The iterator over all traces
    iter: Peekable<Iter>,
    /// The selfdestruct objects that are derived from the yielded traces.
    ///
    /// This is a stack because we need to yield them in the correct order.
    next_selfdestructs: Vec<TransactionTrace>,
}

impl<Iter> Iterator for TransactionTraceIter<Iter>
where
    Iter: Iterator<Item = (TransactionTrace, CallTraceNode)>,
{
    type Item = TransactionTrace;

    fn next(&mut self) -> Option<Self::Item> {
        // ensure the selfdestruct trace is emitted just at the ending of the same depth
        if !self.next_selfdestructs.is_empty() {
            // find the next selfdestruct to yield
            if let Some((next_trace, _)) = self.iter.peek() {
                // find the most recently recorded selfdestruct that has a lower address
                if let Some(pos) = self
                    .next_selfdestructs
                    .iter()
                    .rposition(|selfdestruct| selfdestruct.trace_address < next_trace.trace_address)
                {
                    return Some(self.next_selfdestructs.remove(pos));
                }
            } else {
                // drain the recorded selfdestructs
                return self.next_selfdestructs.pop();
            }
        }

        let (mut trace, node) = self.iter.next()?;
        if node.is_selfdestruct() {
            // since selfdestructs are emitted as additional trace, increase the trace count
            let mut addr = trace.trace_address.clone();
            addr.push(trace.subtraces);
            // need to account for the additional selfdestruct trace
            trace.subtraces += 1;
            if let Some(selfdestruct) = node.parity_selfdestruct_trace(addr) {
                self.next_selfdestructs.push(selfdestruct);
            }
        }
        Some(trace)
    }
}

/// addresses are presorted via breadth first walk thru [CallTraceNode]s, this  can be done by a
/// walker in [crate::tracing::builder::walker]
///
/// iteratively fill the [VmTrace] code fields
pub(crate) fn populate_vm_trace_bytecodes<DB, I>(
    db: DB,
    trace: &mut VmTrace,
    breadth_first_addresses: I,
) -> Result<(), DB::Error>
where
    DB: DatabaseRef,
    I: IntoIterator<Item = Address>,
{
    let mut stack: VecDeque<&mut VmTrace> = VecDeque::new();
    stack.push_back(trace);

    let mut addrs = breadth_first_addresses.into_iter();

    while let Some(curr_ref) = stack.pop_front() {
        for op in curr_ref.ops.iter_mut() {
            if let Some(sub) = op.sub.as_mut() {
                stack.push_back(sub);
            }
        }

        let addr = addrs.next().expect("there should be an address");

        let db_acc = db.basic_ref(addr)?.unwrap_or_default();

        curr_ref.code = if let Some(code) = db_acc.code {
            code.original_bytes()
        } else {
            let code_hash =
                if db_acc.code_hash != KECCAK_EMPTY { db_acc.code_hash } else { continue };

            db.code_by_hash_ref(code_hash)?.original_bytes()
        };
    }

    Ok(())
}

/// Populates [StateDiff] given iterator over [Account]s and a [DatabaseRef].
///
/// Loops over all state accounts in the accounts diff that contains all accounts that are included
/// in the [ExecutionResult] state map and compares the balance and nonce against what's in the
/// `db`, which should point to the beginning of the transaction.
///
/// It's expected that `DB` is a revm [Database](revm::database_interface::Database) which at this
/// point already contains all the accounts that are in the state map and never has to fetch them
/// from disk.
pub fn populate_state_diff<'a, DB, I>(
    state_diff: &mut StateDiff,
    db: DB,
    account_diffs: I,
) -> Result<(), DB::Error>
where
    I: IntoIterator<Item = (&'a Address, &'a Account)>,
    DB: DatabaseRef,
{
    for (addr, changed_acc) in account_diffs.into_iter() {
        // if the account was selfdestructed and created during the transaction, we can ignore it
        if changed_acc.is_selfdestructed() && changed_acc.is_created() {
            continue;
        }

        let addr = *addr;
        let entry = state_diff.entry(addr).or_default();

        // we need to fetch the account from the db
        let db_acc = db.basic_ref(addr)?.unwrap_or_default();

        // we check if this account was created during the transaction
        // where the smart contract was not touched before being created (no balance)
        if changed_acc.is_created() && db_acc.balance == U256::ZERO {
            // This only applies to newly created accounts without balance
            // A non existing touched account (e.g. `to` that does not exist) is excluded here
            entry.balance = Delta::Added(changed_acc.info.balance);
            entry.nonce = Delta::Added(U64::from(changed_acc.info.nonce));

            // accounts without code are marked as added
            let account_code = load_account_code(&db, &changed_acc.info).unwrap_or_default();
            entry.code = Delta::Added(account_code);

            // new storage values are marked as added,
            // however we're filtering changed here to avoid adding entries for the zero value
            for (key, slot) in changed_acc.storage.iter().filter(|(_, slot)| slot.is_changed()) {
                entry.storage.insert((*key).into(), Delta::Added(slot.present_value.into()));
            }
        } else {
            // we check if this account was created during the transaction
            // where the smart contract was touched before being created (has balance)
            if changed_acc.is_created() {
                let original_account_code = load_account_code(&db, &db_acc).unwrap_or_default();
                let present_account_code =
                    load_account_code(&db, &changed_acc.info).unwrap_or_default();
                entry.code = Delta::changed(original_account_code, present_account_code);
            }

            // update _changed_ storage values
            for (key, slot) in changed_acc.storage.iter().filter(|(_, slot)| slot.is_changed()) {
                entry.storage.insert(
                    (*key).into(),
                    Delta::changed(slot.original_value.into(), slot.present_value.into()),
                );
            }

            // check if the account was changed at all
            if entry.storage.is_empty()
                && db_acc == changed_acc.info
                && !changed_acc.is_selfdestructed()
            {
                // clear the entry if the account was not changed
                state_diff.remove(&addr);
                continue;
            }

            entry.balance = if db_acc.balance == changed_acc.info.balance {
                Delta::Unchanged
            } else {
                Delta::Changed(ChangedType { from: db_acc.balance, to: changed_acc.info.balance })
            };

            // this is relevant for the caller and contracts
            entry.nonce = if db_acc.nonce == changed_acc.info.nonce {
                Delta::Unchanged
            } else {
                Delta::Changed(ChangedType {
                    from: U64::from(db_acc.nonce),
                    to: U64::from(changed_acc.info.nonce),
                })
            };
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracing::types::{CallKind, CallTrace};

    #[test]
    fn test_parity_suicide_simple_call() {
        let nodes = vec![CallTraceNode {
            trace: CallTrace {
                kind: CallKind::Call,
                selfdestruct_refund_target: Some(Address::ZERO),
                ..Default::default()
            },
            ..Default::default()
        }];

        let traces = ParityTraceBuilder::new(nodes, None, TracingInspectorConfig::default_parity())
            .into_transaction_traces();

        assert_eq!(traces.len(), 2);
        assert_eq!(traces[0].trace_address.len(), 0);
        assert!(traces[0].action.is_call());
        assert_eq!(traces[1].trace_address, vec![0]);
        assert!(traces[1].action.is_selfdestruct());
    }

    #[test]
    fn test_parity_suicide_with_subsequent_calls() {
        /*
        contract Foo {
            function foo() public {}
            function close(Foo f) public {
                f.foo();
                selfdestruct(payable(msg.sender));
            }
        }

        contract Bar {
            Foo foo1;
            Foo foo2;

            constructor() {
                foo1 = new Foo();
                foo2 = new Foo();
            }

            function close() public {
                foo1.close(foo2);
            }
        }
        */

        let nodes = vec![
            CallTraceNode {
                parent: None,
                children: vec![1],
                idx: 0,
                trace: CallTrace { depth: 0, ..Default::default() },
                ..Default::default()
            },
            CallTraceNode {
                parent: Some(0),
                idx: 1,
                children: vec![2],
                trace: CallTrace {
                    depth: 1,
                    kind: CallKind::Call,
                    selfdestruct_refund_target: Some(Address::ZERO),
                    ..Default::default()
                },
                ..Default::default()
            },
            CallTraceNode {
                parent: Some(1),
                idx: 2,
                trace: CallTrace { depth: 2, ..Default::default() },
                ..Default::default()
            },
        ];

        let traces = ParityTraceBuilder::new(nodes, None, TracingInspectorConfig::default_parity())
            .into_transaction_traces();

        assert_eq!(traces.len(), 4);

        // [] call
        assert_eq!(traces[0].trace_address.len(), 0);
        assert_eq!(traces[0].subtraces, 1);
        assert!(traces[0].action.is_call());

        // [0] call
        assert_eq!(traces[1].trace_address, vec![0]);
        assert_eq!(traces[1].subtraces, 2);
        assert!(traces[1].action.is_call());

        // [0, 0] call
        assert_eq!(traces[2].trace_address, vec![0, 0]);
        assert!(traces[2].action.is_call());

        // [0, 1] suicide
        assert_eq!(traces[3].trace_address, vec![0, 1]);
        assert!(traces[3].action.is_selfdestruct());
    }
}
