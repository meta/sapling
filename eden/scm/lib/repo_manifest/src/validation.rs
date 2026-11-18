/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Path and security validation for manifest elements.
//!
//! Port of git-repo's `_CheckLocalPath` security check. Ensures paths
//! in the manifest do not escape the workspace via directory traversal,
//! absolute paths, or other dangerous patterns.

use std::path::Path;

use anyhow::Result;

/// Characters that are rejected in paths for security reasons.
/// These are Unicode bidi/formatting codepoints that can be used to
/// disguise malicious paths.
fn contains_dangerous_unicode(path: &str) -> bool {
    for c in path.chars() {
        match c {
            // Zero-width / bidi override codepoints
            '\u{200c}'..='\u{200f}' => return true,
            '\u{202a}'..='\u{202e}' => return true,
            '\u{206a}'..='\u{206f}' => return true,
            '\u{feff}' => return true,
            _ => {}
        }
    }
    false
}

/// Check if a path component matches `.git` case-insensitively.
fn is_dot_git_ci(component: &str) -> bool {
    component.eq_ignore_ascii_case(".git")
}

/// Core path validation implementing git-repo's `_CheckLocalPath`.
///
/// Parameters:
/// - `path`: The path string to validate
/// - `dir_ok`: If true, trailing slashes are allowed (for project name/path)
/// - `cwd_dot_ok`: If true, the path "." is allowed (for linkfile src, project path)
pub fn validate_path(path: &str, dir_ok: bool, cwd_dot_ok: bool) -> Result<()> {
    if path.is_empty() {
        anyhow::bail!("Path cannot be empty");
    }

    // Reject newlines
    if path.contains('\r') || path.contains('\n') {
        anyhow::bail!("Path must not contain newline characters: {path:?}");
    }

    // Reject tilde (Windows 8.3 filename attack vector)
    if path.contains('~') {
        anyhow::bail!("Path must not contain '~': {path}");
    }

    // Reject dangerous Unicode codepoints
    if contains_dangerous_unicode(path) {
        anyhow::bail!("Path contains dangerous Unicode formatting characters: {path:?}");
    }

    // Reject trailing slash unless dir_ok
    if !dir_ok && path.ends_with('/') {
        anyhow::bail!("Path must not end with '/': {path}");
    }

    let p = Path::new(path);

    // Must use `has_root`, not `is_absolute`.
    // On Windows, `is_absolute` is false for `/foo`. `/foo` has no drive prefix.
    if p.has_root() {
        anyhow::bail!("Path must be relative: {path}");
    }

    // Check each slash-separated component directly, since Path::components()
    // normalizes away `.` in the middle of paths (e.g., `foo/./bar` → `foo/bar`).
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    for (i, part) in components.iter().enumerate() {
        match *part {
            "." => {
                // "." is only OK as a standalone path when cwd_dot_ok is true
                if !(cwd_dot_ok && components.len() == 1) {
                    anyhow::bail!("Path cannot contain '.': {path}");
                }
            }
            ".." => {
                anyhow::bail!("Path cannot contain '..': {path}");
            }
            _ => {
                // Case-insensitive .git check
                if is_dot_git_ci(part) {
                    anyhow::bail!("Path cannot use reserved '.git' directory: {path}");
                }
                // .repo check (starts_with ".repo" to catch .repoconfig etc.)
                if part.starts_with(".repo") {
                    anyhow::bail!("Path cannot use reserved '.repo' directory: {path}");
                }
                // Only the first component can be ".repo*"; inner components
                // are also checked above, but this catches the case where
                // e.g. "foo/.repoconfig" should also be rejected
                let _ = i; // used above in the loop
            }
        }
    }

    Ok(())
}

/// Validate a project path.
///
/// Project paths allow "." as a standalone path (cwd_dot_ok=true) and
/// allow trailing slashes (dir_ok=true).
pub fn validate_project_path(path: &str) -> Result<()> {
    validate_path(path, true, true)
}

/// Validate a project name.
///
/// Project names allow trailing slashes (dir_ok=true) but do NOT allow
/// "." as a standalone path (cwd_dot_ok=false).
pub fn validate_project_name(name: &str) -> Result<()> {
    validate_path(name, true, false)
}

/// Validate a copyfile/linkfile destination path.
///
/// File destinations do NOT allow trailing slashes (dir_ok=false) and do NOT
/// allow "." as a standalone path (cwd_dot_ok=false).
pub fn validate_file_dest(dest: &str) -> Result<()> {
    validate_path(dest, false, false)
}

