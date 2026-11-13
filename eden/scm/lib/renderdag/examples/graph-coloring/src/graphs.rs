/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Example diagrams are adapted from renderdag's test fixtures.

pub struct Graph {
    pub name: &'static str,
    pub dag: &'static str,
    pub heads: &'static [&'static str],
    pub reserve: &'static [&'static str],
    pub ancestors: &'static [(&'static str, &'static str)],
    pub missing: &'static [&'static str],
    pub public: &'static [&'static str],
}

pub const GRAPHS: &[Graph] = &[
    Graph {
        name: "branches_and_merges",
        dag: r#"
                      T /---------------N--O---\           T
                     / /                        \           \
               /----E-F-\    /-------L--M--------P--\     S--U---\
            A-B-C-D------G--H--I--J--K---------------Q--R---------V--W
                                   \--N
    "#,
        heads: &["W"],
        reserve: &[],
        ancestors: &[],
        missing: &[],
        public: &[],
    },
    Graph {
        name: "octopus",
        dag: r#"
                        /-----\
                       /       \
                      D /--C--\ I
                     / /---D---\ \
                    A-B----E----H-J
                       \---F---/ /
                        \--G--/ F
    "#,
        heads: &["J"],
        reserve: &[],
        ancestors: &[],
        missing: &[],
        public: &[],
    },
    Graph {
        name: "split_parents",
        dag: r#"
                    /-B-\     A-\
                   A     D-E  B--E
                    \-C-/     C-/
    "#,
        heads: &["E"],
        reserve: &["B", "D", "C"],
        ancestors: &[("E", "A"), ("E", "B")],
        missing: &[],
        public: &[],
    },
    Graph {
        name: "stacks",
        dag: r#"
                X1-X2       Y1-Y2-Y3    Z1-Z2
               /           /           /
              B           E           G
            A-B-C-D-E-F-G-H-I
                           \
                            W1-W2
        "#,
        heads: &["I", "X2", "Y3", "Z2", "W2"],
        reserve: &[],
        ancestors: &[],
        missing: &[],
        public: &["A", "B", "C", "D", "E", "F", "G", "H", "I"],
    },
    Graph {
        name: "ancestors",
        dag: r#"
                   A----B-D-----E----------F-\
                    \-C--/       \-W  \-X     \-Y-Z
    "#,
        heads: &["W", "X", "Z"],
        reserve: &["F"],
        ancestors: &[("C", "A"), ("D", "C"), ("E", "D"), ("F", "E")],
        missing: &[],
        public: &[],
    },
    Graph {
        name: "terminations",
        dag: r#"
                   A-B-C  D-E-\
                            F---I--J
                        X-D-H-/  \-K
    "#,
        heads: &["C", "J", "K"],
        reserve: &["E"],
        ancestors: &[("B", "A")],
        missing: &["A", "F", "X"],
        public: &[],
    },
    Graph {
        name: "multi_id_ancestors",
        dag: "P-N-H P-H",
        heads: &["H"],
        reserve: &[],
        ancestors: &[("H", "P"), ("N", "P")],
        missing: &[],
        public: &[],
    },
];
