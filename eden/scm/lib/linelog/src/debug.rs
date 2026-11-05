/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fmt::Write;
use std::sync::Arc;

use crate::SmallRevs;
use crate::linelog::AbstractLineLog;
use crate::linelog::EntryId;
use crate::linelog::Inst;
use crate::linelog::Rev;
use crate::nanodag::NanoDag;
use crate::stacks::Frame;
use crate::stacks::StackVisitor;

impl<T: fmt::Display, M> AbstractLineLog<T, M> {
    /// Dump instructions in a human readable format. Useful for debugging.
    /// Note: This exposes internal details which might change in the future.
    pub fn describe_instructions(&self) -> Vec<String> {
        self.code
            .iter()
            .enumerate()
            .map(|(i, inst)| format!("{i}: {}", describe_inst(inst)))
            .collect()
    }

    /// Dump lines with ASCII annotated insertions and deletions stacks.
    pub fn describe_ins_del_stacks(&self, entry: EntryId) -> Vec<String> {
        // 1st Pass: Figure out the max stack depth, line length for padding.
        struct MeasureVisitor {
            max_ins_depth: usize,
            max_del_depth: usize,
            max_line_len: usize,
        }

        impl<T: fmt::Display> StackVisitor<T> for MeasureVisitor {
            fn on_stack_push(&mut self, _is_ins: bool, ins_stack: &[Frame], del_stack: &[Frame]) {
                self.max_ins_depth = self.max_ins_depth.max(ins_stack.len() + 1);
                self.max_del_depth = self.max_del_depth.max(del_stack.len() + 2);
            }

            fn on_line(
                &mut self,
                data: &Arc<T>,
                _rev: Rev,
                _ins_stack: &[Frame],
                _del_stack: &[Frame],
            ) {
                let len = format!("Line:  {}", data.to_string().trim_end()).len();
                self.max_line_len = self.max_line_len.max(len);
            }
        }

        let mut measure = MeasureVisitor {
            max_ins_depth: 0,
            max_del_depth: 0,
            max_line_len: "Insert (rev 1000) ".len(),
        };
        self.visit_with_ins_del_stacks(entry, &mut measure);

        // 2nd Pass: Render the instructions.
        struct RenderVisitor {
            result: Vec<String>,
            max_ins_depth: usize,
            max_del_depth: usize,
            max_line_len: usize,
        }

        impl RenderVisitor {
            fn push_line(
                &mut self,
                data: &str,
                ins_depth: usize,
                del_depth: usize,
                left_corner: Option<char>,
                right_corner: Option<char>,
            ) {
                let ins_pad = self.max_ins_depth - ins_depth;
                let del_pad = self.max_del_depth - del_depth;
                let vbar = "│".repeat(ins_depth);
                let left = match left_corner {
                    Some(c) => format!("{vbar}{c}{}", "─".repeat(ins_pad)),
                    None => format!("{vbar}{}", " ".repeat(ins_pad + 1)),
                };
                let vbar = "│".repeat(del_depth);
                let right = match right_corner {
                    Some(c) => format!("{}{c}{vbar}", "─".repeat(del_pad)),
                    None => format!("{}{vbar}", " ".repeat(del_pad + 1)),
                };
                let middle = format!("{:width$}", data, width = self.max_line_len);
                self.result.push(format!("{left}{middle}{right}"));
            }
        }

        impl<T: fmt::Display> StackVisitor<T> for RenderVisitor {
            fn on_stack_push(&mut self, is_ins: bool, ins_stack: &[Frame], del_stack: &[Frame]) {
                // ins_stack always has an initial rev 0 sentinel, subtract 1
                // for display depth. For ins/del push, subtract one more because
                // the connector line is drawn *before* the new depth level.
                let rev = if is_ins {
                    ins_stack.last().map_or(0, |f| f.rev)
                } else {
                    del_stack.last().map_or(0, |f| f.rev)
                };
                if is_ins {
                    // │ │ ╭────── Insert (rev x)  <- this line
                    // │ │ │       Line:  ....     <- following lines
                    self.push_line(
                        &format!("Insert (rev {rev})"),
                        ins_stack.len() - 2,
                        del_stack.len(),
                        Some('╭'),
                        None,
                    );
                } else {
                    //    Delete (rev x) ──────╮
                    self.push_line(
                        &format!("Delete (rev {rev})"),
                        ins_stack.len() - 1,
                        del_stack.len() - 1,
                        None,
                        Some('╮'),
                    );
                }
            }

            fn on_stack_pop(&mut self, is_ins: bool, ins_stack: &[Frame], del_stack: &[Frame]) {
                if is_ins {
                    self.push_line("", ins_stack.len() - 1, del_stack.len(), Some('╰'), None);
                } else {
                    self.push_line("", ins_stack.len() - 1, del_stack.len(), None, Some('╯'));
                }
            }

            fn on_line(
                &mut self,
                data: &Arc<T>,
                _rev: Rev,
                ins_stack: &[Frame],
                del_stack: &[Frame],
            ) {
                let trimmed = data.to_string();
                let trimmed = trimmed.trim_end();
                self.push_line(
                    &format!("Line:  {trimmed}"),
                    ins_stack.len() - 1,
                    del_stack.len(),
                    None,
                    None,
                );
            }
        }

        let mut render = RenderVisitor {
            result: Vec::new(),
            max_ins_depth: measure.max_ins_depth,
            max_del_depth: measure.max_del_depth,
            max_line_len: measure.max_line_len,
        };
        self.visit_with_ins_del_stacks(entry, &mut render);
        render.result
    }
}

