/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Utility traits and functions.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Result;
use chrono::DateTime;
use chrono::NaiveDateTime;
use num::PrimInt;

/// Returns `single` if the given number is 1, or `plural` otherwise.
pub(crate) fn plural<'a, T: PrimInt>(n: T, single: &'a str, plural: &'a str) -> &'a str {
    if n == T::one() { single } else { plural }
}

/// Whether a `partial_info` verdict says partial. This is the signal scsc
/// reads.
pub(crate) fn is_partial_verdict(
    partial_info: &Option<scs_client_raw::thrift::PartialResponseInfo>,
) -> bool {
    partial_info.as_ref().is_some_and(|info| info.partial)
}

/// Omission groups carried by a `partial_info` verdict, if any.
pub(crate) fn partial_groups(
    partial_info: &Option<scs_client_raw::thrift::PartialResponseInfo>,
) -> Vec<scs_client_raw::thrift::OmittedRestrictionGroup> {
    partial_info
        .as_ref()
        .and_then(|info| info.omitted_by_restriction.clone())
        .unwrap_or_default()
}

/// Request partial responses with withheld paths when asked: the flag
/// opts the request into partials and asks for the path lists.
pub(crate) fn partial_options(
    include_paths: bool,
) -> Option<scs_client_raw::thrift::PartialOptions> {
    include_paths.then(|| scs_client_raw::thrift::PartialOptions {
        include_omitted_paths: Some(true),
        ..Default::default()
    })
}

/// Merge omission groups from several responses into one deduplicated list:
/// groups naming the same restriction (root, ACL, request group) combine
/// their counts and union their path lists. Sorted for stable output.
pub(crate) fn merge_partial_groups(
    groups: Vec<scs_client_raw::thrift::OmittedRestrictionGroup>,
) -> Vec<scs_client_raw::thrift::OmittedRestrictionGroup> {
    #[derive(Default)]
    struct Merged {
        count: i64,
        omitted: BTreeSet<String>,
        downgraded: BTreeSet<String>,
    }
    let mut combined: BTreeMap<(Option<String>, String, String), Merged> = BTreeMap::new();
    for group in groups {
        let merged = combined
            .entry((
                group.restriction_root,
                group.repo_region_acl,
                group.permission_request_group,
            ))
            .or_default();
        merged.count += group.omitted_count;
        merged
            .omitted
            .extend(group.omitted_paths.unwrap_or_default());
        merged
            .downgraded
            .extend(group.downgraded_paths.unwrap_or_default());
    }
    combined
        .into_iter()
        .map(
            |((restriction_root, repo_region_acl, permission_request_group), merged)| {
                let omitted: Vec<String> = merged.omitted.into_iter().collect();
                let downgraded: Vec<String> = merged.downgraded.into_iter().collect();
                scs_client_raw::thrift::OmittedRestrictionGroup {
                    restriction_root,
                    repo_region_acl,
                    permission_request_group,
                    omitted_count: merged.count,
                    omitted_paths: (!omitted.is_empty()).then_some(omitted),
                    downgraded_paths: (!downgraded.is_empty()).then_some(downgraded),
                    ..Default::default()
                }
            },
        )
        .collect()
}

/// Print the partial-response note to stderr: one count line per denying
/// restriction, then one line per listed omitted and downgraded path.
/// Always stderr so stdout (including `--json`) stays machine-readable.
/// Call once, after rendering output succeeds.
pub(crate) fn print_partial_note(groups: Vec<scs_client_raw::thrift::OmittedRestrictionGroup>) {
    eprintln!("note: response is partial: some paths were omitted due to restricted-path access");
    for group in merge_partial_groups(groups) {
        let count = group.omitted_count;
        let paths = plural(count, "path", "paths");
        match &group.restriction_root {
            Some(root) => eprintln!(
                "note: omitted {count} {paths} under {root} ({}; request access to: {})",
                group.repo_region_acl, group.permission_request_group,
            ),
            None => eprintln!(
                "note: omitted {count} {paths} ({}; request access to: {})",
                group.repo_region_acl, group.permission_request_group,
            ),
        }
        for path in group.omitted_paths.unwrap_or_default() {
            eprintln!("note: omitted path: {path}");
        }
        for path in group.downgraded_paths.unwrap_or_default() {
            eprintln!("note: downgraded path: {path}");
        }
    }
}

fn byte_count(size: i64, unit_single: &str, unit_plural: &str, multiple: &[&str]) -> String {
    const UNIT_LIMIT: i64 = 9999;
    match (size, multiple.split_last()) {
        (i64::MIN..=UNIT_LIMIT, _) | (_, None) => {
            format!("{}{}", size, plural(size, unit_single, unit_plural))
        }
        (size, Some((last_multiple, multiple))) => {
            let mut divisor = 1024;
            for unit in multiple.iter() {
                if size < (UNIT_LIMIT + 1) * divisor {
                    return format!("{:.2}{}", (size as f64) / (divisor as f64), unit);
                }
                divisor *= 1024;
            }
            format!("{:.2}{}", (size as f64) / (divisor as f64), last_multiple)
        }
    }
}

/// Convert a byte count to a human-readable representation of the byte count
/// using appropriate IEC suffixes.
pub(crate) fn byte_count_iec(size: i64) -> String {
    let suffixes = [
        " KiB", " MiB", " GiB", " TiB", " PiB", " EiB", " ZiB", " YiB",
    ];
    byte_count(size, " byte", " bytes", &suffixes)
}

