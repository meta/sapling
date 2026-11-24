/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use nonblocking::non_blocking_result as r;
use tempfile::tempdir;
pub use test_dag::TestDag;

pub use self::drawdag::DrawDag;
use crate::Dag;
use crate::DagAlgorithm;
use crate::IdMap;
#[cfg(test)]
use crate::IdSet;
use crate::Result;
use crate::Set;
use crate::id::Group;
use crate::id::Vertex;
use crate::ops::DagAddHeads;
use crate::ops::DagPersistent;
use crate::ops::ImportAscii;
#[cfg(feature = "render")]
use crate::render::render_dag;
use crate::set::SyncSetQuery;

mod drawdag;
mod test_dag;

#[cfg(test)]
mod test_add_heads;

#[cfg(test)]
mod test_bisect;

#[cfg(test)]
mod test_integrity;

#[cfg(test)]
mod test_sparse;

#[cfg(test)]
mod test_strip;

#[cfg(test)]
mod test_to_parents;

#[cfg(test)]
mod test_discontinuous;

#[cfg(test)]
mod test_server;

#[cfg(test)]
mod test_virtual;

#[cfg(test)]
pub mod dummy_dag;

#[cfg(test)]
pub(crate) use test_dag::ProtocolMonitor;

#[cfg(test)]
use crate::Id;
#[cfg(test)]
use crate::VertexListWithOptions;
use crate::dag::MemDag;
#[cfg(test)]
use crate::iddag::FirstAncestorConstraint;
use crate::ops::IdConvert;
#[cfg(test)]
use crate::protocol::Process;
#[cfg(test)]
use crate::protocol::RequestLocationToName;
#[cfg(test)]
use crate::protocol::RequestNameToLocation;
#[cfg(test)]
use crate::render::render_segment_dag;

// Example from segmented-changelog.pdf
// - DAG1: page 10
// - DAG2: page 11
// - DAG3: page 17
// - DAG4: page 18
// - DAG5: page 19

static ASCII_DAG1: &str = r#"
                C-D-\     /--I--J--\
            A-B------E-F-G-H--------K--L"#;

static ASCII_DAG2: &str = r#"
                      T /---------------N--O---\           T
                     / /                        \           \
               /----E-F-\    /-------L--M--------P--\     S--U---\
            A-B-C-D------G--H--I--J--K---------------Q--R---------V--W
                                   \--N"#;

static ASCII_DAG3: &str = r#"
              B---D---F--\
            A---C---E-----G"#;

static ASCII_DAG4: &str = r#"
             D  C  B
              \  \  \
            A--E--F--G"#;

static ASCII_DAG5: &str = r#"
        B---D---F
         \   \   \
      A---C---E---G"#;

fn test_dag_sort_version<T: DagAlgorithm + IdConvert>(dag: &T) -> Result<()> {
    // Test that sort() returns a set with dag and map hints assigned.
    let sets = [
        set(""),
        set("A C B"),
        r(from_ascii(MemDag::new(), ASCII_DAG3).sort(&set("C A B")))?,
    ];
    for set in sets {
        let sorted = r(dag.sort(&set))?;
        let hints = sorted.hints();
        assert_eq!(hints.dag_version(), Some(dag.dag_version()));
        assert_eq!(hints.id_map_version(), Some(dag.map_version()));
    }

    Ok(())
}

fn test_generic_dag1<T: DagAlgorithm + DagAddHeads + IdConvert>(dag: T) -> Result<T> {
    let dag = from_ascii(dag, ASCII_DAG1);
    assert_eq!(expand(r(dag.all())?), "A B C D E F G H I J K L");
    assert_eq!(expand(r(dag.dirty())?), "A B C D E F G H I J K L");
    assert_eq!(expand(r(dag.ancestors(set("H I")))?), "A B C D E F G H I");
    assert_eq!(expand(r(dag.sort(&set("H E A")))?.skip(1).take(2)), "A E");
    assert_eq!(expand(r(dag.first_ancestors(set("F")))?), "A B E F");
    assert_eq!(expand(r(dag.parents(set("H I E")))?), "B D G");
    assert_eq!(expand(r(dag.children(set("G D L")))?), "E H I");
    assert_eq!(expand(r(dag.merges(r(dag.all())?))?), "E K");
    assert_eq!(expand(r(dag.merges(set("E F J K")))?), "E K");
    assert_eq!(expand(r(dag.merges(set("A B D F H J L")))?), "");
    assert_eq!(expand(r(dag.roots(set("A B E F C D I J")))?), "A C I");
    assert_eq!(expand(r(dag.heads(set("A B E F C D I J")))?), "F J");
    assert_eq!(expand(r(dag.gca_all(set("J K H")))?), "G");

    test_dag_sort_version(&dag)?;

    Ok(dag)
}

fn test_generic_dag_beautify<D: DagAlgorithm + DagAddHeads>(
    new_dag: &impl Fn() -> D,
) -> Result<()> {
    let ascii = r#"
        A C
        | |
        B D
        |/
        E"#;
    let order = ["B", "D", "A", "C"];
    let dag = from_ascii_with_heads(new_dag(), ascii, Some(&order));
    assert_eq!(expand(r(dag.all())?), "A B C D E");

    let dag2 = r(dag.beautify(None))?;
    assert_eq!(expand(r(dag2.all())?), "A B C D E");

    let dag3 = r(dag.beautify(Some(set("A B E"))))?;
    assert_eq!(expand(r(dag3.all())?), "A B C D E");

    let dag4 = r(dag.beautify(Some(set("C D E"))))?;
    assert_eq!(expand(r(dag4.all())?), "A B C D E");

    let ascii = r#"
        A G
        |/
        B F
        |/
        C E
        |/
        D"#;
    let order = ["C", "E", "G", "F", "A"];
    let dag = from_ascii_with_heads(new_dag(), ascii, Some(&order));
    assert_eq!(expand(r(dag.all())?), "A B C D E F G");

    let dag2 = r(dag.beautify(None))?;
    assert_eq!(expand(r(dag2.all())?), "A B C D E F G");

    let dag3 = r(dag.beautify(Some(r(dag.ancestors(set("A")))?)))?;
    assert_eq!(expand(r(dag3.all())?), "A B C D E F G");

    let ascii = r#"
        A---B---C---D---E---F---G
             \
              H---I---J---K
                   \
                    L "#;
    let order = ["D", "J", "L", "K", "G"];
    let dag = from_ascii_with_heads(new_dag(), ascii, Some(&order));
    assert_eq!(expand(r(dag.all())?), "A B C D E F G H I J K L");

    let dag2 = r(dag.beautify(None))?;
    assert_eq!(expand(r(dag2.all())?), "A B C D E F G H I J K L");

    Ok(())
}

