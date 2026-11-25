/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::future::Future;

use tokio::time::Instant;

use super::*;
use crate::fetch_ahead::send_in_order;

fn window(fetches: usize) -> NonZeroUsize {
    NonZeroUsize::new(fetches).unwrap()
}

/// A fetch that takes `secs` and is then `output`.
async fn after<T>(secs: u64, output: T) -> T {
    tokio::time::sleep(Duration::from_secs(secs)).await;
    output
}

/// A stand-in for a p4 command: its output is read in three parts, a second apart, and it
/// fails if that isn't done a minute after it started, however little of the minute it was
/// polled for.
fn command(n: u64) -> impl Future<Output = Option<u64>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    async move {
        let read = async {
            for _part in 0..3 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            n
        };
        tokio::time::timeout_at(deadline, read).await.ok()
    }
}

#[mononoke::test]
async fn test_outputs_are_sent_in_the_order_of_the_fetches() {
    tokio::time::pause();
    let (sender, receiver) = mpsc::channel(0);
    // The later a fetch, the sooner it finishes.
    let fetches = (0..6).map(|n| after(6 - n, n));

    let (sent, ()) = future::join(
        receiver.collect::<Vec<_>>(),
        send_in_order(fetches, window(3), sender),
    )
    .await;

    assert_eq!(vec![0, 1, 2, 3, 4, 5], sent);
}

#[mononoke::test]
async fn test_a_window_of_one_fetches_one_at_a_time() {
    tokio::time::pause();
    let (sender, receiver) = mpsc::channel(0);
    let fetches = (0..4).map(|n| after(1, n));
    let started = Instant::now();

    let (sent, ()) = tokio::time::timeout(
        Duration::from_secs(60),
        future::join(
            receiver.collect::<Vec<_>>(),
            send_in_order(fetches, window(1), sender),
        ),
    )
    .await
    .expect("a fetch was never started");

    assert_eq!(vec![0, 1, 2, 3], sent);
    // Four fetches of a second each, one after another.
    assert_eq!(4, started.elapsed().as_secs());
}

/// What failed a batch in production: a build that took longer than a command's timeout,
/// with commands of later changelists in flight.
#[mononoke::test]
async fn test_fetches_are_polled_while_the_receiver_is_busy() {
    tokio::time::pause();
    let (sender, mut receiver) = mpsc::channel(0);
    let fetches = (0..6).map(command);
    // The build: each changelist takes it a hundred seconds.
    let build = async {
        let mut received = Vec::new();
        while let Some(output) = receiver.next().await {
            received.push(output);
            tokio::time::sleep(Duration::from_secs(100)).await;
        }
        received
    };

    let (received, ()) = future::join(build, send_in_order(fetches, window(4), sender)).await;

    assert_eq!(
        vec![Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)],
        received,
        "`None` is a command that timed out"
    );
}

#[mononoke::test]
async fn test_at_most_the_window_are_fetched_ahead_of_the_receiver() {
    tokio::time::pause();
    let (sender, mut receiver) = mpsc::channel(0);
    let started = AtomicUsize::new(0);
    let fetches = (0..20).map(|n| {
        started.fetch_add(1, Ordering::Relaxed);
        after(1, n)
    });
    // After each output, how many fetches have started that the receiver hasn't had.
    let receive = async {
        let mut ahead = Vec::new();
        while receiver.next().await.is_some() {
            // Long enough for every fetch that may start to start and finish.
            tokio::time::sleep(Duration::from_secs(10)).await;
            ahead.push(started.load(Ordering::Relaxed) - ahead.len() - 1);
        }
        ahead
    };

    let (ahead, ()) = future::join(receive, send_in_order(fetches, window(3), sender)).await;

    // Three in all, running, waiting or in the channel, until the fetches run out.
    let expected: Vec<usize> = (1..=20).map(|received| 3.min(20 - received)).collect();
    assert_eq!(expected, ahead);
}

/// Counts itself when dropped.
struct CountsDrop<'a>(&'a AtomicUsize);

