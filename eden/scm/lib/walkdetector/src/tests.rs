/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::atomic::Ordering;
use std::time::Duration;

use mock_instant::MockClock;
use types::RepoPath;
use types::RepoPathBuf;

use crate::Config;
use crate::Detector;
use crate::Walk;
use crate::WalkType;
use crate::walk_node::SeenNames;
use crate::walk_node::WalkNode;

fn p(p: impl AsRef<str>) -> RepoPathBuf {
    p.as_ref().to_string().try_into().unwrap()
}

const TEST_WALK_THRESHOLD: usize = 2;

#[test]
fn test_walk_big_dir() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    assert_eq!(detector.file_walks().len(), 0);

    detector.file_loaded(p("dir/a"), 0);
    detector.file_loaded(p("dir/a"), 0);

    assert_eq!(detector.file_walks().len(), 0);

    detector.file_loaded(p("dir/b"), 0);

    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    detector.file_loaded(p("dir/c"), 0);
    detector.file_loaded(p("dir/d"), 0);
    detector.file_loaded(p("dir/e"), 0);

    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);
}

#[test]
fn test_file_loaded_with_dir_size() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // The hint says "dir" holds a single file, so its first load is a walk.
    assert!(detector.file_loaded_with_dir_size(p("dir/a"), Some((1, 0)), 0));
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // Without a hint the usual threshold applies.
    assert!(!detector.file_loaded_with_dir_size(p("other/a"), None, 0));
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // A large hint raises the threshold above the base threshold.
    assert!(!detector.file_loaded_with_dir_size(p("huge/a"), Some((1000, 0)), 0));
    assert!(!detector.file_loaded_with_dir_size(p("huge/b"), Some((1000, 0)), 0));
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // A hint of zero files cannot describe the directory of a loaded file, so it is ignored.
    assert!(!detector.file_loaded_with_dir_size(p("stale/a"), Some((0, 1)), 0));
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // A size the detector already knows is not replaced by the hint.
    detector.dir_loaded(p("big"), 1000, 0, 0);
    assert!(!detector.file_loaded_with_dir_size(p("big/a"), Some((1, 0)), 0));
    assert!(!detector.file_loaded_with_dir_size(p("big/b"), Some((1, 0)), 0));
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);
}

#[test]
fn test_bfs_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    detector.file_loaded(p("root/dir1/a"), 0);
    detector.file_loaded(p("root/dir1/b"), 0);

    assert_eq!(detector.file_walks(), vec![(p("root/dir1"), 0)]);

    detector.file_loaded(p("root/dir2/a"), 0);
    detector.file_loaded(p("root/dir2/b"), 0);

    // Raised walk up to parent directory with depth=1.
    assert_eq!(detector.file_walks(), vec![(p("root"), 1)]);

    // Now walk proceeds to the next level.

    detector.file_loaded(p("root/dir1/dir1_1/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/b"), 0);

    // Nothing combined yet.
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 1), (p("root/dir1/dir1_1"), 0)]
    );

    detector.file_loaded(p("root/dir1/dir1_2/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_2/b"), 0);

    // Now we get a second walk for root/dir1
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 1), (p("root/dir1"), 1)]
    );

    // More reads in dir2 doesn't combine the walks
    detector.file_loaded(p("root/dir2/c"), 0);
    detector.file_loaded(p("root/dir2/d"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 1), (p("root/dir1"), 1)]
    );

    // Walking further in root/dir2 will combine up to root.
    detector.file_loaded(p("root/dir2/dir2_1/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/b"), 0);
    detector.file_loaded(p("root/dir2/dir2_2/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_2/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 2)]);

    // Walk boundary can advance after we see depth+1 access that bubbles up to 2
    // different children of root.
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/b"), 0);

    // So far only one advancement - doesn't expand root walk yet.
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 2), (p("root/dir2/dir2_1/dir2_1_1"), 0)]
    );

    // Doesn't bubble up since advancement is still only under a single child "dir2".
    detector.file_loaded(p("root/dir2/dir2_2/dir2_2_1/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_2/dir2_2_1/b"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 2), (p("root/dir2"), 2)]
    );

    // Now we also see a depth=3 access under "dir1" - expand depth.
    detector.file_loaded(p("root/dir1/dir1_1/dir1_1_1/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/dir1_1_1/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 3)]);
}

#[test]
fn test_advanced_remainder() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    detector.file_loaded(p("root/dir1/a"), 0);
    detector.file_loaded(p("root/dir1/b"), 0);
    detector.file_loaded(p("root/dir2/a"), 0);
    detector.file_loaded(p("root/dir2/b"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/b"), 0);

    detector.file_loaded(p("root/dir2/dir2_1/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 2)]);

    // This marks "root/dir1" as "advanced" child.
    detector.file_loaded(p("root/dir1/dir1_1/dir1_1_1/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/dir1_1_1/b"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 2), (p("root/dir1/dir1_1/dir1_1_1"), 0)]
    );

    // This marks "root/dir2" as "advanced" child, but the
    // root/dir2/dir2_1 walk extends deeper than the advanced walk -
    // don't remove it.
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/dir2_1_1_1/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/dir2_1_1_1/b"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/dir2_1_1_2/a"), 0);
    detector.file_loaded(p("root/dir2/dir2_1/dir2_1_1/dir2_1_1_2/b"), 0);

    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 3), (p("root/dir2/dir2_1/dir2_1_1"), 1)]
    );
}