fn test_generic_dag_reachable_roots(dag: impl DagAlgorithm + DagAddHeads) -> Result<()> {
    let ascii = r#"
         Z
         |\
         D |
         | F
         C |
         | E
         B |
         |/
         A
         "#;
    let dag = from_ascii_with_heads(dag, ascii, Some(&["Z"][..]));

    // B is not reachable without going through other roots (C).
    // A is reachable through Z -> F -> E -> A.
    assert_eq!(
        expand(r(dag.reachable_roots(set("A B C"), set("Z")))?),
        "A C"
    );

    // A, E are not reachable without going through other roots (C, F).
    assert_eq!(
        expand(r(dag.reachable_roots(set("A C E F"), set("Z")))?),
        "C F"
    );

    // roots and heads overlap.
    assert_eq!(
        expand(r(dag.reachable_roots(set("A B C D E F Z"), set("D F")))?),
        "D F"
    );

    // E, F are not reachable.
    assert_eq!(
        expand(r(dag.reachable_roots(set("A B E F"), set("D")))?),
        "B"
    );

    // "Bogus" root "Z".
    assert_eq!(expand(r(dag.reachable_roots(set("A Z"), set("C")))?), "A");

    Ok(())
}

// This test is specific to the implementation in this crate, and requires
// >2 parents support.
fn test_specific_dag_import(dag: impl DagAlgorithm + DagAddHeads) -> Result<()> {
    let ascii = r#"
            J K
           /|\|\
          G H I H
          |/|/|
          E F |
         /|/|\|
        A B C D"#;
    let dag1 = from_ascii_with_heads(dag, ascii, Some(&["J", "K"][..]));

    let dir = tempdir().unwrap();
    let mut dag2 = Dag::open(dir.path())?;
    r(dag2.import_and_flush(&dag1, set("J")))?;
    #[cfg(feature = "render")]
    assert_eq!(
        render(&dag2),
        r#"
            K
            ├─╮
            │ │ J
            ╭─┬─┤
            │ I │
            │ ├───╮
            H │ │ │
            ├─────╮
            │ │ │ F
            │ ╭───┼─╮
            │ D │ │ │
            │   │ │ │
            │   │ │ C
            │   │ │
            │   G │
            ├───╯ │
            E     │
            ├─────╮
            │     B
            │
            A"#
    );

    // Check that dag2 is actually flushed to disk.
    let dag3 = Dag::open(dir.path())?;
    #[cfg(feature = "render")]
    assert_eq!(
        render(&dag3),
        r#"
            K
            ├─╮
            │ │ J
            ╭─┬─┤
            │ I │
            │ ├───╮
            H │ │ │
            ├─────╮
            │ │ │ F
            │ ╭───┼─╮
            │ D │ │ │
            │   │ │ │
            │   │ │ C
            │   │ │
            │   G │
            ├───╯ │
            E     │
            ├─────╮
            │     B
            │
            A"#
    );
    Ok(())
}

fn test_generic_dag2<T: DagAlgorithm + DagAddHeads>(dag: T) -> Result<T> {
    let ascii = r#"
            J K
           / \|\
          G H I H
          |/|/|
          E F |
         /|/| |
        A B C D"#;
    let dag = from_ascii_with_heads(dag, ascii, Some(&["J", "K"][..]));

    let v = |name: &str| -> Vertex { Vertex::copy_from(name.as_bytes()) };

    assert_eq!(expand(r(dag.all())?), "A B C D E F G H I J K");
    assert_eq!(expand(r(dag.ancestors(set("H I")))?), "A B C D E F H I");
    assert_eq!(expand(r(dag.first_ancestors(set("H I")))?), "A D E H I");
    assert_eq!(expand(r(dag.first_ancestors(set("J G D")))?), "A D E G J");
    assert_eq!(expand(r(dag.parents(set("H I E")))?), "A B D E F");
    assert_eq!(r(dag.first_ancestor_nth(v("H"), 2))?.unwrap(), v("A"));
    assert!(r(dag.first_ancestor_nth(v("H"), 3))?.is_none());
    assert_eq!(expand(r(dag.heads(set("E H F K I D")))?), "K");
    assert_eq!(expand(r(dag.children(set("E F I")))?), "G H I J K");
    assert_eq!(expand(r(dag.merges(r(dag.all())?))?), "E F H I J K");
    assert_eq!(expand(r(dag.merges(set("E H G D I")))?), "E H I");
    assert_eq!(expand(r(dag.roots(set("E G H J I K D")))?), "D E");
    assert_eq!(r(dag.gca_one(set("J K")))?, Some(v("I")));
    assert_eq!(expand(r(dag.gca_all(set("J K")))?), "E I");
    assert_eq!(expand(r(dag.common_ancestors(set("G H")))?), "A B E");
    assert!(r(dag.is_ancestor(v("B"), v("K")))?);
    assert!(!r(dag.is_ancestor(v("K"), v("B")))?);
    assert_eq!(expand(r(dag.heads_ancestors(set("A E F D G")))?), "D F G");
    assert_eq!(expand(r(dag.range(set("A"), set("K")))?), "A E H K");
    assert_eq!(expand(r(dag.only(set("I"), set("G")))?), "C D F I");
    let (reachable, unreachable) = r(dag.only_both(set("I"), set("G")))?;
    assert_eq!(expand(reachable), "C D F I");
    assert_eq!(expand(unreachable), expand(r(dag.ancestors(set("G")))?));
    assert_eq!(expand(r(dag.descendants(set("F E")))?), "E F G H I J K");

    assert!(r(dag.is_ancestor(v("B"), v("J")))?);
    assert!(r(dag.is_ancestor(v("F"), v("F")))?);
    assert!(!r(dag.is_ancestor(v("K"), v("I")))?);

    Ok(dag)
}

#[test]
fn test_import_ascii_with_vertex_fn() {
    let ascii = r#"
        B C
        |/
        A
    "#;
    let mut dag = MemDag::new();
    let vertex_fn = |s: &str| -> Vertex {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(b'0');
        Vertex::copy_from(&bytes)
    };
    dag.import_ascii_with_vertex_fn(ascii, vertex_fn).unwrap();
    assert_eq!(expand(r(dag.all()).unwrap()), "A0 B0 C0");
}

#[test]
fn test_mem_dag() {
    let new_dag = MemDag::new;
    test_generic_dag(&new_dag);
    test_specific_dag_import(new_dag()).unwrap();
}

#[test]
fn test_dag() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    let dir = tempdir().unwrap();
    let counter = AtomicUsize::new(0);
    let new_dag = move || {
        let count = counter.fetch_add(1, Ordering::AcqRel);
        Dag::open(dir.path().join(count.to_string())).unwrap()
    };
    test_generic_dag(&new_dag);
    test_specific_dag_import(new_dag()).unwrap();
}

