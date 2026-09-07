use crate::tracing::types::CallTraceNode;
use alloc::{collections::VecDeque, vec::Vec};

/// Traverses the internal tracing structure breadth-first.
///
/// This is a lazy iterator.
pub(crate) struct CallTraceNodeWalkerBF<'trace> {
    /// The entire arena.
    nodes: &'trace Vec<CallTraceNode>,
    /// Indexes of nodes to visit as we traverse.
    queue: VecDeque<usize>,
}

impl<'trace> CallTraceNodeWalkerBF<'trace> {
    pub(crate) fn new(nodes: &'trace Vec<CallTraceNode>) -> Self {
        let mut queue = VecDeque::with_capacity(nodes.len());
        queue.push_back(0);
        Self { nodes, queue }
    }
}

impl<'trace> Iterator for CallTraceNodeWalkerBF<'trace> {
    type Item = &'trace CallTraceNode;

    fn next(&mut self) -> Option<Self::Item> {
        self.queue.pop_front().map(|idx| {
            let curr = &self.nodes[idx];
            self.queue.extend(curr.children.iter().copied());
            curr
        })
    }
}