#[test]
fn test_walk_node_insert() {
    let config = Config::default();
    let mut node = WalkNode::new(config.gc_timeout);

    node.insert_walk(&config, WalkType::File, &p("foo"), Walk::new(1));
    // Can re-insert.
    node.insert_walk(&config, WalkType::File, &p("foo"), Walk::new(1));
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![(p("foo"), 1, WalkType::File)]
    );

    // Don't insert since it is fully contained by "foo" walk.
    node.insert_walk(&config, WalkType::File, &p("foo/bar"), Walk::new(0));
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![(p("foo"), 1, WalkType::File)]
    );

    let baz_walk = Walk::new(2);
    node.insert_walk(&config, WalkType::File, &p("foo/bar/baz"), baz_walk);
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![
            (p("foo"), 1, WalkType::File),
            (p("foo/bar/baz"), 2, WalkType::File)
        ]
    );

    let root_walk = Walk::new(0);
    node.insert_walk(&config, WalkType::File, &p(""), root_walk);
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![
            (p(""), 0, WalkType::File),
            (p("foo"), 1, WalkType::File),
            (p("foo/bar/baz"), 2, WalkType::File)
        ]
    );

    // depth=1 doesn't contain any descendant walks - don't clear anything out.
    let root_walk = Walk::new(1);
    node.insert_walk(&config, WalkType::File, &p(""), root_walk);
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![
            (p(""), 1, WalkType::File),
            (p("foo"), 1, WalkType::File),
            (p("foo/bar/baz"), 2, WalkType::File)
        ]
    );

    // depth=2 contains the "foo" walk - clear "foo" out.
    let root_walk = Walk::new(2);
    node.insert_walk(&config, WalkType::File, &p(""), root_walk);
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![
            (p(""), 2, WalkType::File),
            (p("foo/bar/baz"), 2, WalkType::File)
        ]
    );

    // Contains the "foo/bar/baz" walk.
    let root_walk = Walk::new(5);
    node.insert_walk(&config, WalkType::File, &p(""), root_walk);
    assert_eq!(
        node.list_walks(Some(WalkType::File)),
        vec![(p(""), 5, WalkType::File)]
    );
}

#[test]
fn test_walk_node_get() {
    let config = Config::default();
    let mut node = WalkNode::new(config.gc_timeout);

    let get_depth = |node: &mut WalkNode, path: &RepoPath| -> Option<usize> {
        node.get_node(path)
            .and_then(|node| node.file_walk.as_ref().map(|walk| walk.depth))
    };

    assert!(get_depth(&mut node, &p("")).is_none());
    assert!(get_depth(&mut node, &p("foo")).is_none());
    assert!(get_depth(&mut node, &p("foo/bar")).is_none());

    let foo_walk = Walk::new(1);
    node.insert_walk(&config, WalkType::File, &p("foo"), foo_walk);

    assert!(get_depth(&mut node, &p("")).is_none());
    assert_eq!(get_depth(&mut node, &p("foo")), Some(1));
    assert!(get_depth(&mut node, &p("foo/bar")).is_none());

    let foo_bar_walk = Walk::new(2);
    node.insert_walk(&config, WalkType::File, &p("foo/bar"), foo_bar_walk);

    assert!(get_depth(&mut node, &p("")).is_none());
    assert_eq!(get_depth(&mut node, &p("foo")), Some(1));
    assert_eq!(get_depth(&mut node, &p("foo/bar")), Some(2));

    let root_walk = Walk::new(0);
    node.insert_walk(&config, WalkType::File, &p(""), root_walk);

    assert_eq!(get_depth(&mut node, &p("")), Some(0));
    assert_eq!(get_depth(&mut node, &p("foo")), Some(1));
    assert_eq!(get_depth(&mut node, &p("foo/bar")), Some(2));
}

#[test]
fn test_walk_get_containing_node() {
    let config = Config::default();
    let mut node = WalkNode::new(config.gc_timeout);

    let dir = p("foo/bar/baz");

    assert!(node.get_owning_node(WalkType::File, &dir).is_none());

    node.insert_walk(&config, WalkType::File, &p("foo/bar"), Walk::new(0));

    // Still not containing due to depth.
    assert!(node.get_owning_node(WalkType::File, &dir).is_none());

    node.insert_walk(&config, WalkType::File, &p("foo/bar"), Walk::new(1));

    let (containing_node, suffix) = node.get_owning_node(WalkType::File, &dir).unwrap();
    assert_eq!(
        containing_node
            .get_walk_for_type(WalkType::File)
            .unwrap()
            .depth,
        1
    );
    assert_eq!(suffix, p("baz"));
}

#[test]
fn test_dir_hints() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // Hint that "dir" has 0 files and 1 directory.
    detector.dir_loaded(p("dir"), 0, 1, 0);

    // Hint that "dir/subdir" has 1 file and 0 directories.
    detector.dir_loaded(p("dir/subdir"), 1, 0, 0);

    detector.file_loaded(p("dir/subdir/a"), 0);

    // The walk bubbled straight up to "dir".
    assert_eq!(detector.file_walks(), vec![(p("dir"), 1)]);
}

#[test]
fn test_advance_while_advancing() {
    // Test that we can "advance" the walk depth twice in a row when
    // the descendant walks have depths greater than zero.
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // Walk at root/, depth=1.
    insert_walk(&detector, p("root"), WalkType::File, 1);
    assert_eq!(detector.file_walks(), vec![(p("root"), 1)]);

    // Deeper walks that already exist.
    insert_walk(&detector, p("root/dir1/a/b"), WalkType::File, 0);
    insert_walk(&detector, p("root/dir2/a/b"), WalkType::File, 0);

    // Now insert two "advancing" walks.
    insert_walk(&detector, p("root/dir1/a"), WalkType::File, 0);
    insert_walk(&detector, p("root/dir2/a"), WalkType::File, 0);

    // root/ walk is advanced to depth 2, and during insertion we notice we can advance again based
    // on the deeper walks.
    assert_eq!(detector.file_walks(), vec![(p("root"), 3)]);
}

