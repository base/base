//! Inputs of the state campaign: short sequences of chain operations sent to every node, and
//! their mutations.

use alloy_primitives::{hex, keccak256};
use serde::{Deserialize, Serialize};

use crate::{CorpusCase, Rng, StressPlan};

/// One chain operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum StateOp {
    /// Builds `blocks` blocks on the head and makes the last the head, with a forkchoice update
    /// after every block (`fcu_each`) or only after the last.
    #[serde(rename_all = "camelCase")]
    Extend {
        /// Blocks to build (at least one).
        blocks: u8,
        /// Stress traffic of every block.
        stress: StressPlan,
        /// Randomness of the blocks.
        salt: u64,
        /// Whether every block gets its own forkchoice update.
        fcu_each: bool,
    },
    /// Builds a sibling branch of `blocks` blocks on the head's ancestor `depth` blocks back,
    /// and makes its tip the head when `switch` is set (a reorg).
    #[serde(rename_all = "camelCase")]
    Fork {
        /// Blocks between the fork point and the head (at least one).
        depth: u8,
        /// Blocks of the branch (at least one).
        blocks: u8,
        /// Stress traffic of every block.
        stress: StressPlan,
        /// Randomness of the blocks.
        salt: u64,
        /// Whether the branch becomes canonical.
        switch: bool,
    },
    /// Makes an earlier branch tip the head again (`tip` picks among the known tips, most
    /// recent first).
    Switch {
        /// Index into the known tips.
        tip: u8,
    },
    /// Stops the nodes in the mask (bit `i` is node `i`; zero is every node) and starts them on
    /// the same datadir: cleanly (`SIGTERM`), or killed (`SIGKILL`).
    Restart {
        /// Whether the nodes are killed rather than stopped.
        kill: bool,
        /// Node mask.
        nodes: u8,
    },
    /// Submits the next block with a wrong state root (hash recomputed), which every node must
    /// refuse: `kind` picks a random root, the parent's root, the right root with one bit
    /// flipped, or the root of the block a different salt gives.
    BadRoot {
        /// Which wrong root.
        kind: u8,
        /// Randomness of the block.
        salt: u64,
    },
    /// Reads state back at `heights` canonical heights.
    Check {
        /// Heights to read (head first).
        heights: u8,
        /// Randomness of the keys read.
        seed: u64,
    },
}

impl StateOp {
    /// Most blocks one operation builds.
    pub const MAX_BLOCKS: u8 = 12;

    /// The operation's name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Extend { .. } => "extend",
            Self::Fork { .. } => "fork",
            Self::Switch { .. } => "switch",
            Self::Restart { kill: false, .. } => "restart",
            Self::Restart { kill: true, .. } => "kill",
            Self::BadRoot { .. } => "badRoot",
            Self::Check { .. } => "check",
        }
    }

    /// A random operation. Restarts are rarer than the rest: they take seconds.
    pub fn random(rng: &mut Rng, max_depth: u8) -> Self {
        match rng.weighted(&[40, 22, 8, 7, 7, 10]) {
            0 => Self::Extend {
                blocks: Self::blocks(rng),
                stress: StressPlan::random(rng),
                salt: rng.next_u64(),
                fcu_each: rng.chance(70),
            },
            1 => Self::Fork {
                depth: Self::depth(rng, max_depth),
                blocks: Self::blocks(rng),
                stress: StressPlan::random(rng),
                salt: rng.next_u64(),
                switch: rng.chance(70),
            },
            2 => Self::Switch { tip: rng.below(4) as u8 },
            3 => Self::Restart {
                kill: rng.chance(50),
                nodes: if rng.chance(50) { 0 } else { 1 << rng.below(3) },
            },
            4 => Self::BadRoot { kind: rng.below(4) as u8, salt: rng.next_u64() },
            _ => Self::Check { heights: rng.range(1, 6) as u8, seed: rng.next_u64() },
        }
    }

    fn blocks(rng: &mut Rng) -> u8 {
        let any = rng.range(1, u64::from(Self::MAX_BLOCKS)) as u8;
        *rng.pick(&[1, 1, 2, 3, any])
    }

    fn depth(rng: &mut Rng, max_depth: u8) -> u8 {
        let deep = rng.range(1, u64::from(max_depth.max(1))) as u8;
        *rng.pick(&[1, 1, 2, 3, 4, deep])
    }
}

/// One input: operations run in order on the nodes' current chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateInput {
    /// The operations.
    pub ops: Vec<StateOp>,
}

impl CorpusCase for StateInput {
    fn id(&self) -> String {
        let text = serde_json::to_vec(&self.ops).unwrap_or_default();
        let names: Vec<&str> = self.ops.iter().take(3).map(StateOp::name).collect();
        format!("{}-{}", names.join("-"), hex::encode(&keccak256(text)[..5]))
    }
}

impl StateInput {
    /// Most operations in one input.
    pub const MAX_OPS: usize = 8;

    /// A random input of one to [`Self::MAX_OPS`] operations.
    pub fn random(rng: &mut Rng, max_depth: u8) -> Self {
        let len = rng.range(1, Self::MAX_OPS as u64) as usize;
        Self { ops: (0..len).map(|_| StateOp::random(rng, max_depth)).collect() }
    }
}

