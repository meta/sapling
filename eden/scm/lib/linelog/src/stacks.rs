/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linelog features that depend on `visit_with_ins_del_stacks`,
//! including `flatten`, and `calculate_dep_dag`.

use std::mem::take;
use std::sync::Arc;

use crate::SmallRevs;
use crate::linelog::AbstractLineLog;
use crate::linelog::EntryId;
use crate::linelog::Inst;
use crate::linelog::Pc;
use crate::linelog::Rev;
use crate::nanodag::NanoDag;

/// A "flatten" line, annotating which revisions contain this line.
#[derive(Clone)]
#[cfg_attr(debug_assertions, derive(Debug, PartialEq))]
pub struct FlattenLine<T> {
    pub data: Arc<T>,
    pub revs: SmallRevs,
}

#[derive(Eq, PartialEq, Clone, Copy)]
pub(crate) struct Frame {
    pub(crate) rev: Rev,
    end_pc: Pc,
}

impl Frame {
    /// A "virtual" frame that serves as a placeholder, like a parent of everything.
    const ROOT: Frame = Frame {
        rev: 0,
        end_pc: Pc::MAX,
    };

    fn is_root(&self) -> bool {
        *self == Self::ROOT
    }
}

/// Callbacks for `visit_with_ins_del_stacks`. All methods are no-ops by
/// default; implementors override only what they need.
pub(crate) trait StackVisitor<T> {
    /// Called for each LINE instruction, with the current stacks.
    fn on_line(&mut self, _data: &Arc<T>, _rev: Rev, _ins_stack: &[Frame], _del_stack: &[Frame]) {}
    /// Called for each JGE/JL (conditional jump) instruction, before the
    /// corresponding stack push. Receives the current stacks so the
    /// implementor can inspect the outer insertion/deletion context.
    fn on_conditional_jump(&mut self, _rev: Rev, _ins_stack: &[Frame], _del_stack: &[Frame]) {}
    /// Called after a stack push. `is_ins` indicates which stack changed.
    fn on_stack_push(&mut self, _is_ins: bool, _ins_stack: &[Frame], _del_stack: &[Frame]) {}
    /// Called after a stack pop. `is_ins` indicates which stack changed.
    fn on_stack_pop(&mut self, _is_ins: bool, _ins_stack: &[Frame], _del_stack: &[Frame]) {}
}

impl<T, M> AbstractLineLog<T, M> {
    /// Returns all lines that ever existed, including deleted lines,
    /// each annotated with the set of revisions containing it.
    ///
    /// Useful for figuring out file contents after reordering or folding
    /// commits, or providing a view similar to `absorb -e FILE` to edit
    /// all versions of a file in a single view.
    pub fn flatten(&self, entry: EntryId) -> Vec<FlattenLine<T>> {
        // See the comments in visit_with_ins_del_stacks for what the stacks mean.
        //
        // The flatten algorithm works as follows:
        // - For each line, we got an insRev (insStack.at(-1).rev), and a
        //   delRev (delStack.at(-1)?.rev ?? maxRev + 1), meaning the rev
        //   attached to the innermost insertion or deletion blocks,
        //   respectively.
        // - That line is then present in insRev .. delRev (exclusive) revs.
        //
        // This works because:
        // - The blocks are nested in order:
        //    - For nested insertions, the nested one must have a larger rev, and
        //      lines inside the nested block are only present starting from the
        //      larger rev.
        //    - For nested deletions, the nested one must have a smaller rev, and
        //      lines inside the nested block are considered as deleted by the
        //      smaller rev.
        //    - For interleaved insertion and deletions, insertion rev and deletion
        //      rev are tracked separately so their calculations are independent
        //      from each other.
        // - In linear history, (insRev, delRev) can be converted to `Revs` trivially.
        // - In DAG history, insRev .. delRev is generalized to `Revs` based on visibility.
        //   See the `cumulative_` fields below.
        struct FlattenVisitor<'a, T> {
            result: Vec<FlattenLine<T>>,
            // Cumulative revs where every active insertion frame is visible.
            //
            // For DAG `0-{1,2}-3`, if `ins_stack` is `[ROOT, rev 0, rev 1]`,
            // then `cumulative_ins_stack` is:
            // - all revs:       {0, 1, 2, 3}   # [ROOT]
            // - descendants(0): {0, 1, 2, 3}   # [ROOT, rev 0]
            // - descendants(1): {1, 3}         # [ROOT, rev 0, rev 1]
            cumulative_ins_stack: Vec<SmallRevs>,
            // Cumulative revs where any active deletion frame is visible.
            // For example, active deletion frames `[rev 1, rev 2]` produce
            // descendants(1) union descendants(2): {1, 2, 3}.
            cumulative_del_stack: Vec<SmallRevs>,
            // Current line visibility: insertion visibility minus deletion visibility.
            current_revs: SmallRevs,
            dag: &'a NanoDag,
        }