#[test]
fn test_retain_interesting_metadata() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // "interesting" metadata saying root/dir1 only has one directory
    detector.dir_loaded(p("root/dir1"), 2, 1, 0);

    // Walk at root/, depth=1.
    detector.file_loaded(p("root/dir1/a"), 0);
    detector.file_loaded(p("root/dir1/b"), 0);
    detector.file_loaded(p("root/dir2/a"), 0);
    detector.file_loaded(p("root/dir2/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 1)]);

    // Walk at root/dir1/dir1_1, depth=1.
    // Test that we "remembered" root/dir1 metadata, so doot/dir1/dir1_1 is instantly promoted into walk on root/dir1.
    detector.file_loaded(p("root/dir1/dir1_1/a"), 0);
    detector.file_loaded(p("root/dir1/dir1_1/b"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("root"), 1), (p("root/dir1"), 1)]
    );
}

#[test]
fn test_retain_interesting_metadata_when_covered_by_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);
    // The accesses below are in order; this test is about the plain ratio.
    detector.set_walk_ratio_sorted_factor(1.0);

    // Start off with a directory walk at root/ triggered by un-interesting directories.
    detector.dir_loaded(p("root/dir1"), 0, 0, 0);
    detector.dir_loaded(p("root/dir2"), 0, 0, 0);
    assert_eq!(detector.dir_walks(), vec![(p("root"), 0)]);

    // Now we see a massive root/dir3, but it is already covered by root/ walk.
    // Be sure to remember metadata in this case.
    detector.dir_loaded(p("root/dir3"), 100, 0, 0);

    // Don't create a file walk since we remembered that root/dir3 is very large.
    detector.file_loaded(p("root/dir3/a"), 0);
    detector.file_loaded(p("root/dir3/b"), 0);
    assert_eq!(detector.file_walks(), vec![]);
}

#[test]
fn test_dont_retain_empty_directory_metadata() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    detector.dir_loaded(p("root/dir1"), 0, 0, 0);

    // Check we didn't create a node for root/dir1. Empty is not interesting.
    assert!(
        detector
            .inner
            .read()
            .node
            .get_node(&p("root/dir1"))
            .is_none()
    );
}

fn insert_walk(d: &Detector, p: impl AsRef<RepoPath>, wt: WalkType, depth: usize) {
    d.inner
        .write()
        .insert_walk(&d.config, wt, Walk::new(depth), p.as_ref());
}

#[test]
fn test_merge_cousins() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    insert_walk(&detector, p("root"), WalkType::File, 2);

    // These cousins get merged into a new root/foo walk, but only with depth two.
    insert_walk(&detector, p("root/foo/dir1/dir1_1"), WalkType::File, 0);
    insert_walk(&detector, p("root/foo/dir2/dir2_1"), WalkType::File, 3);
    assert_eq!(
        detector.file_walks(),
        vec![
            (p("root"), 2),
            (p("root/foo"), 2),
            (p("root/foo/dir2/dir2_1"), 3)
        ]
    );
}

#[test]
fn test_merge_siblings_walked_over_time() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(3);
    detector.set_gc_timeout(Duration::from_secs(2));

    // Each sibling walk expires before the third arrives, so the three are never live together.
    insert_walk(&detector, p("root/a"), WalkType::File, 0);
    MockClock::advance(Duration::from_secs(1));
    insert_walk(&detector, p("root/b"), WalkType::File, 0);
    MockClock::advance(Duration::from_secs(1));
    assert_eq!(detector.file_walks(), vec![(p("root/b"), 0)]);

    insert_walk(&detector, p("root/c"), WalkType::File, 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 1)]);

    // A gap longer than the GC timeout forgets siblings whose walks have expired.
    insert_walk(&detector, p("other/x"), WalkType::File, 0);
    MockClock::advance(Duration::from_secs(3));
    insert_walk(&detector, p("other/y"), WalkType::File, 0);
    insert_walk(&detector, p("other/z"), WalkType::File, 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("other/y"), 0), (p("other/z"), 0)]
    );

    insert_walk(&detector, p("other/w"), WalkType::File, 0);
    assert_eq!(detector.file_walks(), vec![(p("other"), 1)]);
}

#[test]
fn test_walked_siblings_do_not_block_advancing_parent() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    insert_walk(&detector, p("root"), WalkType::File, 1);

    // Children walking deeper than the parent's walk advance it rather than re-merging into it.
    insert_walk(&detector, p("root/a"), WalkType::File, 1);
    insert_walk(&detector, p("root/b"), WalkType::File, 1);
    assert_eq!(detector.file_walks(), vec![(p("root"), 2)]);

    insert_walk(&detector, p("root/c"), WalkType::File, 2);
    insert_walk(&detector, p("root/d"), WalkType::File, 2);
    assert_eq!(detector.file_walks(), vec![(p("root"), 3)]);
}

#[test]
fn test_merge_siblings_keeps_live_walks_after_gap() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(3);
    detector.set_gc_timeout(Duration::from_secs(2));

    // "root/a" stays busy while "root" itself sees no activity, so it still counts once "root/b"
    // and "root/c" arrive.
    insert_walk(&detector, p("root/a"), WalkType::File, 0);
    MockClock::advance(Duration::from_secs(1));
    detector.file_loaded(p("root/a/file1"), 0);
    MockClock::advance(Duration::from_secs(1));
    detector.file_loaded(p("root/a/file2"), 0);
    insert_walk(&detector, p("root/b"), WalkType::File, 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("root/a"), 0), (p("root/b"), 0)]
    );

    insert_walk(&detector, p("root/c"), WalkType::File, 0);
    assert_eq!(detector.file_walks(), vec![(p("root"), 1)]);
}