#[test]
fn test_protocols() {
    let mut built = build_segments(ASCII_DAG1, "A C E L", 3);
    assert_eq!(
        built.ascii[3],
        r#"
                1-2-\     /--7--8--\
            0-3------4-5-6-9--------10-11
Lv0: RH0-0[] R1-2[] 3-3[0] H4-8[3, 2] 9-9[6] H10-11[9, 8]
Lv1: R0-0[] R1-8[0]"#
    );

    // Replace "[66]" to "B", "[67]" to "C", etc.
    let replace = |mut s: String| -> String {
        for ch in "ABCDEFGHIJKL".chars() {
            s = s.replace(&format!("[{}]", ch as u8), &format!("{ch}"));
        }
        s
    };

    // [Id] -> RequestLocationToName (useful for getting commit hashes from ids).
    // 3 (D) and 9 (J) are p2 that cannot be resolved.
    let ids: Vec<Id> = b"ABCEFGHI"
        .iter()
        .map(|&b| built.name_dag.map.find_id_by_name(&[b]).unwrap().unwrap())
        .collect();
    let ids = IdSet::from_spans(ids);
    let request1: RequestLocationToName =
        r((&built.name_dag.map, &built.name_dag.dag).process(ids)).unwrap();
    assert_eq!(
        replace(dbg(&request1)),
        "RequestLocationToName { paths: [L~2, J~1(+5), D~1, B~1] }"
    );

    // [name] -> RequestNameToLocation (useful for getting ids from commit hashes).
    let names = b"ABCEFGHI"
        .iter()
        .map(|&b| Vertex::copy_from(&[b]))
        .collect();
    let request2: RequestNameToLocation =
        r((&built.name_dag.map, &built.name_dag.dag).process(names)).unwrap();
    assert_eq!(
        replace(dbg(&request2)),
        "RequestNameToLocation { names: [A, B, C, E, F, G, H, I], heads: [L] }"
    );

    // RequestLocationToName -> ResponseIdNamePair
    let response1 = r((&built.name_dag.map, &built.name_dag.dag).process(request1)).unwrap();
    assert_eq!(
        replace(dbg(&response1)),
        "ResponseIdNamePair { path_names: [(L~2, [H]), (J~1(+5), [I, G, F, E, B]), (D~1, [C]), (B~1, [A])] }"
    );

    // RequestNameToLocation -> ResponseIdNamePair
    // Only B, D, H, J, L are used since they are "universally known".
    let response2 = r((&built.name_dag.map, &built.name_dag.dag).process(request2)).unwrap();
    assert_eq!(
        replace(dbg(&response2)),
        "ResponseIdNamePair { path_names: [(B~1, [A]), (H~4, [B]), (D~1, [C]), (H~3, [E]), (H~2, [F]), (H~1, [G]), (L~2, [H]), (J~1, [I])] }"
    );

    // Applying responses to IdMap. Should not cause errors.
    r((&mut built.name_dag.map, &built.name_dag.dag).process(response1)).unwrap();
    r((&mut built.name_dag.map, &built.name_dag.dag).process(response2.clone())).unwrap();

    // Try applying response2 to a sparse IdMap.
    // Prepare the sparse IdMap.
    let mut sparse_id_map = IdMap::open(built.dir.path().join("sparse-id")).unwrap();
    r(built
        .name_dag
        .dag
        .write_sparse_idmap(&built.name_dag.map, &mut sparse_id_map))
    .unwrap();
    assert_eq!(
        dbg(&sparse_id_map),
        "IdMap {\n  D: 2,\n  B: 3,\n  J: 8,\n  H: 9,\n  L: 11,\n}\n"
    );
    // Apply response2.
    r((&mut sparse_id_map, &built.name_dag.dag).process(response2)).unwrap();
    assert_eq!(
        dbg(&sparse_id_map),
        r#"IdMap {
  A: 0,
  C: 1,
  D: 2,
  B: 3,
  E: 4,
  F: 5,
  G: 6,
  I: 7,
  J: 8,
  H: 9,
  L: 11,
}
"#
    );
}

#[test]
fn test_segment_non_master() {
    let ascii = r#"
a----b----c----d----e----f----g----------h----i
     \                    \             /
      h---i---j---k        l---m---n---o
               \                \
                -----------------p---q"#;
    let built = build_segments(ascii, "i q", 3);
    assert_eq!(
        built.ascii[0],
        r#"
N0---N1---N2---N3---N4---N5---N6---------N11--N12
     \                    \             /
      N11-N12-j---k        N7--N8--N9--N10
               \                \
                -----------------p---q
Lv0: RN0-N6[] N7-N10[N5] N11-N12[N0, N6, N10]"#
    );
    assert_eq!(
        built.ascii[1],
        r#"
N0---N1---N2---N3---N4---N5---N6---------N11--N12
     \                    \             /
      N11-N12-N13-k        N7--N8--N9--N10
               \                \
                -----------------N14-N15
Lv0: RN0-N6[] N7-N10[N5] N11-N12[N0, N6, N10] N13-N13[N12] N14-N15[N13, N8]
Lv1: RN0-N12[]"#
    );
}

