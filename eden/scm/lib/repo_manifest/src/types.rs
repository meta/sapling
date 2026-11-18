/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Resolved manifest types.
//!
//! These types represent the fully resolved manifest after attribute
//! inheritance, URL resolution, and validation have been applied.

use std::collections::HashSet;
use std::path::PathBuf;

/// A resolved Git remote.
#[derive(Clone, Debug)]
pub struct Remote {
    pub name: String,
    pub alias: Option<String>,
    pub fetch_url: String,
    pub push_url: Option<String>,
    pub review: Option<String>,
    pub revision: Option<String>,
    // MR-012: Remote annotations
    pub annotations: Vec<Annotation>,
}

/// Resolved default values.
#[derive(Clone, Debug)]
pub struct Defaults {
    pub remote: Option<String>,
    pub revision: Option<String>,
    pub dest_branch: Option<String>,
    pub upstream: Option<String>,
    pub sync_j: Option<u32>,
    pub sync_c: bool,
    pub sync_s: bool,
    pub sync_tags: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            remote: None,
            revision: None,
            dest_branch: None,
            upstream: None,
            sync_j: None,
            sync_c: false,
            sync_s: false,
            sync_tags: true,
        }
    }
}

/// An index into the manifest's project list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectId(pub(crate) usize);

/// A fully resolved project.
#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub remote_name: String,
    pub revision: String,
    pub dest_branch: Option<String>,
    pub upstream: Option<String>,
    pub groups: HashSet<String>,
    pub sync_c: bool,
    pub sync_s: bool,
    pub sync_tags: bool,
    pub clone_depth: Option<u32>,
    pub rebase: bool,
    pub annotations: Vec<Annotation>,
    pub copy_files: Vec<CopyFile>,
    pub link_files: Vec<LinkFile>,
    pub subprojects: Vec<ProjectId>,
    /// True if this is a top-level project (not a subproject).
    pub is_top_level: bool,
}

impl Project {
    /// The display name used by `repo list` — the path relative to the workspace root.
    pub fn relpath(&self) -> &str {
        self.path.to_str().unwrap_or(&self.name)
    }
}

/// A file copy operation (from project checkout to workspace root).
#[derive(Clone, Debug)]
pub struct CopyFile {
    pub src: String,
    pub dest: String,
}

/// A symlink operation (from project checkout to workspace root).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinkFile {
    pub src: String,
    pub dest: String,
}

/// Key-value annotation on a project.
#[derive(Clone, Debug)]
pub struct Annotation {
    pub name: String,
    pub value: String,
    pub keep: bool,
}

/// Superproject configuration.
#[derive(Clone, Debug)]
pub struct Superproject {
    pub name: String,
    pub remote: Option<String>,
    pub revision: Option<String>,
}

/// Contact information for bug reports.
#[derive(Clone, Debug)]
pub struct Contactinfo {
    pub bugurl: String,
}

/// Repository hooks configuration.
#[derive(Clone, Debug)]
pub struct RepoHooks {
    pub in_project: String,
    pub enabled_list: String,
}

/// MR-010: Resolved submanifest.
#[derive(Clone, Debug)]
pub struct Submanifest {
    pub name: String,
    pub remote: Option<String>,
    pub project: Option<String>,
    pub manifest_name: Option<String>,
    pub revision: Option<String>,
    pub path: Option<String>,
    pub groups: HashSet<String>,
    pub default_groups: Vec<String>,
    pub annotations: Vec<Annotation>,
}