#[test]
fn test_gc() {
    // Test with a low interval so full GC is invoked often, and test with a large interval so full
    // GC is not invoked.
    for interval in [1, 100] {
        let mut detector = Detector::new();
        detector.set_walk_threshold(TEST_WALK_THRESHOLD);
        detector.set_gc_interval(Duration::from_secs(interval));
        detector.set_gc_timeout(Duration::from_secs(2));

        detector.file_loaded(p("dir1/a"), 0);
        assert_eq!(detector.file_walks(), vec![]);

        MockClock::advance(Duration::from_secs(1));

        // GC should run but not remove anything.
        detector.file_loaded(p("dir1/b"), 0);
        assert_eq!(detector.file_walks(), vec![(p("dir1"), 0)]);

        MockClock::advance(Duration::from_secs(1));

        // This should keep dir1 walk alive.
        detector.file_loaded(p("dir1/c"), 0);
        detector.file_loaded(p("dir2/a"), 0);
        detector.file_loaded(p("some/deep/dir/a"), 0);
        assert_eq!(detector.file_walks(), vec![(p("dir1"), 0)]);

        MockClock::advance(Duration::from_secs(1));

        detector.file_loaded(p("dir2/b"), 0);
        assert_eq!(detector.file_walks(), vec![(p(""), 1)]);

        MockClock::advance(Duration::from_secs(1));

        // GC should clear out some/deep/dir, so this should not result in walk.
        detector.file_loaded(p("some/deep/dir/b"), 0);
        // This should update access time for root walk.
        detector.file_loaded(p("dir3/a"), 0);
        assert_eq!(detector.file_walks(), vec![(p(""), 1)]);

        // Root walk still here since dir3/a refreshed access time.
        MockClock::advance(Duration::from_secs(1));
        assert_eq!(detector.file_walks(), vec![(p(""), 1)]);

        let logged_end = detector
            .inner
            .read()
            .node
            .get_node(&p(""))
            .and_then(|n| n.get_walk_for_type(WalkType::File))
            .unwrap()
            .logged_end
            .clone();

        assert!(!logged_end.load(Ordering::Relaxed));

        // Everything is GC'd.
        MockClock::advance(Duration::from_secs(1));
        assert_eq!(detector.file_walks(), vec![]);

        // This will trigger a "JIT" clean up of the root walk in the case we haven't run a full GC.
        detector.file_loaded(p("a"), 0);

        // Make sure the end of the walk was logged.
        assert!(logged_end.load(Ordering::Relaxed));
    }
}

#[test]
fn test_gc_stats() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // Don't run GC automatically.
    detector.set_gc_interval(Duration::from_secs(10));

    detector.set_gc_timeout(Duration::from_secs(1));

    detector.file_loaded(p("dir1/a"), 0);
    detector.file_loaded(p("dir1/b"), 0);
    let logged_end = detector
        .inner
        .read()
        .node
        .get_node(&p("dir1"))
        .and_then(|n| n.get_walk_for_type(WalkType::File))
        .expect("dir1 file walk should exist")
        .logged_end
        .clone();

    detector.file_loaded(p("dir2/a"), 0);

    detector.file_loaded(p("dir3/dir4/a"), 0);

    MockClock::advance(Duration::from_secs(2));

    // Refresh access time on dir3.
    detector.file_loaded(p("dir3/dir4/b"), 0);

    // Manually run GC to check stats.
    let gc_result = detector.inner.write().node.gc(&Default::default());
    let nodes_removed = gc_result.deleted_nodes;
    let nodes_remaining = gc_result.remaining_nodes;
    let walks_removed = gc_result.deleted_walks;

    // "dir1" and "dir2"
    assert_eq!(nodes_removed, 2);

    // root node and "dir3" and "dir4"
    assert_eq!(nodes_remaining, 3);

    // "dir1"
    assert_eq!(walks_removed, 1);
    assert!(
        logged_end.load(Ordering::Relaxed),
        "GC should finalize the removed dir1 walk"
    );
}