#[test]
fn test_segment_examples() {
    assert_eq!(
        build_segments(ASCII_DAG1, "L", 3).ascii[0],
        r#"
                2-3-\     /--8--9--\
            0-1------4-5-6-7--------10-11
Lv0: RH0-1[] R2-3[] H4-7[1, 3] 8-9[6] H10-11[7, 9]
Lv1: R0-7[]"#
    );

    assert_eq!(
        build_segments(ASCII_DAG2, "W", 3).ascii[0],
        r#"
                      19/---------------13-14--\           19
                     / /                        \           \
               /----4-5-\    /-------11-12-------15-\     18-20--\
            0-1-2-3------6--7--8--9--10--------------16-17--------21-22
                                   \--13
Lv0: RH0-3[] 4-5[1] H6-10[3, 5] 11-12[7] 13-14[5, 9] 15-15[12, 14] H16-17[10, 15] R18-18[] 19-19[4] 20-20[18, 19] H21-22[17, 20]
Lv1: R0-10[] 11-15[7, 5, 9] 16-17[10, 15] R18-20[4]
Lv2: R0-17[]"#
    );

    assert_eq!(
        build_segments(ASCII_DAG3, "G", 3).ascii[0],
        r#"
              3---4---5--\
            0---1---2-----6
Lv0: RH0-2[] R3-5[] H6-6[2, 5]"#
    );

    assert_eq!(
        build_segments(ASCII_DAG4, "G", 3).ascii[0],
        r#"
             3  1  0
              \  \  \
            2--4--5--6
Lv0: RH0-0[] R1-1[] R2-2[] R3-3[] 4-4[2, 3] 5-5[1, 4] H6-6[0, 5]
Lv1: R0-0[] R1-1[] R2-4[]"#
    );

    assert_eq!(
        build_segments(ASCII_DAG5, "G", 3).ascii[0],
        r#"
        1---3---5
         \   \   \
      0---2---4---6
Lv0: RH0-0[] R1-1[] H2-2[0, 1] 3-3[1] H4-4[2, 3] 5-5[3] H6-6[4, 5]
Lv1: R0-2[] 3-4[1, 2]"#
    );

    // Examples outside segmented-changelog.pdf

    // For this graph, the numbers should look continuous in one direction.
    let ascii_dag = r#"
            Z---E--M--J--C
                 \  \  \  \
                  O--T--D--L
                   \  \  \  \
                    K--H--P--W
                     \  \  \  \
                      X--R--U--V
                       \  \  \  \
                        A--N--S--Y"#;
    assert_eq!(
        build_segments(ascii_dag, "Y", 3).ascii[0],
        r#"
            0---1--6--11-16
                 \  \  \  \
                  2--7--12-17
                   \  \  \  \
                    3--8--13-18
                     \  \  \  \
                      4--9--14-19
                       \  \  \  \
                        5--10-15-20
Lv0: RH0-5[] 6-6[1] 7-7[6, 2] 8-8[3, 7] 9-9[8, 4] H10-10[5, 9] 11-11[6] 12-12[11, 7] 13-13[12, 8] 14-14[13, 9] H15-15[10, 14] 16-16[11] 17-17[16, 12] 18-18[17, 13] 19-19[14, 18] H20-20[15, 19]
Lv1: R0-5[] 6-8[1, 2, 3] 9-10[8, 4, 5] 11-13[6, 7, 8] 14-15[13, 9, 10] 16-18[11, 12, 13]
Lv2: R0-10[] 11-15[6, 7, 8, 9, 10]"#
    );

    // If a graph looks like this, it's hard to optimize anyway.
    let ascii_dag = r#"
            Z---E--J--C--O--T
                 \     \     \
                  D     L     K
                   \     \     \
                    H--P--W--X--R
                     \     \     \
                      U     V     A
                       \     \     \
                        N--S--Y--B--G"#;
    assert_eq!(
        build_segments(ascii_dag, "G", 3).ascii[0],
        r#"
            0---1--2--3--4--5
                 \     \     \
                  8     7     6
                   \     \     \
                    9--10-11-12-13
                     \     \     \
                      15    18    14
                       \     \     \
                        16-17-19-20-21
Lv0: RH0-6[] 7-7[3] 8-10[1] 11-12[7, 10] H13-14[6, 12] 15-17[9] 18-18[11] 19-20[17, 18] H21-21[14, 20]
Lv1: R0-6[] 7-12[3, 1] 13-14[6, 12] 15-20[9, 11]
Lv2: R0-14[]"#
    );
}

#[test]
fn test_segment_groups() {
    let dag = r#"
A---B---C---D---E---F---G--------H---I
     \               \          /
      h--i--j--k      l--m--n--o
                \            \
                 -------------p---q"#;

    // This test involves many things. Lower-case commits are non-master commits.
    // - D after B: Test incremental build of a master commit with a master parent.
    // - i after D: Test non-master with master parent.
    // - k after i: Test non-master with non-master parent.
    // - q after G: Test non-master with both master and non-master ancestors.
    // - I after q: Test overwriting non-master Ids with master Ids (!).
    let built = build_segments(dag, "B D i k G q I", 3);
    assert_eq!(
        built.ascii.join("\n"),
        r#"
0---1---C---D---E---F---G--------H---I
     \               \          /
      h--i--j--k      l--m--n--o
                \            \
                 -------------p---q
Lv0: RH0-1[]

0---1---2---3---E---F---G--------H---I
     \               \          /
      h--i--j--k      l--m--n--o
                \            \
                 -------------p---q
Lv0: RH0-3[]

0---1---2---3---E---F---G--------H---I
     \               \          /
      N0-N1-j--k      l--m--n--o
                \            \
                 -------------p---q
Lv0: RH0-3[] N0-N1[1]

0---1---2---3---E---F---G--------H---I
     \               \          /
      N0-N1-N2-N3     l--m--n--o
                \            \
                 -------------p---q
Lv0: RH0-3[] N0-N1[1] N2-N3[N1]

0---1---2---3---4---5---6--------H---I
     \               \          /
      N0-N1-N2-N3     l--m--n--o
                \            \
                 -------------p---q
Lv0: RH0-6[] N0-N3[1]

0---1---2---3---4---5---6--------H---I
     \               \          /
      N0-N1-N2-N3     N4-N5-N6-o
                \            \
                 -------------N7--N8
Lv0: RH0-6[] N0-N3[1] N4-N6[5] N7-N8[N3, N6]

0---1---2---3---4---5---6--------11--12
     \               \          /
      N0-N1-N2-N3     7--8--9--10
                \            \
                 -------------N4--N5
Lv0: RH0-6[] 7-10[5] H11-12[6, 10] N0-N3[1] N4-N5[N3, 9]"#
    );

    // Notice that N4 to N6 were re-written in the last step.
    // 'm' only has 1 id: 8 (master). The old id (N5) is now taken by 'q'.
    assert_eq!(
        built.name_dag.map.find_id_by_name(b"m").unwrap().unwrap(),
        Id(8)
    );
    assert_eq!(
        built.name_dag.map.find_name_by_id(Id(8)).unwrap().unwrap(),
        b"m"
    );
    let id = Group::NON_MASTER.min_id() + 5;
    assert_eq!(
        built.name_dag.map.find_name_by_id(id).unwrap().unwrap(),
        b"q"
    );

    // Parent-child indexes work fine.
    assert_eq!(dbg(built.name_dag.dag.children_id(Id(5)).unwrap(),), "6 7");
}

#[test]
fn test_dag_reassign_master() -> crate::Result<()> {
    let dir = tempdir().unwrap();
    let mut dag = Dag::open(dir.path())?;
    dag = from_ascii(dag, "A-B-C");

    // The in-memory DAG can answer parent_names questions.
    assert_eq!(dbg(r(dag.parent_names("A".into()))?), "[]");
    assert_eq!(dbg(r(dag.parent_names("C".into()))?), "[B]");

    // First flush, A, B, C are non-master.
    r(dag.flush(&Default::default())).unwrap();

    assert_eq!(dbg(r(dag.vertex_id("A".into()))?), "N0");
    assert_eq!(dbg(r(dag.vertex_id("C".into()))?), "N2");

    // Second flush, making B master without adding new vertexes.
    let heads =
        VertexListWithOptions::from(vec![Vertex::from("B")]).with_desired_group(Group::MASTER);
    r(dag.flush(&heads)).unwrap();
    assert_eq!(dbg(r(dag.vertex_id("A".into()))?), "0");
    assert_eq!(dbg(r(dag.vertex_id("B".into()))?), "1");
    assert_eq!(dbg(r(dag.vertex_id("C".into()))?), "N0");

    Ok(())
}