        impl<T> FlattenVisitor<'_, T> {
            fn refresh_current_revs(&mut self) {
                self.current_revs = self
                    .cumulative_ins_stack
                    .last()
                    .cloned()
                    .unwrap_or_else(SmallRevs::empty);
                if let Some(del_revs) = self.cumulative_del_stack.last() {
                    self.current_revs.difference_with(del_revs);
                }
            }

            fn push_ins_revs(&mut self, frame: Option<Frame>) {
                let mut revs = self
                    .cumulative_ins_stack
                    .last()
                    .cloned()
                    .unwrap_or_else(SmallRevs::empty);
                if let Some(frame) = frame {
                    if !frame.is_root() {
                        revs.intersect_with(self.dag.descendants(frame.rev));
                    }
                }
                self.cumulative_ins_stack.push(revs);
            }

            fn push_del_revs(&mut self, frame: Option<Frame>) {
                let mut revs = self
                    .cumulative_del_stack
                    .last()
                    .cloned()
                    .unwrap_or_else(SmallRevs::empty);
                if let Some(frame) = frame {
                    revs.union_with(self.dag.descendants(frame.rev));
                }
                self.cumulative_del_stack.push(revs);
            }
        }

        impl<T> StackVisitor<T> for FlattenVisitor<'_, T> {
            fn on_stack_push(&mut self, is_ins: bool, ins_stack: &[Frame], del_stack: &[Frame]) {
                if is_ins {
                    self.push_ins_revs(ins_stack.last().copied());
                } else {
                    self.push_del_revs(del_stack.last().copied());
                }
                self.refresh_current_revs();
            }

            fn on_stack_pop(&mut self, is_ins: bool, _ins_stack: &[Frame], _del_stack: &[Frame]) {
                if is_ins {
                    self.cumulative_ins_stack.pop();
                } else {
                    self.cumulative_del_stack.pop();
                }
                self.refresh_current_revs();
            }

            fn on_line(
                &mut self,
                data: &Arc<T>,
                _rev: Rev,
                _ins_stack: &[Frame],
                _del_stack: &[Frame],
            ) {
                self.result.push(FlattenLine {
                    data: data.clone(),
                    revs: self.current_revs.clone(),
                });
            }
        }

        let mut visitor = FlattenVisitor {
            result: Vec::new(),
            cumulative_ins_stack: vec![self.dag.all()],
            cumulative_del_stack: vec![],
            current_revs: self.dag.all(),
            dag: &self.dag,
        };
        self.visit_with_ins_del_stacks(entry, &mut visitor);
        visitor.result
    }

    /// Calculate the dependencies of revisions.
    ///
    /// For example, `{5: {3, 1}}` means rev 5 depends on rev 3 and rev 1.
    ///
    /// Based on LineLog instruction nesting, which could be different from
    /// traditional textual context-line dependencies. LineLog dependencies
    /// help prevent "malformed cases" when nested blocks (insertions or
    /// deletions) might be skipped incorrectly. Example use case is the
    /// check in `topo_remap` to ensure the remap result is still sane.
    /// Practically, LineLog might allow reorder cases that would be disallowed
    /// by traditional context-line dependencies.
    ///
    /// This function intentionally does not use `self.dag` for edge check.
    ///
    /// The goal of this function is to expose linelog's internal implicit
    /// dependencies (e.g. an outer block might cause an inner block to be
    /// skipped, regardless of the two revs' relation in `self.dag`, initially
    /// for performance considerations). So this function does not double check
    /// if the edge is present in `self.dag` to report the linelog internal
    /// dependencies honestly.
    pub(crate) fn calculate_entry_dep_dag(&self, entry: EntryId) -> NanoDag {
        // With the insertion and deletion stacks (see explanation in
        // visit_with_ins_del_stacks), when we see a new insertion block, or deletion
        // block, we add two dependencies:
        // - The inner rev depends on the outer insertion rev.
        // - The outer deletion rev (if present) depends on the inner rev.
        //
        // Let's look at how this is done at the instruction level.
        // the instructions generated by editChunk:
        //
        //      a2Pc: ...
        //            ...
        //     start: JL brev b2Pc
        //            ...
        //      b2Pc: JGE brev a2Pc
        //          : <a1 Inst>
        //       end: J a1Pc+1
        //
        // JL is used for insertion, JGE is used for deletion. We then use them to
        // manipulate the insStack and delStack:
        //
        // insStack:
        //
        //    - On "start: JL brev b2Pc":
        //      Do not follow the JL jump. (by visit_with_ins_del_stacks)
        //      Mark brev as dependent on the outer insertion.
        //      Mark the outer deletion as dependent on this brev.
        //      Push {rev, b2Pc} to insStack. (by visit_with_ins_del_stacks)
        //    - When pc is b2Pc, pop insStack. (by visit_with_ins_del_stacks)
        //
        // delStack:
        //
        //    - On "b2Pc: JGE brev a2Pc":
        //      Do not follow the JGE jump. (by visit_with_ins_del_stacks)
        //      Mark brev as dependent on the outer insertion.
        //      Mark the outer deletion as dependent on this brev.
        //      Push {rev, a2Pc} to delStack. (by visit_with_ins_del_stacks)
        //    - When pc is a2Pc, pop delStack. (by visit_with_ins_del_stacks)
        struct DepMapVisitor(NanoDag);

        impl DepMapVisitor {
            fn maybe_add_edge(&mut self, parent: Rev, child: Rev) {
                // This intentionally does not use the main dag.
                // It does not assume or enforce linear history for linelog.
                // See the outer docstring for context.
                if parent < child {
                    self.0 = take(&mut self.0).with_edge(parent, child);
                }
            }
        }

        impl<T> StackVisitor<T> for DepMapVisitor {
            fn on_conditional_jump(&mut self, rev: Rev, ins_stack: &[Frame], del_stack: &[Frame]) {
                // rev depends on the outer insertion (parent).
                if let Some(parent) = ins_stack.last().filter(|f| !f.is_root()).map(|f| f.rev) {
                    self.maybe_add_edge(parent, rev);
                }
                // The outer deletion depends on rev (rev is the parent).
                if let Some(child) = del_stack.last().map(|f| f.rev) {
                    self.maybe_add_edge(rev, child);
                }
            }
        }

        let mut visitor = DepMapVisitor(NanoDag::default().truncate(self.dag.len()));
        self.visit_with_ins_del_stacks(entry, &mut visitor);
        visitor.0
    }

    pub(crate) fn calculate_dep_dag(&self) -> NanoDag {
        self.entries
            .iter()
            .enumerate()
            .map(|(entry, _)| self.calculate_entry_dep_dag(EntryId(entry)))
            .fold(
                NanoDag::default().truncate(self.dag.len()),
                |mut result, dep| {
                    for (child, parents) in dep.iter() {
                        for parent in parents {
                            result = result.with_edge(*parent, child);
                        }
                    }
                    result
                },
            )
    }

    /// Visit (execute) instructions with the insertion and deletion stacks
    /// converted from JGE and JL instructions maintained by this function.
    ///
    /// See the comment in this function about how to turn JGE and JL to
    /// the stacks.
    ///
    /// For stacks like this:
    ///
    /// ```text
    ///    +---- Insertion (rev 1)
    ///    |     Line 1
    ///    |                    ----+ Deletion (rev 4)
    ///    |     Line 2             |
    ///    | +-- Insertion (rev 2)  |
    ///    | |   Line 3             |
    ///    | |                  --+ | Deletion (rev 3)
    ///    | |   Line 4           | |
    ///    | +--                  | |
    ///    |     Line 5           | |
    ///    |                    --+ |
    ///    |     Line 6             |
    ///    |                    ----+
    ///    |     Line 7
    ///    +----
    /// ```
    ///
    /// When visiting "Line 3", the callsite will get insertion stack =
    /// [rev 1, rev 2] and deletion stack = [rev 4].
    ///
    /// Internally, this is done by turning conditional jumps (JGE or JL)
    /// to stack pushes, pops at the JGE or JL destinations, and follow
    /// unconditional jumps (J) as usual. For more details, see the comment
    /// inside this function.
    ///
    /// This function will call `withContext` to provide the `ins_stack` and
    /// `del_stack` context, and expect the callsite to provide handlers it
    /// is interested in.
    ///
    /// Typical use-cases include features that need to scan all (ever existed)
    /// lines like flatten() and calculate_dep_dag().
    pub(crate) fn visit_with_ins_del_stacks(
        &self,
        entry: EntryId,
        visitor: &mut impl StackVisitor<T>,
    ) {
        // How does it work? First, insertions and deletions in linelog form
        // tree structures. For example:
        //
        //    +---- Insertion (rev 1)
        //    |     Line 1
        //    |                    ----+ Deletion (rev 4)
        //    |     Line 2             |
        //    | +-- Insertion (rev 2)  |
        //    | |   Line 3             |
        //    | |                  --+ | Deletion (rev 3)
        //    | |   Line 4           | |
        //    | +--                  | |
        //    |     Line 5           | |
        //    |                    --+ |
        //    |     Line 6             |
        //    |                    ----+
        //    |     Line 7
        //    +----
        //
        // Note interleaved insertions do not happen. For example, this does not
        // happen:
        //
        //    +---- Insertion (rev 1)
        //    |     Line 1
        //    | +-- Insertion (rev 2)
        //    | |   Line 2
        //    +-|--
        //      |   Line 3
        //      +--
        //
        // Similarly, interleaved deletions do not happen. However, insertions
        // might interleave with deletions, as shown above.
        //
        // Let's look at how this is done at the instruction level. First, look at
        // the instructions generated by editChunk:
        //
        //      a2Pc: ...
        //            ...
        //     start: JL brev b2Pc
        //            ...
        //      b2Pc: JGE brev a2Pc
        //          : <a1 Inst>
        //       end: J a1Pc+1
        //
        // JL is used for insertion, JGE is used for deletion. We then use them to
        // manipulate the ins_stack and del_stack:
        //
        // ins_stack:
        //
        //    - On "start: JL brev b2Pc":
        //      Do not follow the JL jump.
        //      Push {rev, b2Pc} to ins_stack.
        //    - When pc is b2Pc, pop ins_stack.
        //
        // del_stack:
        //
        //    - On "b2Pc: JGE brev a2Pc":
        //      Do not follow the JGE jump.
        //      Push {rev, a2Pc} to del_stack.
        //    - When pc is a2Pc, pop del_stack.
        //
        // You might have noticed that we don't use the revs in LINE instructions
        // at all. This is because that LINE rev always matches its JL rev in this
        // implementation. In other words, the "rev" in LINE instruction is
        // redundant as it can be inferred from JL, with an ins_stack. Note in the
        // original C implementation of LineLog the LINE rev can be different from
        // the JL rev, to deal with merges while maintaining a linear history.
        let mut ins_stack = vec![Frame::ROOT];
        let mut del_stack: Vec<Frame> = Vec::new();
        let mut pc = self.entries[entry.0];
        let mut patience = self.code.len() * 2;

        while patience > 0 {
            if ins_stack.last().is_some_and(|f| f.end_pc == pc) {
                ins_stack.pop();
                visitor.on_stack_pop(true, &ins_stack, &del_stack);
            }
            if del_stack.last().is_some_and(|f| f.end_pc == pc) {
                del_stack.pop();
                visitor.on_stack_pop(false, &ins_stack, &del_stack);
            }

            match &self.code[pc] {
                Inst::LINE(_entry, rev, data) => {
                    visitor.on_line(data, *rev, &ins_stack, &del_stack);
                    pc += 1;
                }
                Inst::END => break,
                Inst::J(j_pc) => pc = *j_pc,
                Inst::JGE(rev, j_pc) => {
                    visitor.on_conditional_jump(*rev, &ins_stack, &del_stack);
                    del_stack.push(Frame {
                        rev: *rev,
                        end_pc: *j_pc,
                    });
                    visitor.on_stack_push(false, &ins_stack, &del_stack);
                    pc += 1;
                }
                Inst::JL(rev, j_pc) => {
                    visitor.on_conditional_jump(*rev, &ins_stack, &del_stack);
                    ins_stack.push(Frame {
                        rev: *rev,
                        end_pc: *j_pc,
                    });
                    visitor.on_stack_push(true, &ins_stack, &del_stack);
                    pc += 1;
                }
            }
            patience -= 1;
        }
        assert!(patience > 0, "bug: code does not terminate");
    }
}
