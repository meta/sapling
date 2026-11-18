/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Manifest parser for git-repo XML format.
//!
//! This crate parses the manifest XML files used by git-repo to describe
//! multi-repository workspaces. It implements the full format including
//! `<include>`, `<extend-project>`, `<remove-project>`, and attribute
//! inheritance.
//!
//! # Architecture
//!
//! The parsing pipeline has three stages:
//!
//! 1. **XML Deserialization** (`raw`): serde + quick-xml deserializes XML
//!    into raw structs that mirror the schema exactly.
//!
//! 2. **Include Flattening** (`ManifestNodes`): Recursively processes
//!    `<include>` elements, flattening all nodes into a single ordered list.
//!
//! 3. **Resolution** (`resolver`): Transforms raw types into resolved types
//!    by applying defaults, resolving references, and validating constraints.

mod attr_check;
pub mod groups;
mod hash;
pub mod includes;
pub mod raw;
mod resolver;
mod serialize;
pub mod types;
pub mod validation;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;

pub use crate::hash::is_git_hash;
use crate::types::Defaults;
use crate::types::Project;
use crate::types::ProjectId;
use crate::types::Remote;

/// Intermediate representation used during include flattening.
///
/// Project-related elements are kept in order because `<extend-project>`
/// and `<remove-project>` operate on previously-defined projects.
#[derive(Debug)]
pub(crate) enum ProjectElement {
    /// A project element, with optional parent revision from include tag (MR-001).
    Project(raw::Project, Option<String>),
    ExtendProject(raw::ExtendProject),
    RemoveProject(raw::RemoveProject),
    RepoHooks(raw::RepoHooks),
}

/// Holds all nodes collected from parsing the manifest and its includes.
#[derive(Default, Debug)]
pub(crate) struct ManifestNodes {
    /// Tracks included files to detect circular includes.
    include_stack: HashSet<String>,

    pub remotes: Vec<raw::Remote>,
    pub defaults: Vec<raw::Defaults>,
    pub notices: Vec<raw::Notice>,
    pub manifest_servers: Vec<raw::ManifestServer>,
    pub project_elements: Vec<ProjectElement>,
    pub superprojects: Vec<raw::Superproject>,
    pub contactinfo: Option<raw::Contactinfo>,
    // MR-010: Submanifest support
    pub submanifests: Vec<raw::Submanifest>,
}

impl ManifestNodes {
    /// Parse a manifest XML and recursively process includes.
    fn process_xml(
        &mut self,
        xml: &[u8],
        open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        parent_groups: &str,
        parent_revision: Option<&str>,
        deny_unknown: bool,
    ) -> Result<()> {
        // Strict attribute validation: pre-scan XML events and check each
        // element's attributes against a per-element allowlist. This rejects
        // unknown *attributes*; unknown *elements* are handled by the permissive
        // parse below (and the resolver's per-element checks).
        if deny_unknown {
            attr_check::validate_no_unknown_attributes(xml)?;
        }

        let manifest: raw::Manifest = quick_xml::de::from_reader(xml)
            .map_err(|e| anyhow::anyhow!("Failed to parse manifest XML: {e}"))?;

        for node in manifest.children {
            match node {
                raw::ManifestChild::Include(inc) => {
                    // MR-015: Validate include path
                    validation::validate_include_name(&inc.name)?;

                    // Circular include detection
                    if !self.include_stack.insert(inc.name.clone()) {
                        anyhow::bail!("Circular include detected: {}", inc.name);
                    }

                    let included_xml = open_fn(&inc.name)?;

                    // Propagate groups from include tag to child projects
                    let mut include_groups = inc.groups;
                    if !parent_groups.is_empty() {
                        if !include_groups.is_empty() {
                            include_groups.push(',');
                        }
                        include_groups.push_str(parent_groups);
                    }

                    // MR-001: Include revision inheritance. git-repo copies the `revision`
                    // of THIS include element onto the included file's top-level projects
                    // only; an outer include's revision does not flow through a nested
                    // include that has no `revision` of its own.
                    self.process_xml(
                        &included_xml,
                        open_fn,
                        &include_groups,
                        inc.revision.as_deref(),
                        deny_unknown,
                    )?;
                    self.include_stack.remove(&inc.name);
                }
                raw::ManifestChild::Remote(r) => {
                    self.remotes.push(r);
                }
                raw::ManifestChild::Default(d) => {
                    self.defaults.push(d);
                }
                raw::ManifestChild::Notice(n) => {
                    self.notices.push(n);
                }
                raw::ManifestChild::ManifestServer(ms) => {
                    self.manifest_servers.push(ms);
                }
                raw::ManifestChild::Project(mut p) => {
                    // Propagate include-level groups to project
                    if !parent_groups.is_empty() {
                        if !p.groups.is_empty() {
                            p.groups.push(',');
                        }
                        p.groups.push_str(parent_groups);
                    }
                    // MR-001: Store parent revision alongside the project
                    self.project_elements.push(ProjectElement::Project(
                        p,
                        parent_revision.map(|s| s.to_string()),
                    ));
                }
                raw::ManifestChild::ExtendProject(mut ep) => {
                    // MR-006: Include groups apply to extend-project too
                    if !parent_groups.is_empty() {
                        if !ep.groups.is_empty() {
                            ep.groups.push(',');
                        }
                        ep.groups.push_str(parent_groups);
                    }
                    self.project_elements
                        .push(ProjectElement::ExtendProject(ep));
                }
                raw::ManifestChild::RemoveProject(rp) => {
                    self.project_elements
                        .push(ProjectElement::RemoveProject(rp));
                }
                raw::ManifestChild::RepoHooks(rh) => {
                    self.project_elements.push(ProjectElement::RepoHooks(rh));
                }
                raw::ManifestChild::Superproject(sp) => {
                    self.superprojects.push(sp);
                }
                raw::ManifestChild::Contactinfo(ci) => {
                    self.contactinfo = Some(ci);
                }
                // MR-010: Submanifest
                raw::ManifestChild::Submanifest(sm) => {
                    self.submanifests.push(sm);
                }
                raw::ManifestChild::Unknown => {
                    if deny_unknown {
                        anyhow::bail!("Unknown element in <manifest> (strict parse mode)");
                    }
                }
            }
        }

        Ok(())
    }
}