#[test]
fn test_dir_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    detector.dir_loaded(p(""), 2, 2, 0);
    assert_eq!(detector.dir_walks(), vec![]);

    detector.dir_loaded(p("dir1"), 2, 2, 0);
    detector.dir_loaded(p("dir2"), 2, 2, 0);
    assert_eq!(detector.dir_walks(), vec![(p(""), 0)]);

    detector.dir_loaded(p("dir1/dir1_1"), 2, 2, 0);
    detector.dir_loaded(p("dir1/dir1_2"), 2, 2, 0);
    detector.dir_loaded(p("dir2/dir2_1"), 2, 2, 0);
    detector.dir_loaded(p("dir2/dir2_2"), 2, 2, 0);
    assert_eq!(detector.dir_walks(), vec![(p(""), 1)]);

    // Now we start seeing files walked.
    detector.file_loaded(p("a"), 0);
    detector.file_loaded(p("b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 0)]);
    // Directory walk still around since it is deeper than file walk.
    assert_eq!(detector.dir_walks(), vec![(p(""), 1)]);

    detector.file_loaded(p("dir1/a"), 0);
    detector.file_loaded(p("dir1/b"), 0);
    detector.file_loaded(p("dir2/a"), 0);
    detector.file_loaded(p("dir2/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 1)]);
    // Directory walk is redundant - remove it.
    assert_eq!(detector.dir_walks(), vec![]);

    detector.file_loaded(p("dir1/dir1_1/a"), 0);
    detector.file_loaded(p("dir1/dir1_1/b"), 0);
    detector.file_loaded(p("dir2/dir2_1/a"), 0);
    detector.file_loaded(p("dir2/dir2_1/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 2)]);
    assert_eq!(detector.dir_walks(), vec![]);

    detector.dir_loaded(p("dir1/dir1_1/dir1_1_1"), 2, 2, 0);
    detector.dir_loaded(p("dir1/dir1_1/dir1_1_2"), 2, 2, 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 2)]);
    // No dir walk - depth=2 is already covered by file walk.
    assert_eq!(detector.dir_walks(), vec![]);
}

#[test]
fn test_walk_changed() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_gc_interval(Duration::from_secs(1));
    detector.set_gc_timeout(Duration::from_secs(2));

    // No walk.
    assert!(!detector.file_loaded(p("dir1/a"), 0));

    // Yes walk.
    assert!(detector.file_loaded(p("dir1/b"), 0));

    // No walk changes.
    assert!(!detector.file_loaded(p("dir2/a"), 0));

    MockClock::advance(Duration::from_secs(5));

    // GC removes walk
    assert!(detector.file_loaded(p("dir2/a"), 0));
}

#[test]
fn test_touched() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_gc_interval(Duration::from_secs(1));
    detector.set_gc_timeout(Duration::from_secs(2));

    detector.file_loaded(p("dir1/a"), 0);
    detector.file_loaded(p("dir1/b"), 0);
    assert_eq!(detector.file_walks().len(), 1);

    detector.dir_loaded(p("dir2/a"), 0, 0, 0);
    detector.dir_loaded(p("dir2/b"), 0, 0, 0);
    assert_eq!(detector.dir_walks().len(), 1);

    for _ in 0..10 {
        MockClock::advance(Duration::from_secs(1));

        assert!(detector.file_read(p("dir1/c"), 0));
        assert!(detector.dir_read(p("dir2/c"), 0));
    }

    detector.file_loaded(p("something/else"), 0);
    detector.dir_loaded(p("something/else"), 0, 0, 0);

    // Walks still around - not GC'd.
    assert_eq!(detector.file_walks().len(), 1);
    assert_eq!(detector.dir_walks().len(), 1);

    // Test that the touched methods don't resurrect expired (but not collected) nodes.
    MockClock::advance(Duration::from_secs(5));
    assert!(!detector.file_read(p("dir1/c"), 0));
    assert!(detector.file_walks().is_empty());
    assert!(detector.dir_walks().is_empty());
}

#[test]
fn test_counters() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    let get_counters = |p: RepoPathBuf, wt: WalkType| {
        detector
            .inner
            .write()
            .node
            .get_node(&p)
            .and_then(|n| n.get_walk_for_type(wt))
            .map(|w| w.counters())
            .unwrap()
    };

    // Include initial counts when we first create the walk.
    detector.dir_loaded(p("dir1/dir1_1"), 0, 0, 0);
    detector.dir_loaded(p("dir1/dir1_2"), 0, 0, 0);
    detector.dir_loaded(p("dir1/dir1_2"), 0, 0, 0);
    detector.dir_read(p("dir1/dir1_2"), 0);
    assert_eq!(
        get_counters(p("dir1"), WalkType::Directory),
        (0, 0, 0, 3, 1)
    );

    // Propagate counts when we convert to file walk.
    detector.file_loaded(p("dir1/a"), 0);
    detector.file_loaded(p("dir1/b"), 0);
    detector.file_read(p("dir1/c"), 0);

    detector.files_preloaded(p("dir1"), 1);
    let (preloaded, read) = detector.files_preloaded(p("dir1"), 1);
    // Two remote loads plus one cached read.
    assert_eq!(read, 3);
    assert_eq!(preloaded, 2);

    assert_eq!(get_counters(p("dir1"), WalkType::File), (2, 1, 2, 3, 1));

    // Propagate when combining walks.
    detector.file_loaded(p("dir2/a"), 0);
    detector.file_loaded(p("dir2/b"), 0);
    assert_eq!(get_counters(p(""), WalkType::File), (4, 1, 2, 3, 1));
}

#[test]
fn test_huge_directory() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);
    // The accesses below are in order; this test is about the plain ratio.
    detector.set_walk_ratio_sorted_factor(1.0);

    // Huge directory.
    detector.dir_loaded(p("dir1"), 100, 100, 0);

    detector.file_loaded(p("dir1/a"), 0);
    detector.file_loaded(p("dir1/b"), 0);

    // No walks yet - 2/100 has not reached out threshold of 10%.
    assert!(detector.file_walks().is_empty());

    for i in 0..5 {
        detector.file_loaded(p(format!("dir1/{i}")), 0);
    }

    // Still not enough.
    assert!(detector.file_walks().is_empty());

    for i in 0..10 {
        detector.file_loaded(p(format!("dir1/{i}")), 0);
    }

    // Now we hit the 10% threshold.
    assert_eq!(detector.file_walks(), vec![(p("dir1"), 0)]);

    // Root directory needs 30*0.1=3 walked children to become walked.
    detector.dir_loaded(p(""), 0, 30, 0);

    detector.file_loaded(p("dir2/a"), 0);
    detector.file_loaded(p("dir2/b"), 0);

    // Didn't bubble up yet even though we met the base walk threshold of 2.
    assert_eq!(detector.file_walks(), vec![(p("dir1"), 0), (p("dir2"), 0)]);

    // Now we meet the 10% limit.
    detector.file_loaded(p("dir3/a"), 0);
    detector.file_loaded(p("dir3/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 1)]);
}

#[test]
fn test_slow_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(3);
    detector.set_gc_interval(Duration::from_secs(1));
    detector.set_gc_timeout(Duration::from_secs(3));

    detector.file_loaded(p("dir/a"), 0);
    detector.file_loaded(p("dir/b"), 0);
    detector.file_loaded(p("dir/c"), 0);
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // Slowly (relative to GC timeout) advance the walk. Make sure the "dir" walk doesn't time out
    // while we are making progress.
    MockClock::advance(Duration::from_secs(2));

    detector.file_loaded(p("dir/dir_1/a"), 0);
    detector.file_loaded(p("dir/dir_1/b"), 0);
    detector.file_loaded(p("dir/dir_1/c"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("dir"), 0), (p("dir/dir_1"), 0)]
    );

    MockClock::advance(Duration::from_secs(2));

    detector.file_loaded(p("dir/dir_2/a"), 0);
    detector.file_loaded(p("dir/dir_2/b"), 0);
    detector.file_loaded(p("dir/dir_2/c"), 0);
    assert_eq!(
        detector.file_walks(),
        vec![(p("dir"), 0), (p("dir/dir_1"), 0), (p("dir/dir_2"), 0)]
    );

    MockClock::advance(Duration::from_secs(2));

    detector.file_loaded(p("dir/dir_3/a"), 0);
    detector.file_loaded(p("dir/dir_3/b"), 0);
    detector.file_loaded(p("dir/dir_3/c"), 0);
    assert_eq!(detector.file_walks(), vec![(p("dir"), 1)]);
}

