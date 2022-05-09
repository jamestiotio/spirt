//! Control-flow graph (CFG) abstractions and utilities.

use crate::{
    spv, AttrSet, ControlNode, ControlNodeKind, ControlRegion, EntityOrientedDenseMap,
    EntityOrientedMapKey, FuncAt, FuncDefBody, FxIndexMap, Value,
};
use smallvec::SmallVec;

/// The control-flow graph (CFG) of a function, as control-flow instructions
/// (`ControlInst`s) attached to `ControlNode`-relative CFG points (`ControlPoint`s).
#[derive(Clone, Default)]
pub struct ControlFlowGraph {
    pub control_insts: EntityOrientedDenseMap<ControlPoint, ControlInst>,
}

/// A point in the control-flow graph (CFG) of a function, relative to a `ControlNode`.
///
/// The whole CFG of the function consists of `ControlInst`s connecting all such
/// points, expect for these special cases:
///
/// * `ControlNodeKind::UnstructuredMerge`: lacks an `Entry` point entirely, as
///   its purpose is to represent an effectively multiple-entry single-exit (MESE)
///   "half-`ControlNode`", that could only become complete by structurization
///   (and would likely end up the "merge" / exit side of the structured node)
///
/// * `ControlNodeKind::Block`: between its `Entry` and `Exit` points, a block only
///   has its own linear sequence of instructions as (implied) control-flow, so
///   no `ControlInst` can attach to its `Entry` or target its `Exit`
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub enum ControlPoint {
    Entry(ControlNode),
    Exit(ControlNode),
}

impl ControlPoint {
    pub fn control_node(self) -> ControlNode {
        match self {
            Self::Entry(control_node) | Self::Exit(control_node) => control_node,
        }
    }
}

impl<V> EntityOrientedMapKey<V> for ControlPoint {
    type Entity = ControlNode;
    fn to_entity(point: Self) -> ControlNode {
        point.control_node()
    }

    type DenseValueSlots = [Option<V>; 2];
    fn get_dense_value_slot(point: Self, [entry, exit]: &[Option<V>; 2]) -> &Option<V> {
        match point {
            Self::Entry(_) => entry,
            Self::Exit(_) => exit,
        }
    }
    fn get_dense_value_slot_mut(point: Self, [entry, exit]: &mut [Option<V>; 2]) -> &mut Option<V> {
        match point {
            Self::Entry(_) => entry,
            Self::Exit(_) => exit,
        }
    }
}

#[derive(Clone)]
pub struct ControlInst {
    pub attrs: AttrSet,

    pub kind: ControlInstKind,

    pub inputs: SmallVec<[Value; 2]>,

    // FIXME(eddyb) change the inline size of this to fit most instructions.
    pub targets: SmallVec<[ControlPoint; 4]>,

    /// `target_merge_outputs[control_node][output_idx]` is the `Value` that
    /// `Value::ControlNodeOutput { control_node, output_idx }` will get on exit
    /// from `control_node` (via `ControlPoint::Exit(control_node)` in `targets`).
    pub target_merge_outputs: FxIndexMap<ControlNode, SmallVec<[Value; 2]>>,
}

#[derive(Clone)]
pub enum ControlInstKind {
    /// Reaching this point in the control-flow is undefined behavior, e.g.:
    /// * a `SelectBranch` case that's known to be impossible
    /// * after a function call, where the function never returns
    ///
    /// Optimizations can take advantage of this information, to assume that any
    /// necessary preconditions for reaching this point, are never met.
    Unreachable,

    /// Leave the current function, optionally returning a value.
    Return,

    /// Leave the current invocation, similar to returning from every function
    /// call in the stack (up to and including the entry-point), but potentially
    /// indicating a fatal error as well.
    ExitInvocation(ExitInvocationKind),

    /// Unconditional branch to a single target.
    Branch,

    /// Branch to one of several targets, chosen by a single value input.
    SelectBranch(SelectionKind),
}

#[derive(Clone)]
pub enum ExitInvocationKind {
    SpvInst(spv::Inst),
}

#[derive(Clone)]
pub enum SelectionKind {
    /// Conditional branch on boolean condition, i.e. `if`-`else`.
    BoolCond,

    SpvInst(spv::Inst),
}

impl ControlFlowGraph {
    /// Iterate over all `ControlPoint`s reachable through the CFG for `func_def_body`,
    /// in reverse post-order (RPO).
    ///
    /// RPO iteration over a CFG provides certain guarantees, most importantly
    /// that SSA definitions are visited before any of their uses.
    pub fn rev_post_order(
        &self,
        func_def_body: &FuncDefBody,
    ) -> impl DoubleEndedIterator<Item = ControlPoint> {
        self.post_order(func_def_body).rev()
    }