/// Options controlling manifest parsing strictness.
#[derive(Clone, Debug, Default)]
pub struct ParseOptions {
    /// When true, parsing fails on unknown XML elements instead of silently
    /// ignoring them. The default (`false`) preserves permissive parsing so
    /// existing callers are unaffected.
    pub deny_unknown: bool,
}

/// Options controlling static serialization.
#[derive(Clone, Debug, Default)]
pub struct StaticOptions {
    /// Omit the `dest-branch` git-repo derives from a symbolic revision. One
    /// declared in the source manifest is still emitted. Default is git-repo parity.
    pub suppress_dest_branch: bool,

    /// Emit this as every remote's `fetch`, instead of the parsed value.
    ///
    /// For callers that own the endpoint rather than inheriting whatever the
    /// source manifest happens to declare. `pushurl` and `push` are untouched.
    pub remote_fetch_override: Option<String>,
}

/// A fully resolved manifest.
#[derive(Debug)]
pub struct Manifest {
    pub(crate) remotes: HashMap<String, Remote>,
    pub(crate) defaults: Defaults,
    pub(crate) projects: Vec<Project>,
    pub(crate) project_map: BTreeMap<String, Vec<ProjectId>>,
    pub(crate) notice: Option<String>,
    pub(crate) manifest_server: Option<String>,
    pub(crate) superproject: Option<types::Superproject>,
    pub(crate) contactinfo: Option<types::Contactinfo>,
    pub(crate) repo_hooks: Option<types::RepoHooks>,
    // MR-010: Submanifests
    pub(crate) submanifests: Vec<types::Submanifest>,
}

impl Manifest {
    /// Parse a manifest from XML bytes.
    ///
    /// `open_fn` is called to load included manifest files. It receives
    /// the filename from the `<include name="...">` attribute and should
    /// return the file contents.
    pub fn from_xml(xml: &[u8], open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>) -> Result<Self> {
        Self::from_xml_with_locals(xml, open_fn, &[])
    }

    /// Parse a manifest from XML bytes, merging in local manifest overlays.
    pub fn from_xml_with_locals(
        xml: &[u8],
        open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        local_manifests: &[Vec<u8>],
    ) -> Result<Self> {
        Self::from_xml_impl(xml, open_fn, local_manifests, &ParseOptions::default())
    }

    /// Parse a manifest from XML bytes with explicit parse options.
    ///
    /// With `ParseOptions::deny_unknown` set, parsing fails on unknown elements
    /// rather than silently ignoring them. `from_xml` and `from_xml_with_locals`
    /// delegate here with permissive defaults, so existing callers are unchanged.
    pub fn from_xml_with_options(
        xml: &[u8],
        open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        options: &ParseOptions,
    ) -> Result<Self> {
        Self::from_xml_impl(xml, open_fn, &[], options)
    }

    fn from_xml_impl(
        xml: &[u8],
        open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
        local_manifests: &[Vec<u8>],
        options: &ParseOptions,
    ) -> Result<Self> {
        let mut nodes = ManifestNodes::default();
        nodes.process_xml(xml, open_fn, "", None, options.deny_unknown)?;
        for local_xml in local_manifests {
            nodes.process_xml(local_xml, open_fn, "", None, options.deny_unknown)?;
        }
        resolver::resolve_manifest(nodes, options.deny_unknown)
    }

    /// Parse a manifest from an XML file on disk.
    ///
    /// Includes are resolved relative to the directory containing `path`.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let xml = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("Failed to read manifest file {path:?}: {e}"))?;

        let base_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Manifest path has no parent directory"))?
            .to_path_buf();

        let mut open_fn = |name: &str| -> Result<Vec<u8>> {
            let include_path = base_dir.join(name);
            std::fs::read(&include_path).map_err(|e| {
                anyhow::anyhow!("Failed to read included manifest {include_path:?}: {e}")
            })
        };

        Self::from_xml(&xml, &mut open_fn)
    }

    /// Iterate over all top-level projects (sorted by path via BTreeMap).
    pub fn projects(&self) -> impl Iterator<Item = &Project> {
        self.project_map
            .values()
            .flatten()
            .map(move |id| &self.projects[id.0])
            .filter(|p| p.is_top_level)
    }

    /// Look up projects by name.
    pub fn projects_by_name(&self, name: &str) -> impl Iterator<Item = &Project> {
        static EMPTY: Vec<ProjectId> = Vec::new();
        self.project_map
            .get(name)
            .unwrap_or(&EMPTY)
            .iter()
            .map(move |id| &self.projects[id.0])
            .filter(|p| p.is_top_level)
    }

    /// Get a remote by name.
    pub fn remote(&self, name: &str) -> Option<&Remote> {
        self.remotes.get(name)
    }

    /// Every remote, name-sorted to match serialization order.
    pub fn remotes(&self) -> impl Iterator<Item = &Remote> {
        let mut remotes: Vec<&Remote> = self.remotes.values().collect();
        remotes.sort_by(|a, b| a.name.cmp(&b.name));
        remotes.into_iter()
    }

    /// Get the defaults.
    pub fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    /// Get the notice text, if any.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Get the manifest server URL, if any.
    pub fn manifest_server(&self) -> Option<&str> {
        self.manifest_server.as_deref()
    }

    /// Get the superproject configuration, if any.
    pub fn superproject(&self) -> Option<&types::Superproject> {
        self.superproject.as_ref()
    }

    /// Get the contact info, if any.
    pub fn contactinfo(&self) -> Option<&types::Contactinfo> {
        self.contactinfo.as_ref()
    }

    /// Get the repo hooks configuration, if any.
    pub fn repo_hooks(&self) -> Option<&types::RepoHooks> {
        self.repo_hooks.as_ref()
    }

    /// Get the submanifests, if any.
    pub fn submanifests(&self) -> &[types::Submanifest] {
        &self.submanifests
    }

    /// Total number of top-level projects.
    pub fn project_count(&self) -> usize {
        self.projects().count()
    }

    /// Check that this manifest would be accepted by git-repo.
    ///
    /// git-repo rejects manifests with duplicate project paths at parse time.
    /// Since `repo_manifest` parses permissively (so validators can report
    /// all issues), consumers that need repo-compatible strictness should
    /// call this method after parsing.
    pub fn ensure_repo_valid(&self) -> Result<()> {
        let mut seen_paths = HashSet::new();
        for project in self.projects() {
            if let Some(path_str) = project.path.to_str() {
                if !seen_paths.insert(path_str) {
                    anyhow::bail!("Duplicate project path: {path_str}");
                }
            }
        }
        Ok(())
    }
}