#[test]
fn test_split_off_child_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);

    detector.dir_loaded(p("big"), 5, 30, 0);

    detector.file_loaded(p("small/a"), 0);
    detector.file_loaded(p("small/b"), 0);
    detector.file_loaded(p("big/a"), 0);
    detector.file_loaded(p("big/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 1)]);

    detector.file_loaded(p("small/dir1/a"), 0);
    detector.file_loaded(p("small/dir1/b"), 0);
    detector.file_loaded(p("big/dir1/a"), 0);
    detector.file_loaded(p("big/dir1/b"), 0);
    detector.file_loaded(p("big/dir2/a"), 0);
    detector.file_loaded(p("big/dir2/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 2)]);

    detector.file_loaded(p("small/dir1/dir1_1/a"), 0);
    detector.file_loaded(p("small/dir1/dir1_1/b"), 0);
    detector.file_loaded(p("big/dir1/dir1_1/a"), 0);
    detector.file_loaded(p("big/dir1/dir1_1/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p(""), 3)]);

    // Now "small" has run out of depth, but "big" still has a lot left.

    detector.file_loaded(p("big/dir2/dir2_1/dir2_1_1/a"), 0);
    detector.file_loaded(p("big/dir2/dir2_1/dir2_1_1/b"), 0);

    detector.file_loaded(p("big/dir3/dir3_1/dir3_1_1/a"), 0);
    detector.file_loaded(p("big/dir3/dir3_1/dir3_1_1/b"), 0);

    // Haven't hit the threshold of 3 yet (0.1*30=3)
    assert_eq!(
        detector.file_walks(),
        vec![
            (p(""), 3),
            (p("big/dir2/dir2_1/dir2_1_1"), 0),
            (p("big/dir3/dir3_1/dir3_1_1"), 0)
        ]
    );

    detector.file_loaded(p("big/dir4/dir4_1/dir4_1_1/a"), 0);
    detector.file_loaded(p("big/dir4/dir4_1/dir4_1_1/b"), 0);

    // No matter how much we see at depth=3, we won't advance the root walk since all advancements are under a single child "big".
    // But if we break off a separate walk for "big", then it can deepen on its own.
    assert_eq!(detector.file_walks(), vec![(p(""), 3), (p("big"), 3)]);
}

#[test]
fn test_important_metadata() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);

    detector.dir_loaded(p("big_remote"), 1000, 1000, 0);
    detector.dir_metadata(p("big_cached"), 1000, 1000);

    // Trigger a GC.
    MockClock::advance(Duration::from_mins(1));
    assert!(detector.file_walks().is_empty());

    // Make sure we remembered the important metadata.
    for dir in ["big_remote", "big_cached"] {
        assert_eq!(
            detector
                .inner
                .read()
                .node
                .get_node(&p(dir))
                .unwrap()
                .total_dirs()
                .unwrap(),
            1000
        );
    }
}

#[test]
fn test_pid_propagation() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    let get_pid = |path: &str| -> u32 {
        detector
            .inner
            .read()
            .node
            .get_node(&p(path))
            .unwrap()
            .get_walk_for_type(WalkType::File)
            .unwrap()
            .pid()
    };

    detector.file_loaded(p("a"), 1);
    detector.file_loaded(p("b"), 1);
    assert_eq!(detector.file_walks(), vec![(p(""), 0)]);
    assert_eq!(get_pid(""), 1);

    // pid will eventually change if we see activity from another pid
    loop {
        detector.file_read(p("c"), 2);
        if get_pid("") == 2 {
            break;
        }
    }
}

#[test]
fn test_dont_gc_ancestor_walk() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_gc_timeout(Duration::from_secs(2));

    insert_walk(&detector, p("root"), WalkType::File, 1);

    MockClock::advance(Duration::from_secs(1));

    // Now we see deeper activity "behind" an intermediate walk.

    insert_walk(&detector, p("root/a/b/bigdir"), WalkType::File, 0);

    MockClock::advance(Duration::from_secs(1));

    insert_walk(&detector, p("root/a/b/bigdir/dir1"), WalkType::File, 0);

    MockClock::advance(Duration::from_secs(1));

    insert_walk(&detector, p("root/a/b/bigdir/dir2"), WalkType::File, 0);

    MockClock::advance(Duration::from_secs(1));

    insert_walk(&detector, p("root/a/b/bigdir/dir3"), WalkType::File, 0);

    MockClock::advance(Duration::from_secs(1));

    // Now things have bubbled up closer to root/.

    insert_walk(&detector, p("root/a/foo"), WalkType::File, 0);

    MockClock::advance(Duration::from_secs(1));

    insert_walk(&detector, p("root/b/foo"), WalkType::File, 0);

    // We kept around the root/ walk (and advanced it to depth 2).
    assert_eq!(detector.file_walks(), vec![(p("root"), 2)]);
}

#[test]
fn test_recursive_metadata() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);

    // Small directory metadata should not get included in rollups to ancestors.
    detector.dir_loaded(p("foo/small"), 1, 1, 0);

    detector.dir_loaded(p("foo/bar"), 10, 30, 0);
    detector.dir_loaded(p("foo/bar/baz"), 100, 200, 0);
    detector.dir_loaded(p("foo/bar/baz"), 101, 201, 0);

    let inner = detector.inner.read();
    let node = &inner.node;
    assert_eq!(node.total_dirs_at_depth(0), None);
    assert_eq!(node.total_files_at_depth(0), None);

    // depth=2 (foo/bar/*)
    assert_eq!(node.total_dirs_at_depth(2), Some(30));
    assert_eq!(node.total_files_at_depth(2), Some(10));

    // depth=3 (foo/bar/baz/*) - make sure we didn't overcount foo/bar/baz metadata
    assert_eq!(node.total_dirs_at_depth(3), Some(201));
    assert_eq!(node.total_files_at_depth(3), Some(101));

    // Check counts starting from foo/bar
    let node = node.get_node(&p("foo/bar")).unwrap();
    // depth=0 (foo/bar/*)
    assert_eq!(node.total_dirs_at_depth(0), Some(30));
    assert_eq!(node.total_files_at_depth(0), Some(10));
    // depth=1 (foo/bar/baz/*)
    assert_eq!(node.total_dirs_at_depth(1), Some(201));
    assert_eq!(node.total_files_at_depth(1), Some(101));
}

