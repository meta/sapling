/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

fn window(numbers: u64) -> NonZeroU64 {
    NonZeroU64::new(numbers).unwrap()
}

/// Each listing's `(after, upto, limit)`, in order.
fn listings(source: &FakeSource) -> Vec<(Option<u64>, u64, usize)> {
    source.listed.lock().unwrap().clone()
}

#[mononoke::fbinit_test]
async fn test_windows_list_what_one_listing_does(_fb: FacebookInit) -> Result<()> {
    let numbers = [3, 4, 5, 12, 25, 26, 61, 400, 401, 9_000];
    for (numbers, latest) in [(&numbers[..], 9_000), (&numbers[..], 5_000_000), (&[], 0)] {
        let source = || FakeSource {
            latest: Some(latest),
            ..with_changelists(numbers)
        };
        for after in [None, Some(3), Some(5), Some(60), Some(401), Some(9_000)] {
            for limit in [0, 1, 2, 3, 100] {
                for first in [1, 7, 50, 10_000] {
                    let one_listing = next_changes(&source(), DEPOT, after, limit).await?;
                    let windows =
                        next_changes_in_windows(&source(), DEPOT, after, limit, window(first))
                            .await?;
                    assert_eq!(
                        one_listing, windows,
                        "latest {latest}, after {after:?}, limit {limit}, first window {first}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_full_first_window_is_the_only_listing(_fb: FacebookInit) -> Result<()> {
    let source = FakeSource {
        latest: Some(5_000_000),
        ..with_changelists(&[3, 4, 5, 12, 25])
    };

    let changes = next_changes_in_windows(&source, DEPOT, Some(3), 2, window(10)).await?;

    assert_eq!(vec![4, 5], changes);
    assert_eq!(
        vec![(Some(3), 13, 2)],
        listings(&source),
        "one listing of 10 numbers, not of everything up to the latest"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_short_window_is_followed_by_one_twice_as_wide(_fb: FacebookInit) -> Result<()> {
    let source = FakeSource {
        latest: Some(1_000),
        ..with_changelists(&[4, 20, 21, 30])
    };

    let changes = next_changes_in_windows(&source, DEPOT, Some(3), 3, window(10)).await?;

    assert_eq!(vec![4, 20, 21], changes);
    assert_eq!(
        vec![(Some(3), 13, 3), (Some(13), 33, 2)],
        listings(&source),
        "the second listing starts where the first ended, covers 20 numbers and asks only \
         for the two changelists still missing"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_a_quiet_depot_on_a_busy_server_costs_a_few_listings(_fb: FacebookInit) -> Result<()> {
    let source = FakeSource {
        latest: Some(5_000_000),
        ..with_changelists(&[3])
    };

    let changes = next_changes_in_windows(&source, DEPOT, Some(3), 10, window(1_000)).await?;

    assert_eq!(Vec::<u64>::new(), changes);
    let listings = listings(&source);
    // 1,000 * (2^13 - 1) numbers is the first total past the 4,999,997 to cover.
    assert_eq!(13, listings.len(), "{listings:?}");
    assert_eq!(
        Some(&(Some(4_095_003), 5_000_000, 10)),
        listings.last(),
        "the last listing ends at the server's latest"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_windows_do_not_list_once_caught_up(_fb: FacebookInit) -> Result<()> {
    let source = FakeSource {
        latest: Some(30),
        ..with_changelists(&[3, 25, 26])
    };

    let caught_up = next_changes_in_windows(&source, DEPOT, Some(30), 10, window(5)).await?;

    assert_eq!(Vec::<u64>::new(), caught_up);
    assert_eq!(Vec::<(Option<u64>, u64, usize)>::new(), listings(&source));
    Ok(())
}

/// A server that lists three changelists whatever it is asked for.
struct ListsTooMany;

#[async_trait]
impl ChangeSource for ListsTooMany {
    async fn server_info(&self) -> Result<ServerInfo> {
        unimplemented!()
    }

    async fn latest_change(&self) -> Result<u64> {
        Ok(100)
    }

    async fn depots(&self) -> Result<Vec<Depot>> {
        unimplemented!()
    }

    async fn submitted_changes(
        &self,
        _depot_root: &str,
        _after: Option<u64>,
        _upto: u64,
        _limit: usize,
    ) -> Result<Vec<u64>> {
        Ok(vec![1, 2, 3])
    }

    async fn describe(&self, _change: u64) -> Result<Changelist> {
        unimplemented!()
    }

    async fn print(&self, _file: &FileRevision) -> Result<BoxStream<'static, Result<Bytes>>> {
        unimplemented!()
    }

    async fn integrations(
        &self,
        _files: &[&FileRevision],
    ) -> Result<HashMap<String, Vec<Integration>>> {
        unimplemented!()
    }
}

#[mononoke::fbinit_test]
async fn test_a_listing_longer_than_asked_for_is_an_error(_fb: FacebookInit) -> Result<()> {
    let error = next_changes_in_windows(&ListsTooMany, DEPOT, None, 2, window(10))
        .await
        .unwrap_err();

    assert_eq!(
        "listing //test1/ after None up to 10 returned 3 changelists, 2 asked for",
        error.to_string()
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_import_next_lists_in_windows_when_asked_to(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let repo: TestRepo = test_repo_factory::build_empty(fb).await?;
    let master = BookmarkKey::new("master")?;
    let source = FakeSource {
        latest: Some(5_000_000),
        ..two_changelists()
    };
    let options = ImportOptions {
        listing_window: Some(window(8)),
        ..Default::default()
    };

    let first = import_next(&ctx, &repo, &source, DEPOT, &master, 1, &options).await?;
    let second = import_next(&ctx, &repo, &source, DEPOT, &master, 1, &options).await?;

    let imported = |batch: &ImportedBatch| -> Vec<u64> {
        batch.changelists.iter().map(|c| c.change).collect()
    };
    assert_eq!((vec![7], vec![9]), (imported(&first), imported(&second)));
    assert_eq!(
        vec![(None, 8, 1), (Some(7), 15, 1)],
        listings(&source),
        "each batch lists 8 numbers after its resume point"
    );
    Ok(())
}