/// Serialize a symbolic manifest (git-repo XML) into its static form.
///
/// Parses `symbolic_xml` in strict mode, then re-serializes it with symbolic
/// references resolved into concrete values.
pub fn to_static(
    symbolic_xml: &[u8],
    open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    resolve_fn: impl Fn(&Project) -> Result<String>,
) -> Result<String> {
    to_static_with_options(symbolic_xml, open_fn, resolve_fn, &StaticOptions::default())
}

/// [`to_static`], with control over what the serializer emits.
pub fn to_static_with_options(
    symbolic_xml: &[u8],
    open_fn: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    resolve_fn: impl Fn(&Project) -> Result<String>,
    options: &StaticOptions,
) -> Result<String> {
    let m = Manifest::from_xml_with_options(
        symbolic_xml,
        open_fn,
        &ParseOptions { deny_unknown: true },
    )?;
    serialize::serialize_static(&m, resolve_fn, options)
}

/// Re-serialize an already-pinned static manifest without changing any pin.
pub fn canonicalize_static(xml: &[u8], options: &ParseOptions) -> Result<String> {
    let mut open_fn = |name: &str| -> Result<Vec<u8>> {
        anyhow::bail!("static manifest includes {name}; expected a flat document")
    };
    let m = Manifest::from_xml_with_options(xml, &mut open_fn, options)?;
    serialize::serialize_static(
        &m,
        |p: &Project| {
            anyhow::bail!(
                "static manifest project {} is not pinned (revision {})",
                p.name,
                p.revision
            )
        },
        &StaticOptions::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_includes(name: &str) -> Result<Vec<u8>> {
        anyhow::bail!("Unexpected include: {name}")
    }

    #[test]
    fn test_parse_and_resolve_basic() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <project name="bar" path="bar" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 2);
    }

    #[test]
    fn test_include_processing() {
        let included_xml = br#"
            <manifest>
                <project name="included-project" path="included" />
            </manifest>
        "#;

        let root_xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="root-project" path="root" />
                <include name="extra.xml" />
            </manifest>
        "#;

        let manifest = Manifest::from_xml(root_xml.as_bytes(), &mut |name| {
            assert_eq!(name, "extra.xml");
            Ok(included_xml.to_vec())
        })
        .unwrap();

        assert_eq!(manifest.project_count(), 2);
        let names: Vec<_> = manifest.projects().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"root-project"));
        assert!(names.contains(&"included-project"));
    }

    #[test]
    fn test_circular_include_detected() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="self.xml" />
            </manifest>
        "#;

        let result = Manifest::from_xml(xml.as_bytes(), &mut |_name| {
            // Return XML that includes itself again
            Ok(br#"<manifest><include name="self.xml" /></manifest>"#.to_vec())
        });

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Circular"));
    }

    #[test]
    fn test_include_groups_propagated() {
        let included_xml = br#"
            <manifest>
                <project name="inc-project" path="inc" groups="own-group" />
            </manifest>
        "#;

        let root_xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="extra.xml" groups="from-include" />
            </manifest>
        "#;

        let manifest =
            Manifest::from_xml(root_xml.as_bytes(), &mut |_| Ok(included_xml.to_vec())).unwrap();

        let project = manifest.projects().next().unwrap();
        assert!(project.groups.contains("own-group"));
        assert!(project.groups.contains("from-include"));
    }

    #[test]
    fn test_notice_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <notice>Important notice text</notice>
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.notice(), Some("Important notice text"));
    }

    #[test]
    fn test_manifest_server_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <manifest-server url="https://smart-sync.example.com/" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(
            manifest.manifest_server(),
            Some("https://smart-sync.example.com/")
        );
    }

    #[test]
    fn test_projects_by_name() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <project name="bar" path="bar" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();

        let foo: Vec<_> = manifest.projects_by_name("foo").collect();
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].name, "foo");

        let missing: Vec<_> = manifest.projects_by_name("nonexistent").collect();
        assert!(missing.is_empty());
    }

    // =========================================================================
    // Tests ported from git-repo's test_manifest_xml.py
    // =========================================================================

    // --- ManifestValidateFilePaths ---

    /// Ported from test_manifest_xml.py: INVALID_FS_PATHS + test_bad_paths
    #[test]
    fn test_invalid_fs_paths_rejected() {
        let invalid_paths = [
            "..",
            "../foo",
            "foo/./bar",
            "foo/../bar",
            "/absolute",
            ".repo",
            ".repo/foo",
        ];
        for path in &invalid_paths {
            assert!(
                validation::validate_project_path(path).is_err(),
                "Expected rejection of path: {path:?}"
            );
        }
    }

    /// Ported from test_manifest_xml.py: test_normal_path
    #[test]
    fn test_valid_fs_paths_accepted() {
        let valid_paths = ["foo", "foo/bar", "a/b/c/d", "platform/art"];
        for path in &valid_paths {
            assert!(
                validation::validate_project_path(path).is_ok(),
                "Expected acceptance of path: {path:?}"
            );
        }
    }

    /// R3-010: Verify .GIT project names are rejected at parse time
    #[test]
    fn test_dotgit_project_name_rejected() {
        let cases = [".GIT", ".git", ".Git", "foo/.git"];
        for name in &cases {
            let xml = format!(
                r#"<manifest>
                    <remote name="origin" fetch="https://example.com" />
                    <default remote="origin" revision="main" />
                    <project name="{name}" path="xgit" />
                </manifest>"#
            );
            let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
            assert!(
                result.is_err(),
                "Expected rejection of project name {name:?}, but got Ok"
            );
        }
    }

    /// R3-010: Verify .repo project names are rejected at parse time
    #[test]
    fn test_dotrepo_project_name_rejected() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com" />
            <default remote="origin" revision="main" />
            <project name=".repo" path="xrepo" />
        </manifest>"#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
        assert!(
            result.is_err(),
            "Expected rejection of project name '.repo'"
        );
    }

    // --- XmlManifestTests ---

    /// Ported from test_manifest_xml.py: test_empty
    #[test]
    fn test_empty_manifest() {
        let xml = r#"<manifest></manifest>"#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 0);
        assert!(manifest.remotes.is_empty());
    }

    /// Ported from test_manifest_xml.py: test_unknown_tags
    #[test]
    fn test_unknown_tags_ignored() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <x-custom-tag foo="bar" />
                <some-future-element baz="quux" />
                <project name="test" path="test" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 1);
    }

    /// Ported from test_manifest_xml.py: test_repo_hooks
    #[test]
    fn test_repo_hooks_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="hooks-project" path="hooks" />
                <repo-hooks in-project="hooks-project" enabled-list="pre-upload, commit-msg" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let hooks = manifest.repo_hooks().unwrap();
        assert_eq!(hooks.in_project, "hooks-project");
        assert_eq!(hooks.enabled_list, "pre-upload, commit-msg");
    }

    /// Ported from test_manifest_xml.py: test_contactinfo
    #[test]
    fn test_contactinfo_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <contactinfo bugurl="https://bugs.example.com/repo" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let ci = manifest.contactinfo().unwrap();
        assert_eq!(ci.bugurl, "https://bugs.example.com/repo");
    }

    /// Ported from test_manifest_xml.py: test_superproject
    #[test]
    fn test_superproject_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <superproject name="super" remote="origin" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let sp = manifest.superproject().unwrap();
        assert_eq!(sp.name, "super");
        assert_eq!(sp.remote.as_deref(), Some("origin"));
    }

    /// A `<superproject revision="...">` round-trips into the resolved model.
    #[test]
    fn test_superproject_revision_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <superproject name="s" remote="origin"
                              revision="deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let sp = manifest.superproject().unwrap();
        assert_eq!(sp.name, "s");
        assert_eq!(sp.remote.as_deref(), Some("origin"));
        assert_eq!(
            sp.revision.as_deref(),
            Some("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
        );
    }

    /// Ported from test_manifest_xml.py: test_parse_with_xml_doctype
    #[test]
    fn test_xml_with_doctype_declaration() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
            <!DOCTYPE manifest []>
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
            </manifest>
        "#;
        // Should parse without error (doctype is ignored)
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 0);
    }

    // --- IncludeElementTests ---

    /// Ported from test_manifest_xml.py: test_revision_default
    /// Include revision inheritance through nested includes
    #[test]
    fn test_include_nested_with_remotes_and_revision_inheritance() {
        // level-b defines remote beta and 2 projects
        let level_b = br#"
            <manifest>
                <remote name="beta" fetch="ssh://beta.example.com" />
                <project name="bravo1" path="b1" />
                <project name="bravo2" path="b2" remote="beta" />
            </manifest>
        "#;

        // level-a defines remote alpha, includes level-b, defines 2 projects
        let level_a = br#"
            <manifest>
                <remote name="alpha" fetch="ssh://alpha.example.com" />
                <include name="level-b.xml" />
                <project name="alfa1" path="a1" />
                <project name="alfa2" path="a2" remote="alpha" />
            </manifest>
        "#;

        // root defines remote origin + default, includes level-a, defines projects
        let root = r#"
            <manifest>
                <remote name="origin" fetch="ssh://origin.example.com" />
                <include name="level-a.xml" />
                <default remote="origin" revision="main" />
                <project name="root1" path="r1" />
            </manifest>
        "#;

        let manifest = Manifest::from_xml(root.as_bytes(), &mut |name| match name {
            "level-a.xml" => Ok(level_a.to_vec()),
            "level-b.xml" => Ok(level_b.to_vec()),
            other => anyhow::bail!("unexpected include: {other}"),
        })
        .unwrap();

        assert_eq!(manifest.project_count(), 5);

        // All projects should have revision "main" (from default)
        // except those whose remote has its own revision
        for project in manifest.projects() {
            assert_eq!(
                project.revision, "main",
                "Project {} should have revision 'main', got '{}'",
                project.name, project.revision
            );
        }
    }

    /// Ported from test_manifest_xml.py: test_group_levels
    /// Include groups accumulate across nesting levels
    #[test]
    fn test_include_group_accumulation_across_levels() {
        let inner = br#"
            <manifest>
                <project name="inner-proj" path="inner" groups="inner-group" />
            </manifest>
        "#;

        let mid = br#"
            <manifest>
                <include name="inner.xml" groups="mid-group" />
                <project name="mid-proj" path="mid" groups="mid-own" />
            </manifest>
        "#;

        let root = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="mid.xml" groups="root-group" />
                <project name="root-proj" path="root" />
            </manifest>
        "#;

        let manifest = Manifest::from_xml(root.as_bytes(), &mut |name| match name {
            "mid.xml" => Ok(mid.to_vec()),
            "inner.xml" => Ok(inner.to_vec()),
            other => anyhow::bail!("unexpected: {other}"),
        })
        .unwrap();

        // root-proj: no include groups (but has automatic groups)
        let root_proj = manifest.projects().find(|p| p.name == "root-proj").unwrap();
        assert!(!root_proj.groups.contains("root-group"));
        assert!(!root_proj.groups.contains("mid-group"));

        // mid-proj: gets "root-group" from the include tag
        let mid_proj = manifest.projects().find(|p| p.name == "mid-proj").unwrap();
        assert!(mid_proj.groups.contains("mid-own"));
        assert!(mid_proj.groups.contains("root-group"));

        // inner-proj: gets "mid-group" + "root-group" (accumulated) + its own "inner-group"
        let inner_proj = manifest
            .projects()
            .find(|p| p.name == "inner-proj")
            .unwrap();
        assert!(inner_proj.groups.contains("inner-group"));
        assert!(inner_proj.groups.contains("mid-group"));
        assert!(inner_proj.groups.contains("root-group"));
    }

    // --- RemoveProjectElementTests ---

    /// Ported from test_manifest_xml.py: test_remove_one_project
    #[test]
    fn test_remove_project_basic() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="to-remove" path="to-remove" />
                <remove-project name="to-remove" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 0);
    }

    /// Ported from test_manifest_xml.py: test_remove_one_project_one_remains
    #[test]
    fn test_remove_project_one_remains() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="keep" path="keep" />
                <project name="remove" path="remove" />
                <remove-project name="remove" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.project_count(), 1);
        assert_eq!(manifest.projects().next().unwrap().name, "keep");
    }

    /// Ported from test_manifest_xml.py: test_remove_one_project_doesnt_exist
    #[test]
    fn test_remove_project_nonexistent_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <remove-project name="nonexistent" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("undefined"));
    }

    // --- ExtendProjectElementTests ---

    /// Ported from test_manifest_xml.py: test_extend_project_dest_branch
    #[test]
    fn test_extend_project_dest_branch() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" dest-branch="default-branch" />
                <project name="foo" path="foo" />
                <extend-project name="foo" revision="v2" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let project = manifest.projects().next().unwrap();
        assert_eq!(project.revision, "v2");
        assert_eq!(project.dest_branch.as_deref(), Some("default-branch"));
    }

    /// Ported from test_manifest_xml.py: test_extend_project_dest_path_multi_match_path_specified
    #[test]
    fn test_extend_project_with_path_disambiguation() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="multi" path="multi/a" />
                <project name="multi" path="multi/b" />
                <extend-project name="multi" path="multi/a" revision="override-a" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let projects: Vec<_> = manifest.projects_by_name("multi").collect();
        assert_eq!(projects.len(), 2);

        let a = projects
            .iter()
            .find(|p| p.path.to_str() == Some("multi/a"))
            .unwrap();
        assert_eq!(a.revision, "override-a");

        let b = projects
            .iter()
            .find(|p| p.path.to_str() == Some("multi/b"))
            .unwrap();
        assert_eq!(b.revision, "main"); // unchanged
    }

    // --- ProjectElementTests ---

    /// Ported from test_manifest_xml.py: test_bad_path_name_checks
    #[test]
    fn test_project_bad_path_rejected() {
        let bad_paths = ["../escape", "/absolute", ".repo/secret"];
        for bad_path in &bad_paths {
            let xml = format!(
                r#"
                <manifest>
                    <remote name="origin" fetch="https://example.com" />
                    <default remote="origin" revision="main" />
                    <project name="test" path="{bad_path}" />
                </manifest>
                "#
            );
            let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
            assert!(
                result.is_err(),
                "Expected error for project path {bad_path:?}"
            );
        }
    }

    /// Ported from test_manifest_xml.py: test_group
    /// Project groups should include user-specified groups
    #[test]
    fn test_project_groups_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="test" path="test" groups="pdk,tools" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let project = manifest.projects().next().unwrap();
        assert!(project.groups.contains("pdk"));
        assert!(project.groups.contains("tools"));
        assert!(!project.groups.contains("nonexistent"));
    }

    // --- AOSP-style manifest tests ---

    /// Test parsing an AOSP-style manifest with nested includes.
    #[test]
    fn test_parse_real_include_chain() {
        let root = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos"
    pushurl="ssh://git.example.com/data/gitrepos"/>
  <include name="include-a.xml" />
  <default remote="origin" revision="msm8998-nougat" sync-j="12"/>
  <project name="aosp/platform/art" path="art"/>
  <project name="aosp/platform/bionic" path="bionic"/>
  <project name="aosp/platform/build" path="build">
    <copyfile dest="Makefile" src="core/root.mk"/>
  </project>
  <project name="aosp/platform/dalvik" path="dalvik"/>
  <project name="aosp/platform/frameworks/base" path="frameworks/base"/>
  <project name="aosp/platform/libcore" path="libcore"/>
  <project name="aosp/platform/ndk" path="ndk"/>
  <project name="aosp/platform/pdk" path="pdk"/>
  <project name="example/aosp/vendor/example" path="vendor/example">
    <linkfile dest=".arcconfig" src="arcanist/arcconfig"/>
    <linkfile dest=".jfconfig" src="jfconfig"/>
    <linkfile dest=".flake8" src=".flake8"/>
    <linkfile dest=".watchmanconfig" src="watchmanconfig"/>
    <annotation name="git_lfs" value="true"/>
    <annotation name="lfs_url" value="https://lfs-server.example.com/lfs"/>
    <annotation name="prebuilt_manifest" value="prebuilt_manifest.json"/>
  </project>
</manifest>"#;

        let include_a = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="alpha" fetch="ssh://alpha.example.com/data/gitrepos"
    pushurl="ssh://alpha.example.com/data/gitrepos"/>
  <include name="include-b.xml" />
  <project name="alfa1" path="a1"/>
  <project name="alfa2" path="a2" remote="alpha" />
</manifest>"#;

        let include_b = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="beta" fetch="ssh://beta.example.com/data/gitrepos"
    pushurl="ssh://beta.example.com/data/gitrepos"/>
  <project name="bravo1" path="b1"/>
  <project name="bravo2" path="b2" remote="beta" />
</manifest>"#;

        let manifest = Manifest::from_xml(root, &mut |name| match name {
            "include-a.xml" => Ok(include_a.to_vec()),
            "include-b.xml" => Ok(include_b.to_vec()),
            other => anyhow::bail!("unexpected include: {other}"),
        })
        .unwrap();

        // Root has 9 projects, include-a has 2, include-b has 2 = 13 total
        assert_eq!(manifest.project_count(), 13);

        // Check specific projects
        let art: Vec<_> = manifest.projects_by_name("aosp/platform/art").collect();
        assert_eq!(art.len(), 1);
        assert_eq!(art[0].path.to_str().unwrap(), "art");
        assert_eq!(art[0].remote_name, "origin");
        assert_eq!(art[0].revision, "msm8998-nougat");

        // Check included project with explicit remote
        let alfa2: Vec<_> = manifest.projects_by_name("alfa2").collect();
        assert_eq!(alfa2.len(), 1);
        assert_eq!(alfa2[0].remote_name, "alpha");

        // Check deeply included project with explicit remote
        let bravo2: Vec<_> = manifest.projects_by_name("bravo2").collect();
        assert_eq!(bravo2.len(), 1);
        assert_eq!(bravo2[0].remote_name, "beta");

        // Check project with copyfile
        let build: Vec<_> = manifest.projects_by_name("aosp/platform/build").collect();
        assert_eq!(build[0].copy_files.len(), 1);
        assert_eq!(build[0].copy_files[0].dest, "Makefile");

        // Check project with linkfiles and annotations
        let vendor: Vec<_> = manifest
            .projects_by_name("example/aosp/vendor/example")
            .collect();
        assert_eq!(vendor[0].link_files.len(), 4);
        assert_eq!(vendor[0].annotations.len(), 3);
    }

    /// Test parsing an AOSP-style manifest with two remotes and projects that share a name.
    #[test]
    fn test_parse_real_diff_manifest() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <manifest-server url="http://before.server.example.com/" />
  <remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos"
    pushurl="ssh://git.example.com/data/gitrepos"/>
  <remote name="remote2" fetch="ssh://git2.example.com/data/gitrepos" />
  <default remote="origin" revision="msm8998-nougat" sync-j="12"/>
  <project name="test/before-only" path="test/before-only" />
  <project name="aosp/manifest" path="manifest"/>
  <project name="aosp/platform/art" path="art"/>
  <project name="aosp/platform/bionic" path="bionic"/>
  <project name="aosp/platform/build" path="build">
    <copyfile dest="Makefile" src="core/root.mk"/>
  </project>
  <project name="aosp/platform/dalvik" path="dalvik"/>
  <project name="aosp/platform/external/bison" path="external/bison"/>
  <project name="aosp/platform/external/bzip2" path="external/bzip2"/>
  <project name="aosp/platform/external/curl" path="external/curl"/>
  <project name="aosp/platform/external/elfutils" path="external/elfutils"/>
  <project name="aosp/platform/external/gflags" path="external/gflags" revision="release-10.0"/>
  <project name="aosp/platform/external/gmock" path="external/gmock"/>
  <project name="aosp/platform/external/gtest" path="external/gtest"/>
  <project name="aosp/platform/frameworks/base" path="frameworks/base"/>
  <project name="aosp/platform/pdk" path="pdk"/>
  <project name="aosp/platform/sdk" path="sdk"/>
  <project name="aosp/platform/system/core" path="system/core"/>
  <project name="aosp/vendor/ci" path="vendor/ci" revision="master">
    <annotation name="branch_agnostic" value="true"/>
  </project>
  <project name="example/aosp/device/example" path="device/example">
    <linkfile dest="hardware/qcom/msm8998/Android.mk" src="build/qcom_hal_guard.mk" />
    <linkfile dest="vendor/qcom/msm8998/Android.mk" src="build/qcom_hal_guard.mk" />
  </project>
  <project name="example/aosp/vendor/example" path="vendor/example">
    <linkfile dest=".arcconfig" src="arcanist/arcconfig"/>
    <linkfile dest=".jfconfig" src="jfconfig"/>
    <linkfile dest=".flake8" src=".flake8"/>
    <linkfile dest=".watchmanconfig" src="watchmanconfig"/>
    <annotation name="git_lfs" value="true"/>
    <annotation name="lfs_url" value="https://lfs-server.example.com/lfs"/>
    <annotation name="prebuilt_manifest" value="prebuilt_manifest.json"/>
  </project>
  <project name="test/path-mover" path="test/path-before" />
  <project name="test/group-change" path="test/group-change" groups="removed,remained"/>
  <project name="test/multi-project-change" path="test/multi-project/nougat" />
  <project name="test/multi-project-change" path="test/multi-project/alfa" revision="alpha" />
  <project name="test/multi-project-change" path="test/multi-project/bravo" revision="beta" groups="" />
  <project name="test/multi-project-change" path="test/multi-project/charlie" revision="charlie" groups="three,tag" />
  <project name="test/multi-project-change" path="test/multi-project/delta" revision="delta" />
  <project name="test/multi-project-change" path="test/multi-project/echo" remote="remote2" />
  <project name="test/multi-project-change" path="test/multi-project/foxtrot" revision="F" />
  <project name="test/multi-project-change" path="test/multi-project/golf" revision="gamma" groups="" />