#[test]
fn test_dag_reassign_non_master() {
    let mut t = TestDag::new();

    // A: master; B, Z: non-master.
    t.drawdag("A--B--Z", &["A"]);
    // C, D, E: non-master.
    t.drawdag("B--C--D--E", &[]);
    // Prompt C to master. Triggers non-master reassignment.
    t.drawdag("", &["C"]);

    // Z still exists.
    #[cfg(feature = "render")]
    assert_eq!(
        t.render_graph(),
        r#"
            Z  N2
            │
            │ E  N1
            │ │
            │ D  N0
            │ │
            │ C  2
            ├─╯
            B  1
            │
            A  0"#
    );

    // Z can round-trip in IdMap.
    let z_id = r(t.dag.vertex_id("Z".into())).unwrap();
    let z_vertex = r(t.dag.vertex_name(z_id)).unwrap();
    assert_eq!(dbg(z_vertex), "Z");
}

#[test]
fn test_segment_ancestors_example1() {
    // DAG from segmented-changelog.pdf
    let ascii_dag = r#"
            2-3-\     /--8--9--\
        0-1------4-5-6-7--------10-11"#;
    let result = build_segments(ascii_dag, "11", 3);
    let dag = result.name_dag.dag;

    for (id, count) in &[
        (11, 12),
        (10, 11),
        (9, 9),
        (8, 8),
        (7, 8),
        (6, 7),
        (5, 6),
        (4, 5),
        (3, 2),
        (2, 1),
        (1, 2),
        (0, 1),
    ] {
        assert_eq!(dag.ancestors((*id).into()).unwrap().count(), *count);
    }

    for a in (0..=11).map(Id) {
        for b in (0..=11).map(Id) {
            assert_eq!(
                dag.is_ancestor(a, b).unwrap(),
                dag.ancestors(b.into()).unwrap().contains(a),
                "is_ancestor({a:?}, {b:?})"
            );
        }
    }

    for (a, b, ancestor) in [
        (10, 3, 3.into()),
        (11, 0, 0.into()),
        (11, 10, 10.into()),
        (11, 9, 9.into()),
        (3, 0, None),
        (7, 1, 1.into()),
        (9, 2, 2.into()),
        (9, 7, 6.into()),
    ] {
        let ancestor = ancestor.map(Id);
        let a = Id(a);
        let b = Id(b);
        assert_eq!(dag.gca_one((a, b).into()).unwrap(), ancestor);
        assert_eq!(
            dag.gca_all((a, b).into()).unwrap().iter_desc().nth(0),
            ancestor
        );
        assert_eq!(dag.gca_all((a, b).into()).unwrap().iter_desc().nth(1), None);
        assert_eq!(dag.is_ancestor(b, a).unwrap(), ancestor == Some(b));
        assert_eq!(dag.is_ancestor(a, b).unwrap(), ancestor == Some(a));
    }

    for (spans, ancestors) in [
        (vec![3..=8], vec![3]),
        (vec![1..=1, 4..=9], vec![1]),
        (vec![1..=4], vec![]),
    ] {
        assert_eq!(
            dag.gca_all(IdSet::from_spans(spans))
                .unwrap()
                .iter_desc()
                .collect::<Vec<Id>>(),
            ancestors.into_iter().map(Id).collect::<Vec<Id>>(),
        );
    }
}

#[test]
fn test_segment_multiple_gcas() {
    let ascii_dag = r#"
        B---C
         \ /
        A---D"#;
    let result = build_segments(ascii_dag, "C D", 3);
    assert_eq!(
        result.ascii[1],
        r#"
        1---2
         \ /
        0---3
Lv0: RH0-0[] R1-1[] H2-2[0, 1] 3-3[0, 1]
Lv1: R0-2[]"#
    );
    let dag = result.name_dag.dag;
    // This is kind of "undefined" whether it's 1 or 0.
    assert_eq!(dag.gca_one((2, 3).into()).unwrap(), Some(Id(1)));
    assert_eq!(
        dag.gca_all((2, 3).into())
            .unwrap()
            .iter_desc()
            .collect::<Vec<_>>(),
        vec![1, 0]
    );
}

#[test]
fn test_parents() {
    let result = build_segments(ASCII_DAG1, "L", 3);
    assert_eq!(
        result.ascii[0],
        r#"
                2-3-\     /--8--9--\
            0-1------4-5-6-7--------10-11
Lv0: RH0-1[] R2-3[] H4-7[1, 3] 8-9[6] H10-11[7, 9]
Lv1: R0-7[]"#
    );

    let dag = result.name_dag.dag;

    let parents = |spans| -> String { dbg(dag.parents(IdSet::from_spans(spans)).unwrap()) };
    let parent_ids = |id| -> String { dbg(dag.parent_ids(Id(id)).unwrap()) };
    let first_ancestor_nth = |id, n| -> String { dbg(dag.first_ancestor_nth(Id(id), n).unwrap()) };
    let to_first_ancestor_nth = |id| -> String {
        let c = FirstAncestorConstraint::KnownUniversally {
            heads: Id(11).into(),
        };
        let res = dag.to_first_ancestor_nth(Id(id), c);
        match res {
            Ok(s) => dbg(s),
            Err(e) => e.to_string(),
        }
    };

    assert_eq!(parents(vec![]), "");

    assert_eq!(parents(vec![0..=0]), "");
    assert_eq!(parents(vec![0..=1]), "0");
    assert_eq!(parents(vec![0..=2]), "0");
    assert_eq!(parents(vec![0..=3]), "0 2");
    assert_eq!(parents(vec![0..=4]), "0..=3");
    assert_eq!(parents(vec![0..=5]), "0..=4");
    assert_eq!(parents(vec![0..=6]), "0..=5");
    assert_eq!(parents(vec![0..=7]), "0..=6");
    assert_eq!(parents(vec![0..=8]), "0..=6");
    assert_eq!(parents(vec![0..=9]), "0..=6 8");
    assert_eq!(parents(vec![0..=10]), "0..=9");
    assert_eq!(parents(vec![0..=11]), "0..=10");

    assert_eq!(parents(vec![0..=0, 2..=2]), "");
    assert_eq!(parents(vec![0..=0, 3..=3, 5..=5, 9..=10]), "2 4 7 8 9");
    assert_eq!(parents(vec![1..=1, 4..=4, 6..=6, 8..=11]), "0 1 3 5..=10");

    assert_eq!(parent_ids(0), "[]");
    assert_eq!(parent_ids(1), "[0]");
    assert_eq!(parent_ids(4), "[1, 3]");
    assert_eq!(parent_ids(10), "[7, 9]");
    assert_eq!(parent_ids(11), "[10]");

    assert_eq!(first_ancestor_nth(0, 0), "0");
    assert_eq!(first_ancestor_nth(4, 2), "0");
    assert_eq!(first_ancestor_nth(10, 2), "6");
    assert_eq!(first_ancestor_nth(10, 3), "5");
    assert_eq!(first_ancestor_nth(11, 0), "11");
    assert_eq!(first_ancestor_nth(11, 1), "10");
    assert_eq!(first_ancestor_nth(11, 2), "7");
    assert_eq!(first_ancestor_nth(11, 3), "6");
    assert_eq!(first_ancestor_nth(11, 4), "5");
    assert_eq!(first_ancestor_nth(11, 6), "1");
    assert_eq!(first_ancestor_nth(11, 7), "0");
    assert!(dag.first_ancestor_nth(Id::MIN, 1).is_err());
    assert!(dag.first_ancestor_nth(Id(11), 8).is_err());

    assert_eq!(to_first_ancestor_nth(0), "Some((1, 1))");
    assert_eq!(to_first_ancestor_nth(1), "Some((9, 5))");
    assert_eq!(to_first_ancestor_nth(2), "Some((3, 1))");
    assert_eq!(
        to_first_ancestor_nth(3),
        "ProgrammingError: cannot convert 3 to x~n form (x must be in `H + parents(ancestors(H) & merge())` where H = 11) (trace: in seg R2-3[], 3 has child seg (H4-7[1, 3]), child seg cannot be followed (3 is not p1))"
    );
    assert_eq!(to_first_ancestor_nth(4), "Some((9, 4))");
    assert_eq!(to_first_ancestor_nth(5), "Some((9, 3))");
    assert_eq!(to_first_ancestor_nth(6), "Some((9, 2))");
    assert_eq!(to_first_ancestor_nth(7), "Some((11, 2))");
    assert_eq!(to_first_ancestor_nth(8), "Some((9, 1))");
    assert_eq!(
        to_first_ancestor_nth(9),
        "ProgrammingError: cannot convert 9 to x~n form (x must be in `H + parents(ancestors(H) & merge())` where H = 11) (trace: in seg 8-9[6], 9 has child seg (H10-11[7, 9]), child seg cannot be followed (9 is not p1))"
    );
    assert_eq!(to_first_ancestor_nth(10), "Some((11, 1))");
    assert_eq!(to_first_ancestor_nth(11), "Some((11, 0))");
}