    /// Iterate over all `ControlPoint`s reachable through the CFG for `func_def_body`,
    /// in post-order.
    pub fn post_order(
        &self,
        func_def_body: &FuncDefBody,
    ) -> impl DoubleEndedIterator<Item = ControlPoint> {
        let mut post_order = SmallVec::<[_; 8]>::new();
        {
            let mut visited = EntityOrientedDenseMap::new();
            self.post_order_step(
                func_def_body.at(ControlPoint::Entry(
                    func_def_body.body.children.iter().first,
                )),
                &ControlRegionSuccessor::Return,
                &mut visited,
                &mut post_order,
            );
        }

        post_order.into_iter()
    }
}

/// The logical continuation of a `ControlRegion` (used by `post_order_step`).
enum ControlRegionSuccessor<'a> {
    /// No structural exit allowed, only `ControlInst`.
    Unstructured,

    /// Structural return implied by exiting a function body.
    Return,

    /// The `ControlRegion` has a parent `ControlNode`, which must also be exited.
    ExitParent {
        parent: ControlNode,
        parent_region_successor: &'a ControlRegionSuccessor<'a>,
    },
}

impl ControlFlowGraph {
    fn post_order_step(
        &self,
        func_at_point: FuncAt<ControlPoint>,
        region_successor: &ControlRegionSuccessor<'_>,
        // FIXME(eddyb) use a dense entity-oriented bitset here instead.
        visited: &mut EntityOrientedDenseMap<ControlPoint, ()>,
        post_order: &mut SmallVec<[ControlPoint; 8]>,
    ) {
        let point = func_at_point.position;
        let already_visited = visited.insert(point, ()).is_some();
        if already_visited {
            return;
        }

        let mut visit_target = |target, new_region_successor: &_| {
            self.post_order_step(
                func_at_point.at(target),
                new_region_successor,
                visited,
                post_order,
            );
        };
        if let Some(control_inst) = self.control_insts.get(point) {
            // With a `ControlInst`, it can be followed regardless of `ControlNodeKind`.
            for &target in &control_inst.targets {
                visit_target(target, &ControlRegionSuccessor::Unstructured);
            }
        } else {
            // Without a `ControlInst`, edges must be structural/implicit.
            let control_node = point.control_node();
            let control_node_def = &func_at_point.control_nodes[control_node];

            if let (ControlPoint::Entry(_), ControlNodeKind::Block { .. }) =
                (point, &control_node_def.kind)
            {
                // Blocks don't have `ControlInst`s attached to their `Entry`,
                // only to their `Exit`, so we pretend here there is an edge
                // between their `Entry` and `Exit` points.
                visit_target(ControlPoint::Exit(control_node), region_successor);
            } else {
                match point {
                    // Entering a `ControlNode` depends entirely on the `ControlNodeKind`.
                    ControlPoint::Entry(_) => {
                        let child_regions: &[ControlRegion] = match control_node_def.kind {
                            ControlNodeKind::Block { .. } => unreachable!(),

                            ControlNodeKind::UnstructuredMerge => &[],
                        };
                        for region in child_regions {
                            visit_target(
                                ControlPoint::Entry(region.children.iter().first),
                                &ControlRegionSuccessor::ExitParent {
                                    parent: control_node,
                                    parent_region_successor: region_successor,
                                },
                            )
                        }
                    }

                    // Exiting a `ControlNode` chains to a sibling/parent.
                    ControlPoint::Exit(_) => {
                        match control_node_def.next_in_list() {
                            // Enter the next sibling in the `ControlRegion`, if one exists.
                            Some(next_control_node) => {
                                visit_target(
                                    ControlPoint::Entry(next_control_node),
                                    region_successor,
                                );
                            }

                            // Exit the parent `ControlNode`, if one exists.
                            None => match region_successor {
                                ControlRegionSuccessor::Unstructured => unreachable!(
                                    "cfg: missing `ControlInst`, despite \
                                     having left structured control-flow"
                                ),

                                ControlRegionSuccessor::Return => {}

                                &ControlRegionSuccessor::ExitParent {
                                    parent,
                                    parent_region_successor,
                                } => visit_target(
                                    ControlPoint::Exit(parent),
                                    parent_region_successor,
                                ),
                            },
                        }
                    }
                }
            }
        }
        post_order.push(point);
    }
}