/// Mutates state inputs.
#[derive(Debug)]
pub struct StateMutator {
    /// Randomness.
    pub rng: Rng,
    /// Deepest fork a mutation asks for.
    pub max_depth: u8,
}

impl StateMutator {
    /// A child of `parent`, maybe spliced with `donor`, and the operators applied.
    pub fn mutate(
        &mut self,
        parent: &StateInput,
        donor: Option<&StateInput>,
    ) -> (StateInput, Vec<&'static str>) {
        let mut child = parent.clone();
        let mut operators = Vec::new();
        for _ in 0..self.rng.range(1, 3) {
            let rng = &mut self.rng;
            let len = child.ops.len();
            let operator = match rng.below(9) {
                0 if len < StateInput::MAX_OPS => {
                    let op = StateOp::random(rng, self.max_depth);
                    child.ops.insert(rng.index(len + 1), op);
                    "insert"
                }
                1 if len > 1 => {
                    child.ops.remove(rng.index(len));
                    "delete"
                }
                2 if len < StateInput::MAX_OPS => {
                    let op = child.ops[rng.index(len)].clone();
                    child.ops.insert(rng.index(len + 1), op);
                    "duplicate"
                }
                3 if len > 1 => {
                    let at = rng.index(len - 1);
                    child.ops.swap(at, at + 1);
                    "swap"
                }
                4 => {
                    let Some(donor) = donor.filter(|d| !d.ops.is_empty()) else { continue };
                    let cut = rng.index(len + 1);
                    let from = rng.index(donor.ops.len());
                    child.ops.truncate(cut);
                    child.ops.extend(donor.ops[from..].iter().cloned());
                    child.ops.truncate(StateInput::MAX_OPS);
                    "splice"
                }
                5 => {
                    let at = rng.index(len);
                    Self::reseed(rng, &mut child.ops[at]);
                    "reseed"
                }
                6 => {
                    let at = rng.index(len);
                    Self::restress(rng, &mut child.ops[at]);
                    "stress"
                }
                _ => {
                    let at = rng.index(len);
                    Self::tweak(rng, &mut child.ops[at], self.max_depth);
                    "tweak"
                }
            };
            operators.push(operator);
        }
        if child.ops.is_empty() {
            child = StateInput::random(&mut self.rng, self.max_depth);
            operators.push("fresh");
        }
        (child, operators)
    }

    const fn reseed(rng: &mut Rng, op: &mut StateOp) {
        match op {
            StateOp::Extend { salt, .. }
            | StateOp::Fork { salt, .. }
            | StateOp::BadRoot { salt, .. }
            | StateOp::Check { seed: salt, .. } => *salt = rng.next_u64(),
            StateOp::Switch { tip } => *tip = rng.below(4) as u8,
            StateOp::Restart { nodes, .. } => *nodes = rng.below(8) as u8,
        }
    }

    /// Changes one knob of an operation's stress plan, or replaces the plan.
    fn restress(rng: &mut Rng, op: &mut StateOp) {
        let (StateOp::Extend { stress, .. } | StateOp::Fork { stress, .. }) = op else {
            return Self::reseed(rng, op);
        };
        if rng.chance(30) {
            *stress = StressPlan::random(rng);
            return;
        }
        let pct = |rng: &mut Rng| {
            let any = rng.below(101) as u8;
            *rng.pick(&[0, 100, any])
        };
        match rng.below(11) {
            0 => stress.txs = rng.below(13) as u8,
            1 => stress.slots = [1, 600, rng.below(800) as u16][rng.index(3)],
            2 => stress.cluster = pct(rng),
            3 => stress.hot = pct(rng),
            4 => stress.delete = pct(rng),
            5 => stress.reset = pct(rng),
            6 => stress.accounts = [0, 300, rng.below(400) as u16][rng.index(3)],
            7 => stress.destroy = pct(rng),
            8 => stress.touch = pct(rng),
            9 => stress.large = pct(rng),
            _ => stress.random_txs = rng.below(16) as u8,
        }
    }

    /// Changes an operation's numeric parameters or flags.
    fn tweak(rng: &mut Rng, op: &mut StateOp, max_depth: u8) {
        match op {
            StateOp::Extend { blocks, fcu_each, .. } => {
                if rng.chance(50) {
                    *fcu_each = !*fcu_each;
                } else {
                    *blocks = rng.range(1, u64::from(StateOp::MAX_BLOCKS)) as u8;
                }
            }
            StateOp::Fork { depth, blocks, switch, .. } => match rng.below(3) {
                0 => *depth = StateOp::depth(rng, max_depth),
                1 => *blocks = rng.range(1, u64::from(StateOp::MAX_BLOCKS)) as u8,
                _ => *switch = !*switch,
            },
            StateOp::Switch { tip } => *tip = tip.wrapping_add(1) % 4,
            StateOp::Restart { kill, nodes } => {
                if rng.chance(50) {
                    *kill = !*kill;
                } else {
                    *nodes = rng.below(8) as u8;
                }
            }
            StateOp::BadRoot { kind, .. } => *kind = (*kind + 1) % 4,
            StateOp::Check { heights, .. } => *heights = rng.range(1, 6) as u8,
        }
    }
}