#[test]
fn test_children() {
    let result = build_segments(ASCII_DAG1, "L", 3);
    let dag = result.name_dag.dag;
    let children = |spans| -> String { dbg(dag.children(IdSet::from_spans(spans)).unwrap()) };

    // See test_parents above for the ASCII DAG.

    assert_eq!(children(vec![]), "");
    assert_eq!(children(vec![0..=0]), "1");

    assert_eq!(children(vec![0..=1]), "1 4");
    assert_eq!(children(vec![0..=2]), "1 3 4");
    assert_eq!(children(vec![0..=3]), "1 3 4");
    assert_eq!(children(vec![0..=4]), "1 3 4 5");
    assert_eq!(children(vec![0..=5]), "1 3..=6");
    assert_eq!(children(vec![0..=6]), "1 3..=8");
    assert_eq!(children(vec![0..=7]), "1 3..=8 10");
    assert_eq!(children(vec![0..=8]), "1 3..=10");
    assert_eq!(children(vec![0..=9]), "1 3..=10");
    assert_eq!(children(vec![0..=10]), "1 3..=11");
    assert_eq!(children(vec![0..=11]), "1 3..=11");

    assert_eq!(children(vec![1..=10]), "3..=11");
    assert_eq!(children(vec![2..=10]), "3..=11");
    assert_eq!(children(vec![3..=10]), "4..=11");
    assert_eq!(children(vec![4..=10]), "5..=11");
    assert_eq!(children(vec![5..=10]), "6..=11");
    assert_eq!(children(vec![6..=10]), "7..=11");
    assert_eq!(children(vec![7..=10]), "9 10 11");
    assert_eq!(children(vec![8..=10]), "9 10 11");
    assert_eq!(children(vec![9..=10]), "10 11");
    assert_eq!(children(vec![10..=10]), "11");

    assert_eq!(children(vec![0..=0, 2..=2]), "1 3");
    assert_eq!(children(vec![0..=0, 3..=3, 5..=5, 9..=10]), "1 4 6 10 11");
    assert_eq!(children(vec![1..=1, 4..=4, 6..=6, 10..=10]), "4 5 7 8 11");
}

#[test]
fn test_heads() {
    let ascii = r#"
    C G   K L
    | |\  |/
    B E F I J
    | |/  |/
    A D   H"#;

    let result = build_segments(ascii, "C G K L J", 2);
    assert_eq!(
        result.ascii[4],
        r#"
    2 6   9 10
    | |\  |/
    1 5 4 8 11
    | |/  |/
    0 3   7
Lv0: RH0-2[] R3-4[] 5-5[3] 6-6[5, 4] R7-9[] 10-10[8] 11-11[7]
Lv1: R0-2[] R3-4[] 5-6[3, 4] R7-9[] 10-10[8]
Lv2: R0-2[] R3-6[] R7-9[]"#
    );

    let dag = result.name_dag.dag;
    let heads = |spans| -> String { dbg(dag.heads(IdSet::from_spans(spans)).unwrap()) };

    assert_eq!(heads(vec![]), "");
    assert_eq!(heads(vec![0..=11]), "2 6 9 10 11");
    assert_eq!(heads(vec![0..=1, 3..=5, 7..=10]), "1 4 5 9 10");
    assert_eq!(heads(vec![0..=0, 2..=2]), "0 2");
    assert_eq!(heads(vec![1..=2, 4..=6, 7..=7, 11..=11, 9..=9]), "2 6 9 11");
}

#[test]
fn test_roots() {
    let ascii = r#"
    C G   J
    | |\  |\
    B E F I K
    | |/  |\
    A D   H L"#;

    let result = build_segments(ascii, "C G J", 2);
    assert_eq!(
        result.ascii[2],
        r#"
    2 6   11
    | |\  |\
    1 5 4 107
    | |/  |\
    0 3   9 8
Lv0: RH0-2[] R3-4[] 5-5[3] 6-6[5, 4] R7-7[] R8-8[] R9-9[] 10-10[9, 8] 11-11[10, 7]
Lv1: R0-2[] R3-4[] 5-6[3, 4] R7-7[] R8-8[] R9-10[8]
Lv2: R0-2[] R3-6[] R7-7[]"#
    );

    let dag = result.name_dag.dag;
    let roots = |spans| -> String { dbg(dag.roots(IdSet::from_spans(spans)).unwrap()) };

    assert_eq!(roots(vec![]), "");
    assert_eq!(roots(vec![0..=11]), "0 3 7 8 9");
    assert_eq!(roots(vec![1..=2, 4..=6, 8..=10]), "1 4 5 8 9");
    assert_eq!(roots(vec![0..=0, 2..=3, 5..=6, 9..=11]), "0 2 3 9");
    assert_eq!(roots(vec![1..=1, 3..=3, 6..=8, 11..=11]), "1 3 6 7 8");
}