/// Convert a byte count to a human-readable representation of the byte count
/// using short suffixes.
pub(crate) fn byte_count_short(size: i64) -> String {
    byte_count(size, "", "", &["K", "M", "G", "T", "P", "E", "Z", "Y"])
}

pub(crate) fn convert_to_ts(date_str: Option<&str>) -> Result<Option<i64>> {
    date_str
        .map(|date_str| parse_date(date_str).map(|(timestamp, _offset)| timestamp))
        .transpose()
}

/// Parse a date given as "YYYY-MM-DD HH:MM:SS +HH:MM", as "YYYY-MM-DD HH:MM:SS"
/// (UTC) or as a UNIX timestamp, into the timestamp and the offset east of
/// UTC in seconds. The date must be after 1970-01-01 00:00:00 UTC.
pub(crate) fn parse_date(date_str: &str) -> Result<(i64, i32)> {
    let (timestamp, offset) =
        if let Ok(date) = DateTime::parse_from_str(date_str, "%Y-%m-%d %H:%M:%S %:z") {
            (date.timestamp(), date.offset().local_minus_utc())
        } else if let Ok(naive) = NaiveDateTime::parse_from_str(date_str, "%Y-%m-%d %H:%M:%S") {
            (naive.and_utc().timestamp(), 0)
        } else {
            (date_str.parse::<i64>()?, 0)
        };
    if timestamp > 0 {
        return Ok((timestamp, offset));
    }
    anyhow::bail!("The given date or timestamp must be after 1970-01-01 00:00:00 UTC: {date_str:?}")
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    fn group(
        root: Option<&str>,
        acl: &str,
        request_group: &str,
        count: i64,
        omitted: &[&str],
        downgraded: &[&str],
    ) -> scs_client_raw::thrift::OmittedRestrictionGroup {
        scs_client_raw::thrift::OmittedRestrictionGroup {
            restriction_root: root.map(str::to_string),
            repo_region_acl: acl.to_string(),
            permission_request_group: request_group.to_string(),
            omitted_count: count,
            omitted_paths: (!omitted.is_empty())
                .then(|| omitted.iter().map(|path| path.to_string()).collect()),
            downgraded_paths: (!downgraded.is_empty())
                .then(|| downgraded.iter().map(|path| path.to_string()).collect()),
            ..Default::default()
        }
    }

    #[mononoke::test]
    fn merge_combines_same_restriction_and_sorts() {
        let merged = merge_partial_groups(vec![
            group(Some("tent_b"), "REPO_REGION:b", "b", 1, &[], &[]),
            group(Some("tent_a"), "REPO_REGION:a", "a", 2, &[], &[]),
            group(Some("tent_a"), "REPO_REGION:a", "a", 3, &[], &[]),
            group(None, "REPO_REGION:m", "m", 1, &[], &[]),
        ]);
        let summary: Vec<_> = merged
            .iter()
            .map(|group| (group.restriction_root.clone(), group.omitted_count))
            .collect();
        // Rootless groups sort first; tent_a's counts combine.
        assert_eq!(
            summary,
            vec![
                (None, 1),
                (Some("tent_a".to_string()), 5),
                (Some("tent_b".to_string()), 1),
            ],
        );
    }

    #[mononoke::test]
    fn merge_distinguishes_request_group() {
        // Same root and ACL but different request groups stay separate.
        let merged = merge_partial_groups(vec![
            group(Some("tent"), "REPO_REGION:acl", "group_one", 1, &[], &[]),
            group(Some("tent"), "REPO_REGION:acl", "group_two", 1, &[], &[]),
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[mononoke::test]
    fn merge_unions_path_lists_sorted_and_deduped() {
        let merged = merge_partial_groups(vec![
            group(
                Some("tent"),
                "REPO_REGION:acl",
                "group",
                2,
                &["tent/b", "tent/a"],
                &["tent/changed"],
            ),
            group(
                Some("tent"),
                "REPO_REGION:acl",
                "group",
                1,
                &["tent/a", "tent/c"],
                &[],
            ),
            group(Some("bare"), "REPO_REGION:acl", "group", 1, &[], &[]),
        ]);
        assert_eq!(merged.len(), 2);
        let tent = merged
            .iter()
            .find(|group| group.restriction_root.as_deref() == Some("tent"))
            .expect("tent group present");
        assert_eq!(tent.omitted_count, 3);
        assert_eq!(
            tent.omitted_paths,
            Some(vec![
                "tent/a".to_string(),
                "tent/b".to_string(),
                "tent/c".to_string(),
            ]),
        );
        assert_eq!(
            tent.downgraded_paths,
            Some(vec!["tent/changed".to_string()]),
        );
        let bare = merged
            .iter()
            .find(|group| group.restriction_root.as_deref() == Some("bare"))
            .expect("bare group present");
        assert_eq!(bare.omitted_paths, None);
        assert_eq!(bare.downgraded_paths, None);
    }

    /// What it tests: the accepted date formats.
    /// Expected: an explicit offset is kept (seconds east of UTC); a naive
    /// date and a timestamp are UTC; dates before the epoch and anything else
    /// fail.
    #[mononoke::test]
    fn test_parse_date() -> Result<()> {
        assert_eq!(
            parse_date("2026-10-30 10:00:00 +01:00")?,
            (1_793_350_800, 3600)
        );
        assert_eq!(parse_date("2026-10-30 09:00:00")?, (1_793_350_800, 0));
        assert_eq!(parse_date("1793350800")?, (1_793_350_800, 0));
        assert!(parse_date("0").is_err());
        assert!(parse_date("yesterday").is_err());
        Ok(())
    }
}