fn describe_inst<T: fmt::Display>(inst: &Inst<T>) -> String {
    match inst {
        Inst::J(pc) => format!("J {pc}"),
        Inst::JGE(rev, pc) => format!("JGE {rev} {pc}"),
        Inst::JL(rev, pc) => format!("JL {rev} {pc}"),
        Inst::LINE(entry, rev, data) => {
            let trimmed = data.to_string();
            let trimmed = trimmed.trim_end();
            format!("LINE E{} {rev} {trimmed:?}", entry.0)
        }
        Inst::END => "END".to_string(),
    }
}

impl fmt::Display for NanoDag {
    /// Output compact representation of the dag, like: `1-{2,3-4}-5`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len() == 0 {
            return Ok(());
        }

        struct State {
            children_vec: Vec<SmallRevs>,
            postdom: Vec<SmallRevs>,
            end: usize,
        }

        fn shift1(revs: &SmallRevs) -> SmallRevs {
            revs.iter().map(|r| r + 1).collect()
        }

        impl State {
            /// Construct from `NanoDag`.
            ///
            /// Calculate post-dominators to decide when to use "}".
            ///
            /// Main logic (`walk`) requires a single "start" and a single
            /// "end", insert a super "root" (rev end), the parent of
            /// dag.roots(), and a super "head" (rev 0), the head of dag.heads()
            /// to satisfy the need. Original dag revs are added by 1 to make
            /// room for the super head.
            fn from_nanodag(dag: &NanoDag) -> Self {
                let end = dag.len() + 1;
                let end_revs = SmallRevs::from(end);

                let mut children_vec = Vec::with_capacity(end + 1);
                children_vec.push(shift1(&dag.roots(&dag.all())));
                for rev in 0..dag.parents.len() {
                    let children = dag.children(rev);
                    let revs = if children.is_empty() {
                        end_revs.clone()
                    } else {
                        shift1(children)
                    };
                    children_vec.push(revs);
                }
                children_vec.push(SmallRevs::empty());
                assert_eq!(children_vec.len(), end + 1);

                // postdom[v] = {v} ∪ intersection(postdom[c] for c in children[v])
                // postdom[end] = {end}
                let mut postdom = Vec::new();
                postdom.resize_with(end + 1, SmallRevs::empty);
                postdom[end].insert(end);
                for rev in (0..end).rev() {
                    postdom[rev].insert(rev);
                    let children = &children_vec[rev];
                    let mut revs = SmallRevs::empty();
                    for child in children.iter() {
                        if revs.is_empty() {
                            revs = postdom[child].clone()
                        } else {
                            revs.intersect_with(&postdom[child]);
                        }
                    }
                    postdom[rev].union_with(&revs);
                }

                Self {
                    children_vec,
                    postdom,
                    end,
                }
            }

            /// immediate post-dominator for rev, min(postdom[rev] - {rev}).
            /// useful to decide the end (where to put '}').
            fn ipdom(&self, rev: Rev) -> Rev {
                for p in self.postdom[rev].iter() {
                    if p > rev {
                        return p;
                    }
                }
                // should be unreachable, but just in case...
                self.end
            }

            /// Draw from start (inclusive) to end (exclusive).
            /// Super head and root are skipped.
            fn walk(&self, f: &mut fmt::Formatter, mut start: Rev, end: Rev) -> fmt::Result {
                let mut prefix = "";
                while start < end {
                    if let Some(rev) = start.checked_sub(1) {
                        write!(f, "{prefix}{rev}")?;
                        prefix = "-";
                    }
                    let children = &self.children_vec[start];
                    match children.len() {
                        0 => unreachable!(),
                        1 => {
                            start = children.iter().next().unwrap();
                            continue;
                        }
                        _ => {
                            f.write_str(prefix)?;
                            f.write_char('{')?;
                            let end = self.ipdom(start);
                            let mut sep = "";
                            for child in children.iter() {
                                f.write_str(sep)?;
                                self.walk(f, child, end)?;
                                sep = ",";
                            }
                            f.write_char('}')?;
                            prefix = "-";
                            start = end;
                        }
                    }
                }
                Ok(())
            }
        }

        let state = State::from_nanodag(self);
        state.walk(f, 0, state.end)
    }
}

impl fmt::Debug for NanoDag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NanoDag(")?;
        fmt::Display::fmt(&self, f)?;
        f.write_char(')')
    }
}