#[test]
fn test_range() {
    let ascii = r#"
            J
           /|\
          G H I
          |/|/
          E F
         /|/|\
        A B C D"#;

    let result = build_segments(ascii, "J", 2);
    assert_eq!(
        result.ascii[0],
        r#"
            9
           /|\
          3 7 8
          |/|/
          2 6
         /|/|\
        0 1 4 5
Lv0: RH0-0[] R1-1[] H2-3[0, 1] R4-4[] R5-5[] 6-6[1, 4, 5] 7-7[2, 6] 8-8[6] H9-9[3, 7, 8]
Lv1: R0-0[] R1-3[0] R4-4[] R5-6[1, 4] 7-7[2, 6]
Lv2: R0-3[] R4-6[1]"#
    );

    let dag = result.name_dag.dag;
    let range = |roots, heads| -> String {
        dbg(dag
            .range(IdSet::from_spans(roots), IdSet::from_spans(heads))
            .unwrap())
    };

    assert_eq!(range(vec![6], vec![3]), "");
    assert_eq!(range(vec![1], vec![3, 8]), "1 2 3 6 8");
    assert_eq!(range(vec![4], vec![3, 8]), "4 6 8");
    assert_eq!(range(vec![0, 5], vec![7]), "0 2 5 6 7");
    assert_eq!(range(vec![0, 5], vec![3, 8]), "0 2 3 5 6 8");
    assert_eq!(range(vec![0, 1, 4, 5], vec![3, 7, 8]), "0..=8");

    assert_eq!(range(vec![0], vec![0]), "0");
    assert_eq!(range(vec![0], vec![1]), "");
    assert_eq!(range(vec![0], vec![2]), "0 2");
    assert_eq!(range(vec![0], vec![3]), "0 2 3");
    assert_eq!(range(vec![0], vec![4]), "");
    assert_eq!(range(vec![0], vec![5]), "");
    assert_eq!(range(vec![0], vec![6]), "");
    assert_eq!(range(vec![0], vec![7]), "0 2 7");
    assert_eq!(range(vec![0], vec![8]), "");
    assert_eq!(range(vec![0], vec![9]), "0 2 3 7 9");
    assert_eq!(range(vec![1], vec![1]), "1");
    assert_eq!(range(vec![1], vec![2]), "1 2");
    assert_eq!(range(vec![1], vec![3]), "1 2 3");
    assert_eq!(range(vec![1], vec![4]), "");
    assert_eq!(range(vec![1], vec![5]), "");
    assert_eq!(range(vec![1], vec![6]), "1 6");
    assert_eq!(range(vec![1], vec![7]), "1 2 6 7");
    assert_eq!(range(vec![1], vec![8]), "1 6 8");
    assert_eq!(range(vec![1], vec![9]), "1 2 3 6..=9");
    assert_eq!(range(vec![2], vec![2]), "2");
    assert_eq!(range(vec![2], vec![3]), "2 3");
    assert_eq!(range(vec![2], vec![4]), "");
    assert_eq!(range(vec![2], vec![5]), "");
    assert_eq!(range(vec![2], vec![6]), "");
    assert_eq!(range(vec![2], vec![7]), "2 7");
    assert_eq!(range(vec![2], vec![8]), "");
    assert_eq!(range(vec![2], vec![9]), "2 3 7 9");
    assert_eq!(range(vec![3], vec![3]), "3");
    assert_eq!(range(vec![3], vec![4]), "");
    assert_eq!(range(vec![3], vec![5]), "");
    assert_eq!(range(vec![3], vec![6]), "");
    assert_eq!(range(vec![3], vec![7]), "");
    assert_eq!(range(vec![3], vec![8]), "");
    assert_eq!(range(vec![3], vec![9]), "3 9");
    assert_eq!(range(vec![4], vec![4]), "4");
    assert_eq!(range(vec![4], vec![5]), "");
    assert_eq!(range(vec![4], vec![6]), "4 6");
    assert_eq!(range(vec![4], vec![7]), "4 6 7");
    assert_eq!(range(vec![4], vec![8]), "4 6 8");
    assert_eq!(range(vec![4], vec![9]), "4 6..=9");
    assert_eq!(range(vec![5], vec![5]), "5");
    assert_eq!(range(vec![5], vec![6]), "5 6");
    assert_eq!(range(vec![5], vec![7]), "5 6 7");
    assert_eq!(range(vec![5], vec![8]), "5 6 8");
    assert_eq!(range(vec![5], vec![9]), "5..=9");
    assert_eq!(range(vec![6], vec![6]), "6");
    assert_eq!(range(vec![6], vec![7]), "6 7");
    assert_eq!(range(vec![6], vec![8]), "6 8");
    assert_eq!(range(vec![6], vec![9]), "6..=9");
    assert_eq!(range(vec![7], vec![7]), "7");
    assert_eq!(range(vec![7], vec![8]), "");
    assert_eq!(range(vec![7], vec![9]), "7 9");
    assert_eq!(range(vec![8], vec![8]), "8");
    assert_eq!(range(vec![8], vec![9]), "8 9");
    assert_eq!(range(vec![9], vec![9]), "9");

    // Test descendants() and ancestors() against range().
    for bits in 0..(1 << 10) {
        let mut set = IdSet::empty();
        for i in (0..=9).rev() {
            if bits & (1 << i) != 0 {
                set.push_span(i.into());
            }
        }

        let all = dag.all().unwrap();
        assert_eq!(
            dag.range(set.clone(), all.clone()).unwrap().as_spans(),
            dag.descendants(set.clone()).unwrap().as_spans(),
        );

        assert_eq!(
            dag.range(all.clone(), set.clone()).unwrap().as_spans(),
            dag.ancestors(set.clone()).unwrap().as_spans(),
        );
    }
}

#[test]
fn test_render_segment_dag() {
    // For reference in below graphs.
    assert_eq!(
        build_segments(ASCII_DAG2, "W", 3).ascii[0],
        r#"
                      19/---------------13-14--\           19
                     / /                        \           \
               /----4-5-\    /-------11-12-------15-\     18-20--\
            0-1-2-3------6--7--8--9--10--------------16-17--------21-22
                                   \--13
Lv0: RH0-3[] 4-5[1] H6-10[3, 5] 11-12[7] 13-14[5, 9] 15-15[12, 14] H16-17[10, 15] R18-18[] 19-19[4] 20-20[18, 19] H21-22[17, 20]
Lv1: R0-10[] 11-15[7, 5, 9] 16-17[10, 15] R18-20[4]
Lv2: R0-17[]"#
    );

    let built = build_segments(ASCII_DAG2, "W", 3);
    let mut buf = Vec::new();
    buf.push(b'\n');
    render_segment_dag(&mut buf, &built.name_dag, 0, Group::MASTER).unwrap();

    assert_eq!(
        String::from_utf8(buf).unwrap(),
        r#"
o    V(21)-W(22)
├─╮
│ o    U(20)-U(20)
│ ├─╮
│ │ o  T(19)-T(19)
│ │ │
│ o │  S(18)-S(18)
│   │
o   │  Q(16)-R(17)
├─╮ │
│ o │    P(15)-P(15)
│ ├───╮
│ │ │ o  N(13)-O(14)
╭───┬─╯
│ o │  L(11)-M(12)
├─╯ │
o   │  G(6)-K(10)
├───╮
│   o  E(4)-F(5)
├───╯
o  A(0)-D(3)

"#
    );

    let mut buf = Vec::new();
    buf.push(b'\n');
    render_segment_dag(&mut buf, &built.name_dag, 1, Group::MASTER).unwrap();

    assert_eq!(
        String::from_utf8(buf).unwrap(),
        r#"
o  S(18)-U(20)
│
│ o  Q(16)-R(17)
╭─┤
│ o  L(11)-P(15)
╭─╯
o  A(0)-K(10)

"#
    );
}