</manifest>"#;

        let manifest = Manifest::from_xml(xml.as_ref(), &mut no_includes).unwrap();

        // Check manifest server
        assert_eq!(
            manifest.manifest_server(),
            Some("http://before.server.example.com/")
        );

        // Multiple projects with same name at different paths
        let multi: Vec<_> = manifest
            .projects_by_name("test/multi-project-change")
            .collect();
        assert_eq!(multi.len(), 8);

        // Check project with per-project revision override
        let gflags: Vec<_> = manifest
            .projects_by_name("aosp/platform/external/gflags")
            .collect();
        assert_eq!(gflags[0].revision, "release-10.0");

        // Check project with non-default remote
        let echo = multi
            .iter()
            .find(|p| p.path.to_str() == Some("test/multi-project/echo"))
            .unwrap();
        assert_eq!(echo.remote_name, "remote2");

        // Check project with groups
        let group_change: Vec<_> = manifest.projects_by_name("test/group-change").collect();
        assert!(group_change[0].groups.contains("removed"));
        assert!(group_change[0].groups.contains("remained"));

        // Check project with annotation
        let ci: Vec<_> = manifest.projects_by_name("aosp/vendor/ci").collect();
        assert_eq!(ci[0].annotations.len(), 1);
        assert_eq!(ci[0].annotations[0].name, "branch_agnostic");
        assert_eq!(ci[0].annotations[0].value, "true");
    }

    /// Ported from test_manifest_xml.py: test_no_remote_no_default_errors
    #[test]
    fn test_no_remote_no_default_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <project name="test" path="test" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
        assert!(result.is_err(), "Should fail without default remote");
    }

    /// Ported from test_manifest_xml.py: test_no_revision_errors
    #[test]
    fn test_no_revision_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" />
                <project name="test" path="test" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
        assert!(
            result.is_err(),
            "Should fail without any revision specified"
        );
    }

    /// Test nested (sub)projects
    #[test]
    fn test_nested_subproject() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="parent" path="parent">
                    <project name="child" path="child" />
                </project>
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();

        // Only the parent should be top-level
        let top_level: Vec<_> = manifest.projects().collect();
        assert_eq!(top_level.len(), 1);
        assert_eq!(top_level[0].name, "parent");
        assert_eq!(top_level[0].subprojects.len(), 1);
    }

    /// Test that multiple remotes work correctly
    #[test]
    fn test_multiple_remotes() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" revision="main" />
                <remote name="upstream" fetch="https://upstream.com" revision="dev" />
                <default remote="origin" revision="main" />
                <project name="from-origin" path="a" />
                <project name="from-upstream" path="b" remote="upstream" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let projects: Vec<_> = manifest.projects().collect();

        assert_eq!(projects[0].remote_name, "origin");
        assert_eq!(projects[1].remote_name, "upstream");
        // "from-upstream" should get revision from remote "upstream"
        assert_eq!(projects[1].revision, "dev");
    }

    // --- MR-001: Include revision inheritance ---

    #[test]
    fn test_include_revision_inheritance() {
        let included_xml = br#"
            <manifest>
                <project name="inc-project" path="inc" />
            </manifest>
        "#;

        let root_xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="extra.xml" revision="v2" />
                <project name="root-project" path="root" />
            </manifest>
        "#;

        let manifest =
            Manifest::from_xml(root_xml.as_bytes(), &mut |_| Ok(included_xml.to_vec())).unwrap();

        // Root project should use default revision
        let root_proj = manifest
            .projects()
            .find(|p| p.name == "root-project")
            .unwrap();
        assert_eq!(root_proj.revision, "main");

        // Included project should inherit revision from include tag
        let inc_proj = manifest
            .projects()
            .find(|p| p.name == "inc-project")
            .unwrap();
        assert_eq!(inc_proj.revision, "v2");
    }

    /// git-repo applies `<include revision=…>` to the top-level `<project>` elements of that
    /// included file only (`_ParseManifestXml`: `parent_node.getAttribute("revision")` on the
    /// include element of the file being parsed). It does not reach the projects of a nested
    /// `<include>` that has no `revision` of its own, nor nested `<project>` children; those fall
    /// back to the remote/default revision.
    #[test]
    fn test_include_revision_scope_matches_git_repo() {
        let level_a = br#"
            <manifest>
                <project name="a-proj" path="a">
                    <project name="a-sub" path="sub" />
                </project>
                <include name="level-b.xml" />
            </manifest>
        "#;
        let level_b = br#"
            <manifest>
                <project name="b-proj" path="b" />
            </manifest>
        "#;
        let root = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="level-a.xml" revision="increv" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(root.as_bytes(), &mut |name| match name {
            "level-a.xml" => Ok(level_a.to_vec()),
            "level-b.xml" => Ok(level_b.to_vec()),
            other => anyhow::bail!("unexpected include: {other}"),
        })
        .unwrap();

        let a_proj = manifest.projects_by_name("a-proj").next().unwrap();
        assert_eq!(
            a_proj.revision, "increv",
            "top-level project of the included file"
        );

        // A nested project is parsed from its own attributes, like git-repo does.
        let a_sub = &manifest.projects[a_proj.subprojects[0].0];
        assert_eq!(a_sub.revision, "main");

        // The nested include has no `revision` attribute, so the outer one does not apply.
        let b_proj = manifest.projects_by_name("b-proj").next().unwrap();
        assert_eq!(b_proj.revision, "main");
    }

    // --- MR-006: Include groups propagate to extend-project ---

    #[test]
    fn test_include_groups_propagate_to_extend_project() {
        let included_xml = br#"
            <manifest>
                <extend-project name="foo" groups="from-extend" revision="v2" />
            </manifest>
        "#;

        let root_xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <include name="extra.xml" groups="inc-group" />
            </manifest>
        "#;

        let manifest =
            Manifest::from_xml(root_xml.as_bytes(), &mut |_| Ok(included_xml.to_vec())).unwrap();

        let project = manifest.projects().next().unwrap();
        assert!(project.groups.contains("from-extend"));
        assert!(project.groups.contains("inc-group"));
        assert_eq!(project.revision, "v2");
    }

    // --- MR-010: Submanifest ---

    #[test]
    fn test_submanifest_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <submanifest name="sub1" remote="origin" path="sub" groups="g1,g2" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert_eq!(manifest.submanifests().len(), 1);
        assert_eq!(manifest.submanifests()[0].name, "sub1");
        assert_eq!(manifest.submanifests()[0].path.as_deref(), Some("sub"));
        assert!(manifest.submanifests()[0].groups.contains("g1"));
        assert!(manifest.submanifests()[0].groups.contains("g2"));
    }

    /// A `<submanifest>` keeps its `<annotation>` children in the resolved model.
    #[test]
    fn test_submanifest_annotations_parsed() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <submanifest name="s" remote="origin">
                    <annotation name="k" value="v" />
                </submanifest>
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        let sm = &manifest.submanifests()[0];
        assert_eq!(sm.annotations.len(), 1);
        assert_eq!(sm.annotations[0].name, "k");
        assert_eq!(sm.annotations[0].value, "v");
        assert!(sm.annotations[0].keep); // @keep omitted, defaults to true
    }

    /// A strict parse rejects an unknown element nested inside a `<submanifest>`,
    /// matching the strict-parse contract that `<remote>`/`<project>` already
    /// enforce for their unknown children.
    #[test]
    fn test_strict_rejects_unknown_submanifest_child_element() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <submanifest name="s" remote="origin">
                    <x-bogus />
                </submanifest>
            </manifest>
        "#;
        let options = ParseOptions { deny_unknown: true };
        let result = Manifest::from_xml_with_options(xml.as_bytes(), &mut no_includes, &options);
        assert!(
            result.is_err(),
            "strict mode should reject unknown submanifest child"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains("unknown") && err.contains("submanifest"),
            "error should name the unknown element and the submanifest: {err}"
        );
    }

    /// The permissive default ignores an unknown `<submanifest>` child: parsing
    /// succeeds and the unknown element is dropped (annotations stay empty).
    #[test]
    fn test_permissive_ignores_unknown_submanifest_child_element() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <submanifest name="s" remote="origin">
                    <x-bogus />
                </submanifest>
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut no_includes).unwrap();
        assert!(manifest.submanifests()[0].annotations.is_empty());
    }

    // --- MR-015: Include path traversal rejected ---

    #[test]
    fn test_include_path_traversal_rejected() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <include name="../secret.xml" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut no_includes);
        assert!(result.is_err());
    }

    // --- G1.1: Strict parse mode rejects unknown elements ---

    /// A strict parse rejects an unknown top-level element, while the permissive
    /// default still ignores it.
    #[test]
    fn test_strict_rejects_unknown_top_level_element() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <x-future-element foo="bar" />
                <project name="foo" path="foo" />
            </manifest>
        "#;
        // Permissive default: unknown element ignored.
        assert!(Manifest::from_xml(xml.as_bytes(), &mut no_includes).is_ok());

        // Strict: rejected with an "unknown" error.
        let options = ParseOptions { deny_unknown: true };
        let result = Manifest::from_xml_with_options(xml.as_bytes(), &mut no_includes, &options);
        assert!(result.is_err(), "strict mode should reject unknown element");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("unknown"),
            "error should mention 'unknown'"
        );
    }

    /// A strict parse rejects an unknown element nested inside a `<project>`.
    #[test]
    fn test_strict_rejects_unknown_project_child_element() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo">
                    <x-child />
                </project>
            </manifest>
        "#;
        // Permissive default: unknown child ignored.
        assert!(Manifest::from_xml(xml.as_bytes(), &mut no_includes).is_ok());

        // Strict: rejected.
        let options = ParseOptions { deny_unknown: true };
        let result = Manifest::from_xml_with_options(xml.as_bytes(), &mut no_includes, &options);
        assert!(
            result.is_err(),
            "strict mode should reject unknown project child"
        );
    }

    // --- G1.2: Strict parse mode rejects unknown attributes ---

    /// A strict parse rejects an unknown attribute on a `<project>`, while the
    /// permissive default still ignores it.
    #[test]
    fn test_strict_rejects_unknown_attribute() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="p" path="p" x-custom="v" />
            </manifest>
        "#;
        // Permissive default: unknown attribute ignored.
        assert!(Manifest::from_xml(xml.as_bytes(), &mut no_includes).is_ok());

        // Strict: rejected.
        let options = ParseOptions { deny_unknown: true };
        let result = Manifest::from_xml_with_options(xml.as_bytes(), &mut no_includes, &options);
        assert!(
            result.is_err(),
            "strict mode should reject unknown attribute"
        );
    }

    /// Real AOSP `<remote>` elements carry a legacy `push` attr duplicating `pushurl`.
    /// `to_static` must accept it under strict parse and drop it on serialize (git-repo
    /// reads/emits only `pushurl`), so the pinned static manifest stays spec-conformant.
    #[test]
    fn test_to_static_accepts_and_drops_remote_push() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com"
                    push="https://push.example.com" pushurl="https://push.example.com" />
            <default remote="origin" revision="main" />
            <project name="foo" path="foo" />
        </manifest>"#;
        let out = to_static(xml, &mut no_includes, |_p| Ok("a".repeat(40)))
            .expect("to_static should accept the legacy remote@push attribute");
        // pushurl is emitted; push is dropped -> the shared URL value appears exactly once.
        assert!(out.contains("pushurl="), "pushurl must be emitted: {out}");
        assert_eq!(
            out.matches("\"https://push.example.com\"").count(),
            1,
            "push must be dropped (git-repo emits only pushurl), so its value appears once: {out}"
        );
    }

    /// `push` is accept-and-ignored ONLY because it is a redundant duplicate of
    /// `pushurl`. If it ever DIVERGES, silently dropping it would lose information, so
    /// the crate fails loud rather than drop a divergent `push`. (Never happens on real
    /// manifests — push always == pushurl — but guards the losslessness assumption.)
    #[test]
    fn test_to_static_rejects_divergent_remote_push() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com"
                    push="https://divergent.example.com" pushurl="https://push.example.com" />
            <default remote="origin" revision="main" />
            <project name="foo" path="foo" />
        </manifest>"#;
        let result = to_static(xml, &mut no_includes, |_p| Ok("a".repeat(40)));
        assert!(
            result.is_err(),
            "to_static must reject a remote whose push diverges from pushurl"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("push") && err.contains("pushurl"),
            "error should name push and pushurl: {err}"
        );
    }

    /// End-to-end: a realistic manifest parses cleanly under strict mode, i.e.
    /// the strict attribute pass does not falsely reject known attributes when
    /// run through the full pipeline.
    #[test]
    fn test_strict_accepts_realistic_manifest() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos"
    pushurl="ssh://git.example.com/data/gitrepos"/>
  <default remote="origin" revision="main" sync-j="12"/>
  <project name="platform/art" path="art"/>
  <project name="platform/build" path="build">
    <copyfile dest="Makefile" src="core/root.mk"/>
    <linkfile dest=".arcconfig" src="arcanist/arcconfig"/>
    <annotation name="git_lfs" value="true"/>
  </project>
</manifest>"#;
        let options = ParseOptions { deny_unknown: true };
        let manifest = Manifest::from_xml_with_options(xml, &mut no_includes, &options).unwrap();
        assert_eq!(manifest.project_count(), 2);
    }
}