/// Validate an include file name.
///
/// Include names must not traverse directories upward or use reserved paths.
/// Does NOT allow trailing slashes or "." as standalone path.
pub fn validate_include_name(name: &str) -> Result<()> {
    validate_path(name, false, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Basic path validation ---

    #[test]
    fn test_valid_paths() {
        assert!(validate_project_path("art").is_ok());
        assert!(validate_project_path("platform/art").is_ok());
        assert!(validate_project_path("a/b/c/d").is_ok());
    }

    #[test]
    fn test_absolute_path_rejected() {
        assert!(validate_project_path("/absolute/path").is_err());
    }

    #[test]
    fn test_parent_traversal_rejected() {
        assert!(validate_project_path("../escape").is_err());
        assert!(validate_project_path("foo/../bar").is_err());
        assert!(validate_project_path("foo/../../etc").is_err());
    }

    #[test]
    fn test_dot_component_rejected() {
        // Standalone "." is OK for project paths (cwd_dot_ok=true)
        assert!(validate_project_path(".").is_ok());
        // But not in the middle
        assert!(validate_project_path("foo/./bar").is_err());
        assert!(validate_project_path("./foo").is_err());
    }

    #[test]
    fn test_dot_repo_rejected() {
        assert!(validate_project_path(".repo").is_err());
        assert!(validate_project_path(".repo/manifests").is_err());
    }

    #[test]
    fn test_dot_git_rejected() {
        assert!(validate_project_path(".git").is_err());
        assert!(validate_project_path("foo/.git/bar").is_err());
    }

    #[test]
    fn test_empty_path_rejected() {
        assert!(validate_project_path("").is_err());
    }

    // --- MR-014: Enhanced _CheckLocalPath port ---

    #[test]
    fn test_trailing_slash_rejected_for_file_dest() {
        assert!(validate_file_dest("foo/").is_err());
        // But OK for project path
        assert!(validate_project_path("foo/").is_ok());
    }

    #[test]
    fn test_tilde_rejected() {
        assert!(validate_project_path("foo~1").is_err());
        assert!(validate_project_path("~user/bar").is_err());
    }

    #[test]
    fn test_dangerous_unicode_rejected() {
        // Zero-width non-joiner
        assert!(validate_project_path("foo\u{200c}bar").is_err());
        // Right-to-left override
        assert!(validate_project_path("foo\u{202e}bar").is_err());
        // BOM
        assert!(validate_project_path("\u{feff}foo").is_err());
    }

    #[test]
    fn test_newline_rejected() {
        assert!(validate_project_path("foo\nbar").is_err());
        assert!(validate_project_path("foo\rbar").is_err());
    }

    #[test]
    fn test_case_insensitive_git_rejected() {
        assert!(validate_project_path(".GIT").is_err());
        assert!(validate_project_path(".Git").is_err());
        assert!(validate_project_path("foo/.GIT/bar").is_err());
    }

    #[test]
    fn test_repoconfig_rejected() {
        assert!(validate_project_path(".repoconfig").is_err());
        assert!(validate_project_path(".repofoo").is_err());
    }

    #[test]
    fn test_dot_standalone_for_project_path_ok() {
        // Project path allows "." standalone (cwd_dot_ok=true)
        assert!(validate_project_path(".").is_ok());
    }

    #[test]
    fn test_dot_standalone_for_file_dest_rejected() {
        // File dest does NOT allow "." standalone (cwd_dot_ok=false)
        assert!(validate_file_dest(".").is_err());
    }

    #[test]
    fn test_dot_standalone_for_project_name_rejected() {
        // Project name does NOT allow "." standalone (cwd_dot_ok=false)
        assert!(validate_project_name(".").is_err());
    }

    #[test]
    fn test_project_name_dotgit_rejected() {
        // .git and case variants must be rejected in project names
        assert!(validate_project_name(".git").is_err());
        assert!(validate_project_name(".GIT").is_err());
        assert!(validate_project_name(".Git").is_err());
        assert!(validate_project_name("foo/.git").is_err());
    }

    #[test]
    fn test_project_name_dotrepo_rejected() {
        assert!(validate_project_name(".repo").is_err());
        assert!(validate_project_name(".repoconfig").is_err());
    }

    // --- MR-015: Include path validation ---

    #[test]
    fn test_include_path_traversal_rejected() {
        assert!(validate_include_name("../secret.xml").is_err());
        assert!(validate_include_name("foo/../bar.xml").is_err());
    }

    #[test]
    fn test_include_valid_path() {
        assert!(validate_include_name("extra.xml").is_ok());
        assert!(validate_include_name("subdir/extra.xml").is_ok());
    }

    #[test]
    fn test_include_absolute_rejected() {
        assert!(validate_include_name("/etc/passwd").is_err());
    }
}
