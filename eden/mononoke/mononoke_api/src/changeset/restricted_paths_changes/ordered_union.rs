/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use futures::StreamExt;
use futures::TryStreamExt;
use futures::future::try_join;
use futures::stream;
use futures::stream::BoxStream;

use super::ChangedPath;
use crate::MononokeError;

/// Merge two unique component-ordered path streams. Keep endpoint metadata
/// when both views contain a path, before any page limit or policy lookup.
pub(super) fn ordered_union(
    left: BoxStream<'static, Result<ChangedPath, MononokeError>>,
    right: BoxStream<'static, Result<ChangedPath, MononokeError>>,
) -> BoxStream<'static, Result<ChangedPath, MononokeError>> {
    let state = (
        left.fuse(),
        right.fuse(),
        None::<ChangedPath>,
        None::<ChangedPath>,
    );
    stream::try_unfold(state, |(mut left, mut right, mut a, mut b)| async move {
        if a.is_none() && b.is_none() {
            (a, b) = try_join(left.try_next(), right.try_next()).await?;
        } else if a.is_none() {
            a = left.try_next().await?;
        } else if b.is_none() {
            b = right.try_next().await?;
        }
        let next = match (&a, &b) {
            (Some(left_path), Some(right_path)) if left_path.path() == right_path.path() => {
                b = None;
                a.take()
            }
            (Some(left_path), Some(right_path)) if left_path.path() < right_path.path() => a.take(),
            (Some(_), Some(_)) | (None, Some(_)) => b.take(),
            (Some(_), None) => a.take(),
            (None, None) => None,
        };
        Ok(next.map(|next| (next, (left, right, a, b))))
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use mononoke_macros::mononoke;
    use mononoke_types::MPath;

    use super::*;

    fn paths(names: &[&str]) -> Result<BoxStream<'static, Result<ChangedPath, MononokeError>>> {
        let paths = names
            .iter()
            .map(|name| MPath::new(*name).map(ChangedPath::Added))
            .collect::<Result<Vec<_>>>()?;
        Ok(stream::iter(paths.into_iter().map(Ok)).boxed())
    }

    #[mononoke::test]
    async fn union_is_component_ordered_and_deduplicated() -> Result<()> {
        let union = ordered_union(paths(&["a/b", "a.txt", "z"])?, paths(&["a/b", "a/c", "b"])?);
        let names: Vec<_> = union
            .map_ok(|path| path.path().to_string())
            .try_collect()
            .await?;
        assert_eq!(names, vec!["a/b", "a/c", "a.txt", "b", "z"]);
        Ok(())
    }

    #[mononoke::test]
    async fn union_does_not_turn_source_failure_into_completion() -> Result<()> {
        let error = MononokeError::InvalidRequest("source failed".into());
        let source = stream::iter(vec![Err(error)]).boxed();
        assert!(
            ordered_union(paths(&["a"])?, source)
                .try_collect::<Vec<_>>()
                .await
                .is_err()
        );
        Ok(())
    }
}