#[test]
fn test_advance_depth_across_wide_dir() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);
    // The accesses below are in order; this test is about the plain ratio.
    detector.set_walk_ratio_sorted_factor(1.0);

    // Big dir with 100 sub-directories.
    detector.dir_loaded(p("really/big/dir"), 0, 100, 0);

    // Existing walk covers up to really/big/dir/* (but not into the 100 sub-directories).
    insert_walk(&detector, p(""), WalkType::File, 3);

    // Two children of root ("some" and "other") have a depth advancing walk.
    insert_walk(&detector, p("some/small/dir/child"), WalkType::File, 0);
    insert_walk(&detector, p("other/small/dir/child"), WalkType::File, 0);
    // Root walk wasn't advanced yet - need to surpass 10% threshold for really/big/dir.
    assert_eq!(
        detector.file_walks(),
        vec![
            (p(""), 3),
            (p("other/small/dir/child"), 0),
            (p("some/small/dir/child"), 0)
        ]
    );

    // Trigger walks of 10 of the dirs under really/big/dir.
    for i in 0..10 {
        detector.file_loaded(p(format!("really/big/dir/child_{i}/a")), 0);
        detector.file_loaded(p(format!("really/big/dir/child_{i}/b")), 0);
    }
    // Now we surpassed the 10% (of 100) threshold for depth 4.
    assert_eq!(detector.file_walks(), vec![(p(""), 4)]);
}

#[test]
fn test_breadth_credit() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);

    // "big" has 100 directories, so detecting its walk takes 10 of them.
    detector.dir_loaded(p("big"), 0, 100, 0);
    for i in 0..10 {
        detector.dir_loaded(p(format!("big/d{i}")), 0, 0, 0);
    }
    assert_eq!(detector.dir_walks(), vec![(p("big"), 0)]);

    // The walk already crossed 100 children, so advancing needs only the base threshold.
    insert_walk(&detector, p("big/d0"), WalkType::Directory, 0);
    insert_walk(&detector, p("big/d1"), WalkType::Directory, 0);
    assert_eq!(detector.dir_walks(), vec![(p("big"), 1)]);

    // A wider level than any crossed so far still needs the ratio: big/d0 has 1000 directories.
    detector.dir_loaded(p("big/d0"), 0, 1000, 0);
    insert_walk(&detector, p("big/d0/x"), WalkType::Directory, 0);
    insert_walk(&detector, p("big/d1/x"), WalkType::Directory, 0);
    assert_eq!(
        detector.dir_walks(),
        vec![(p("big"), 1), (p("big/d0/x"), 0), (p("big/d1/x"), 0)]
    );

    // A walk rooted inside big's coverage inherits big's credit: big/d2 and big/d2/x each have 100
    // directories, no wider than the 100 already crossed, so two kin walks suffice to deepen it.
    detector.dir_loaded(p("big/d2"), 0, 100, 0);
    detector.dir_loaded(p("big/d2/x"), 0, 100, 0);
    insert_walk(&detector, p("big/d2"), WalkType::Directory, 1);
    insert_walk(&detector, p("big/d2/x/q"), WalkType::Directory, 0);
    insert_walk(&detector, p("big/d2/y/q"), WalkType::Directory, 0);
    assert!(detector.dir_walks().contains(&(p("big/d2"), 2)));
}

#[test]
fn test_fast_paths() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);

    // Nothing covers these yet, so the fast paths decline and the slow paths detect the walk.
    assert!(!detector.file_loaded_fast(p("dir/a"), 0));
    assert!(!detector.dir_loaded_fast(p("dir/sub1"), 5, 5, 0));
    detector.file_loaded(p("dir/a"), 0);
    detector.file_loaded(p("dir/b"), 0);
    assert_eq!(detector.file_walks(), vec![(p("dir"), 0)]);

    // Inside the walk the fast paths account the access and the walk's counters advance.
    assert!(detector.file_loaded_fast(p("dir/c"), 0));
    assert!(detector.dir_loaded_fast(p("dir/sub2"), 5, 5, 0));
    // A directory size worth remembering still needs the slow path.
    assert!(!detector.dir_loaded_fast(p("dir/sub3"), 1, 0, 0));
    let (_, reads) = detector.files_preloaded(p("dir"), 0);
    assert_eq!(reads, 3);
}