#[test]
fn test_render_segment_dag_non_master() {
    let mut t = TestDag::new();

    // A: master; B, Z: non-master.
    t.drawdag("A--B--Z", &["A"]);

    let mut buf = Vec::new();
    buf.push(b'\n');
    render_segment_dag(&mut buf, &t.dag, 0, Group::NON_MASTER).unwrap();

    assert_eq!(
        String::from_utf8(buf).unwrap(),
        r#"
o  B(N0)-Z(N1)
│
~
"#
    );
}

#[cfg_attr(test, tokio::test)]
async fn test_subdag() {
    let t = TestDag::draw("A..E");
    let s = t.dag.subdag(set("B D E")).await.unwrap();
    #[cfg(feature = "render")]
    assert_eq!(
        render(&s),
        r#"
            E
            │
            D
            │
            B"#
    );

    // Test ordering: preserve the heads order (D before C).
    let t = TestDag::draw("A-X B-X X-C X-D");
    let s1 = t.dag.subdag(set("D C B A")).await.unwrap();
    let s2 = t.dag.subdag(set("A B C D")).await.unwrap();
    #[cfg(feature = "render")]
    assert_eq!(
        render(&s1),
        r#"
            D
            ├─╮
            │ │ C
            ╭─┬─╯
            │ A
            │
            B"#
    );
    #[cfg(feature = "render")]
    assert_eq!(render(&s1), render(&s2));
}

// Test utilities

fn expand(set: Set) -> String {
    let mut names = set
        .iter()
        .unwrap()
        .map(|n| String::from_utf8_lossy(n.unwrap().as_ref()).to_string())
        .collect::<Vec<String>>();
    names.sort();
    names.join(" ")
}

fn set(names: &str) -> Set {
    let names: Vec<Vertex> = names
        .split_whitespace()
        .map(|n| Vertex::copy_from(n.as_bytes()))
        .collect();
    Set::from_static_names(names)
}

impl IdMap {
    /// Replace names in an ASCII DAG using the ids assigned.
    fn replace(&self, text: &str) -> String {
        let mut result = text.to_string();
        for &group in Group::ALL.iter() {
            const MAX_ID_IN_ASCII_TEST: u64 = 30;
            for id in group.min_id().to(group.min_id() + MAX_ID_IN_ASCII_TEST) {
                if let Ok(Some(name)) = self.find_name_by_id(id) {
                    let name = String::from_utf8(name.to_vec()).unwrap();
                    let id_str = format!("{:01$}", id, name.len());
                    // Try to replace while maintaining width
                    if name.len() + 2 == id_str.len() {
                        result = result
                            .replace(&format!("{name}--"), &id_str)
                            .replace(&format!("{name}  "), &id_str);
                    } else if name.len() + 1 == id_str.len() {
                        result = result
                            .replace(&format!("{name}-"), &id_str)
                            .replace(&format!("{name} "), &id_str);
                    }
                    result = result.replace(&name.to_string(), &id_str);
                }
            }
        }
        result
    }
}

fn get_parents_func_from_ascii(text: &str) -> impl Fn(Vertex) -> Result<Vec<Vertex>> + use<> {
    let parents = ::drawdag::parse(text);
    move |name: Vertex| -> Result<Vec<Vertex>> {
        Ok(parents[&String::from_utf8(name.as_ref().to_vec()).unwrap()]
            .iter()
            .map(|p| Vertex::copy_from(p.as_bytes()))
            .collect())
    }
}

/// Result of `build_segments`.
pub(crate) struct BuildSegmentResult {
    pub(crate) ascii: Vec<String>,
    pub(crate) name_dag: Dag,
    pub(crate) dir: tempfile::TempDir,
}

/// Take an ASCII DAG, assign segments from given heads.
/// Return the ASCII DAG and the built Dag.
pub(crate) fn build_segments(text: &str, heads: &str, segment_size: usize) -> BuildSegmentResult {
    let mut dag = TestDag::new_with_segment_size(segment_size);

    let mut ascii = Vec::new();
    for head in heads.split(' ') {
        // Assign to non-master if the name starts with a lowercase character.
        let master = if head.chars().nth(0).unwrap().is_lowercase() {
            vec![]
        } else {
            vec![head]
        };
        dag.drawdag_with_limited_heads(text, &master[..], Some(&[head]));
        let annotated = dag.annotate_ascii(text);
        let segments = dag.render_segments();
        ascii.push(format!("{annotated}\n{segments}"));
    }

    BuildSegmentResult {
        ascii,
        name_dag: dag.dag,
        dir: dag.dir,
    }
}

fn from_ascii<D: DagAddHeads>(dag: D, text: &str) -> D {
    from_ascii_with_heads(dag, text, None)
}

fn from_ascii_with_heads<D: DagAddHeads>(mut dag: D, text: &str, heads: Option<&[&str]>) -> D {
    dag.import_ascii_with_heads(text, heads).unwrap();
    dag
}

/// Test a general DAG interface against a few test cases.
pub fn test_generic_dag<D: DagAddHeads + DagAlgorithm + IdConvert + Send + Sync + 'static>(
    new_dag: &impl Fn() -> D,
) {
    test_generic_dag1(new_dag()).unwrap();
    test_generic_dag2(new_dag()).unwrap();
    test_generic_dag_reachable_roots(new_dag()).unwrap();
    test_generic_dag_beautify(new_dag).unwrap();
}

#[cfg(feature = "render")]
fn render(dag: &(impl DagAlgorithm + ?Sized)) -> String {
    render_dag(dag, |_| None).unwrap()
}

#[cfg(test)]
pub(crate) fn nid(i: u64) -> Id {
    Group::NON_MASTER.min_id() + i
}

#[cfg(test)]
pub(crate) fn vid(i: u64) -> Id {
    Group::VIRTUAL.min_id() + i
}

#[cfg(test)]
pub(crate) fn dbg_iter<'a, T: std::fmt::Debug>(
    iter: Box<dyn Iterator<Item = Result<T>> + 'a>,
) -> String {
    let v = iter.map(|s| s.unwrap()).collect::<Vec<_>>();
    dbg(v)
}

#[cfg(test)]
pub(crate) fn dbg<T: std::fmt::Debug>(t: T) -> String {
    format!("{t:?}")
}
