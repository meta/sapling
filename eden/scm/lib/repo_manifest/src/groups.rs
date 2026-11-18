/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Group filtering for manifest projects.
//!
//! git-repo uses groups to selectively sync subsets of projects.
//! Groups are specified as comma/whitespace-separated lists, and can use
//! negation with a `-` prefix (e.g., `-notdefault`).

use std::collections::HashSet;

/// Parse a group string into a set of group names.
///
/// Groups can be separated by commas and/or whitespace.
/// Empty strings and whitespace-only entries are filtered out.
pub fn parse_group_list(groups: &str) -> HashSet<String> {
    groups
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Determine whether a project's groups match a filter.
///
/// The filter uses the same syntax as `repo init -g`:
/// - A group name matches projects that belong to that group
/// - A `-group` name excludes projects in that group
/// - `default` is a special group that includes most projects
/// - `notdefault` marks projects that are not in the default set
/// - `all` is a special group — every project implicitly belongs to it
///
/// If `filter_groups` is empty, all projects match.
pub fn matches_groups(project_groups: &HashSet<String>, filter_groups: &[String]) -> bool {
    if filter_groups.is_empty() {
        return true;
    }

    // git-repo implicitly adds "all" and (unless "notdefault" is present)
    // "default" to every project's group set before matching.
    let mut expanded = project_groups.clone();
    expanded.insert("all".to_string());
    if !project_groups.contains("notdefault") {
        expanded.insert("default".to_string());
    }

    // Sequential evaluation matching git-repo's MatchesGroups: last matching filter wins.
    // If only negative filters, default to included (match everything not excluded).
    let has_positive_filters = filter_groups.iter().any(|f| !f.starts_with('-'));
    let mut matched = !has_positive_filters;

    for filter in filter_groups {
        if let Some(negated) = filter.strip_prefix('-') {
            if expanded.contains(negated) {
                matched = false;
            }
        } else if expanded.contains(filter.as_str()) {
            matched = true;
        }
    }

    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_group_list_comma_separated() {
        let groups = parse_group_list("pdk,tools,build");
        assert_eq!(groups.len(), 3);
        assert!(groups.contains("pdk"));
        assert!(groups.contains("tools"));
        assert!(groups.contains("build"));
    }

    #[test]
    fn test_parse_group_list_whitespace() {
        let groups = parse_group_list("pdk tools  build");
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn test_parse_group_list_mixed() {
        let groups = parse_group_list("pdk, tools , build");
        assert_eq!(groups.len(), 3);
    }

    #[test]
    fn test_parse_group_list_empty() {
        let groups = parse_group_list("");
        assert!(groups.is_empty());
    }

    #[test]
    fn test_matches_groups_empty_filter_matches_all() {
        let project_groups: HashSet<String> = ["pdk"].iter().map(|s| s.to_string()).collect();
        assert!(matches_groups(&project_groups, &[]));
    }

    #[test]
    fn test_matches_groups_positive_match() {
        let project_groups: HashSet<String> =
            ["pdk", "tools"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["pdk".to_string()];
        assert!(matches_groups(&project_groups, &filter));
    }

    #[test]
    fn test_matches_groups_positive_no_match() {
        let project_groups: HashSet<String> = ["tools"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["pdk".to_string()];
        assert!(!matches_groups(&project_groups, &filter));
    }

    #[test]
    fn test_matches_groups_negative_excludes() {
        let project_groups: HashSet<String> =
            ["notdefault"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["-notdefault".to_string()];
        assert!(!matches_groups(&project_groups, &filter));
    }

    #[test]
    fn test_matches_groups_negative_only_includes_non_matching() {
        let project_groups: HashSet<String> = ["pdk"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["-notdefault".to_string()];
        assert!(matches_groups(&project_groups, &filter));
    }

    #[test]
    fn test_matches_groups_positive_and_negative() {
        let project_groups: HashSet<String> = ["pdk", "notdefault"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let filter = vec!["pdk".to_string(), "-notdefault".to_string()];
        // Excluded by -notdefault even though pdk matches
        assert!(!matches_groups(&project_groups, &filter));
    }

    /// CLI-025: `--groups all` should include all projects regardless of their groups.
    #[test]
    fn test_matches_groups_all_matches_everything() {
        // A project with no specific groups should still match "all"
        let project_groups: HashSet<String> = HashSet::new();
        let filter = vec!["all".to_string()];
        assert!(
            matches_groups(&project_groups, &filter),
            "'all' should match every project"
        );
    }

    #[test]
    fn test_matches_groups_all_matches_any_project() {
        let project_groups: HashSet<String> =
            ["custom_group"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["all".to_string()];
        assert!(
            matches_groups(&project_groups, &filter),
            "'all' should match projects with any groups"
        );
    }

    #[test]
    fn test_matches_groups_all_with_negative_filter() {
        // "all,-notdefault" should include everything except notdefault projects
        let project_groups: HashSet<String> =
            ["notdefault"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["all".to_string(), "-notdefault".to_string()];
        assert!(
            !matches_groups(&project_groups, &filter),
            "'all,-notdefault' should exclude notdefault projects"
        );
    }

    #[test]
    fn test_matches_groups_default_implicit_for_non_notdefault() {
        // Projects without "notdefault" implicitly belong to "default"
        let project_groups: HashSet<String> = ["pdk"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["default".to_string()];
        assert!(
            matches_groups(&project_groups, &filter),
            "Projects without 'notdefault' should implicitly match 'default'"
        );
    }

    #[test]
    fn test_matches_groups_default_not_implicit_for_notdefault() {
        // Projects with "notdefault" should NOT implicitly match "default"
        let project_groups: HashSet<String> =
            ["notdefault"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["default".to_string()];
        assert!(
            !matches_groups(&project_groups, &filter),
            "Projects with 'notdefault' should not match 'default'"
        );
    }

    #[test]
    fn test_matches_groups_order_dependent_negative_then_positive() {
        // git-repo resolves filters in order; last matching filter wins.
        // "-group1,group2" on a project in both → group2 wins → included
        let project_groups: HashSet<String> =
            ["group1", "group2"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["-group1".to_string(), "group2".to_string()];
        assert!(
            matches_groups(&project_groups, &filter),
            "later positive filter should override earlier negative"
        );
    }

    #[test]
    fn test_matches_groups_order_dependent_positive_then_negative() {
        // "group1,-group2" on a project in both → -group2 wins → excluded
        let project_groups: HashSet<String> =
            ["group1", "group2"].iter().map(|s| s.to_string()).collect();
        let filter = vec!["group1".to_string(), "-group2".to_string()];
        assert!(
            !matches_groups(&project_groups, &filter),
            "later negative filter should override earlier positive"
        );
    }
}