#[test]
fn test_might_have_walk() {
    let mut detector = Detector::new();
    detector.set_gc_timeout(Duration::from_secs(1));
    detector.set_gc_interval(Duration::from_secs(100));

    let might_have_walk = |dir: &str| -> bool {
        detector
            .inner
            .read()
            .node
            .get_node(&p(dir))
            .is_some_and(|n| n.descendant_might_have_walk)
    };

    assert!(!might_have_walk(""));

    insert_walk(&detector, p("foo/bar"), WalkType::File, 1);
    assert!(might_have_walk(""));
    assert!(might_have_walk("foo"));
    assert!(!might_have_walk("foo/bar"));

    insert_walk(&detector, p("foo/baz/qux/dir"), WalkType::Directory, 1);
    assert!(might_have_walk("foo/baz"));
    assert!(might_have_walk("foo/baz/qux"));

    // foo/baz/qux walk does not conmpletely contain foo/baz/qux/dir
    insert_walk(&detector, p("foo/baz/qux"), WalkType::Directory, 1);
    assert!(might_have_walk("foo/baz"));
    assert!(might_have_walk("foo/baz/qux"));

    // now we completely contain - foo/baz/qux no longer has descendant walk
    insert_walk(&detector, p("foo/baz/qux"), WalkType::Directory, 2);
    assert!(might_have_walk("foo/baz"));
    assert!(!might_have_walk("foo/baz/qux"));

    MockClock::advance(Duration::from_secs(2));

    // Refresh last_accessed
    insert_walk(&detector, p("foo/bar"), WalkType::Directory, 2);

    // GC foo/baz/qux walk - foo/baz should no longer have walk
    detector.inner.write().node.gc(&detector.config);
    assert!(might_have_walk(""));
    assert!(might_have_walk("foo"));
    assert!(!might_have_walk("foo/bar"));
    assert!(!might_have_walk("foo/baz"));
}

#[test]
fn test_seen_names_order() {
    let name = |s: &str| p(s).last_component().unwrap().to_owned();
    let mut seen = SeenNames::default();

    seen.insert(name("a"));
    seen.insert(name("c"));
    // A repeat carries no order information.
    seen.insert(name("a"));
    assert_eq!((seen.len(), seen.is_sorted()), (2, true));

    seen.insert(name("b"));
    assert_eq!((seen.len(), seen.is_sorted()), (3, false));
    // Ascending again does not undo it.
    seen.insert(name("d"));
    assert!(!seen.is_sorted());

    // After a reset the arrival order of the seeds is unknown, so a later name is in order only
    // if it sorts after all of them.
    seen.reset_to([name("c"), name("a")]);
    seen.insert(name("b"));
    assert_eq!((seen.len(), seen.is_sorted()), (3, false));
    seen.reset_to([name("c"), name("a")]);
    seen.insert(name("d"));
    assert_eq!((seen.len(), seen.is_sorted()), (3, true));
}

#[test]
fn test_sorted_walk_ratio() {
    let mut detector = Detector::new();
    detector.set_walk_threshold(TEST_WALK_THRESHOLD);
    detector.set_walk_ratio(0.1);
    detector.set_walk_ratio_sorted_factor(0.4);

    // Keep the root from merging the two directory walks below.
    detector.dir_metadata(p(""), 0, 1000);

    // "big" has 100 subdirectories: a walk takes 10 of them, or 4 loaded in sorted order.
    detector.dir_metadata(p("big"), 0, 100);
    for name in ["a", "b", "c"] {
        detector.dir_loaded(p(format!("big/{name}")), 0, 0, 0);
    }
    assert!(detector.dir_walks().is_empty());
    detector.dir_loaded(p("big/d"), 0, 0, 0);
    assert_eq!(detector.dir_walks(), vec![(p("big"), 0)]);

    // One load out of order and the full ratio applies, even though later loads ascend.
    detector.dir_metadata(p("other"), 0, 100);
    for name in ["b", "a", "c", "d", "e", "f", "g", "h", "i"] {
        detector.dir_loaded(p(format!("other/{name}")), 0, 0, 0);
    }
    assert_eq!(detector.dir_walks(), vec![(p("big"), 0)]);
    detector.dir_loaded(p("other/j"), 0, 0, 0);
    assert_eq!(detector.dir_walks(), vec![(p("big"), 0), (p("other"), 0)]);
}

#[test]
fn test_sorted_sibling_walks_merge_sooner() {
    let walk_siblings = |order: [&str; 3]| {
        let mut detector = Detector::new();
        detector.set_walk_threshold(TEST_WALK_THRESHOLD);
        detector.set_walk_ratio(0.1);
        detector.set_walk_ratio_sorted_factor(0.3);

        // The root has 100 subdirectories: merging child walks into it takes 10 of them, or 3
        // walked in sorted order.
        detector.dir_metadata(p(""), 0, 100);
        for dir in order {
            detector.file_loaded(p(format!("{dir}/x")), 0);
            detector.file_loaded(p(format!("{dir}/y")), 0);
        }
        detector.file_walks()
    };

    assert_eq!(walk_siblings(["a", "b", "c"]), vec![(p(""), 1)]);
    assert_eq!(
        walk_siblings(["b", "a", "c"]),
        vec![(p("a"), 0), (p("b"), 0), (p("c"), 0)]
    );
}