impl Drop for CountsDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[mononoke::test]
async fn test_stops_and_drops_the_fetches_once_the_receiver_is_gone() {
    tokio::time::pause();
    let (sender, receiver) = mpsc::channel::<u32>(0);
    let (started, dropped) = (AtomicUsize::new(0), AtomicUsize::new(0));
    // Two that finish at once, so that one waits to be sent, and three that never finish.
    let fetches = (0..5).map(|n| {
        started.fetch_add(1, Ordering::Relaxed);
        let running = CountsDrop(&dropped);
        async move {
            if n >= 2 {
                future::pending::<()>().await;
            }
            drop(running);
            n
        }
    });
    let leave = async {
        for _ in 0..10 {
            yield_once().await;
        }
        drop(receiver);
    };

    tokio::time::timeout(
        Duration::from_secs(60),
        future::join(send_in_order(fetches, window(4), sender), leave),
    )
    .await
    .expect("still sending to a receiver that is gone");

    assert!(
        started.load(Ordering::Relaxed) >= 4,
        "the window was filled"
    );
    assert_eq!(
        started.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        "a fetch outlived the sending"
    );
}

fn during_build(changelist_lookahead: usize) -> ImportOptions {
    ImportOptions {
        fetch_during_build: true,
        changelist_lookahead,
        ..Default::default()
    }
}

fn ids(built: &[BuiltChangeset]) -> Vec<ChangesetId> {
    built
        .iter()
        .map(|built| built.changeset.get_changeset_id())
        .collect()
}

#[mononoke::fbinit_test]
async fn test_the_changesets_are_the_same_either_way(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let source = branch_after_two_changelists("//test1/a.txt");
    let changes = [7, 9, 11];

    for lookahead in [1, 2, 4] {
        let between_builds = ImportOptions {
            changelist_lookahead: lookahead,
            ..Default::default()
        };
        let before =
            build_changesets(&ctx, &repo, &source, DEPOT, None, &changes, &between_builds).await?;
        let options = during_build(lookahead);
        let now = build_changesets(&ctx, &repo, &source, DEPOT, None, &changes, &options).await?;

        assert_eq!(3, now.len(), "lookahead {lookahead}");
        assert_eq!(ids(&before), ids(&now), "lookahead {lookahead}");
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_failed_fetch_names_its_changelist(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    // No records for the branch: the fake fails its filelog.
    let source = four_changelists()
        .with_changelist(11, &[("//test1/b.txt", 1, Action::Branch, Some(b"x\n"))]);

    let error = build_changesets(
        &ctx,
        &repo,
        &source,
        DEPOT,
        None,
        &[3, 5, 7, 9, 11],
        &during_build(4),
    )
    .await
    .unwrap_err();

    assert!(
        format!("{error:#}")
            .contains("building changelist 11: unexpected filelog of //test1/b.txt#1"),
        "{error:?}"
    );
    Ok(())
}

/// As without it: the changelist whose fetch outlasts `max_build_time` is the batch's last,
/// and the batch doesn't wait for the fetch of the next one, already in flight.
#[mononoke::fbinit_test]
async fn test_a_batch_cut_by_max_build_time_drops_the_fetches_in_flight(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    tokio::time::pause();
    let day = Duration::from_secs(24 * 60 * 60);
    let source = FakeSource {
        slow_describes: HashMap::from([(5, Duration::from_secs(601)), (7, day)]),
        ..four_changelists()
    };
    let options = ImportOptions {
        max_build_time: Duration::from_secs(600),
        ..during_build(2)
    };

    let started = Instant::now();
    let built =
        build_changesets(&ctx, &repo, &source, DEPOT, None, &[3, 5, 7, 9], &options).await?;

    let changes: Vec<_> = built.iter().map(|built| built.change).collect();
    assert_eq!(vec![3, 5], changes);
    assert!(
        started.elapsed() < Duration::from_secs(700),
        "waited for 7's fetch: {:?}",
        started.elapsed()
    );
    Ok(())
}
