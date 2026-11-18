/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Raw XML deserialization types for git-repo manifest format.
//!
//! These types map directly to the XML schema using serde + quick-xml.
//! They are intermediate representations — the resolver transforms them
//! into the fully resolved types in `types.rs`.
//!
//! Reference: https://gerrit.googlesource.com/git-repo/+/HEAD/docs/manifest-format.md

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

/// Consume and discard all remaining child elements/text in the current XML node.
fn ignore_all<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<serde::de::IgnoredAny>::deserialize(deserializer)?;
    Ok(())
}

fn ret_true() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_default<T: Default + Eq>(v: &T) -> bool {
    v == &T::default()
}

// === Top-level elements ===

/// Child elements that can appear inside `<remote>`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteChild {
    Annotation(Annotation),

    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Remote {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@alias")]
    pub alias: Option<String>,
    #[serde(rename = "@fetch")]
    pub fetch: String,
    #[serde(rename = "@pushurl")]
    pub pushurl: Option<String>,
    // Legacy Meta duplicate of `pushurl` (git-repo neither reads nor emits it). Modelled
    // only so the resolver can reject a divergent value; never copied to `types` or
    // serialized. See `resolver::resolve_manifest`.
    #[serde(rename = "@push")]
    pub push: Option<String>,
    #[serde(rename = "@review")]
    pub review: Option<String>,
    #[serde(rename = "@revision")]
    pub revision: Option<String>,

    // MR-012: Parse remote annotations instead of ignoring children
    #[serde(default, rename = "$value")]
    pub children: Vec<RemoteChildWrapper>,
}

/// Wrapper to handle Remote children deserialization while keeping Eq/PartialEq on Remote.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteChildWrapper {
    Annotation(Annotation),

    #[serde(other)]
    Unknown,
}

impl PartialEq for RemoteChildWrapper {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RemoteChildWrapper::Annotation(a), RemoteChildWrapper::Annotation(b)) => {
                a.name == b.name && a.value == b.value && a.keep == b.keep
            }
            (RemoteChildWrapper::Unknown, RemoteChildWrapper::Unknown) => true,
            _ => false,
        }
    }
}

impl Eq for RemoteChildWrapper {}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct Defaults {
    #[serde(rename = "@remote", skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(rename = "@revision", skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(rename = "@dest-branch", skip_serializing_if = "Option::is_none")]
    pub dest_branch: Option<String>,
    #[serde(rename = "@upstream", skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(rename = "@sync-j", skip_serializing_if = "Option::is_none")]
    pub sync_j: Option<u32>,
    #[serde(rename = "@sync-c", skip_serializing_if = "Option::is_none")]
    pub sync_c: Option<bool>,
    #[serde(rename = "@sync-s", skip_serializing_if = "Option::is_none")]
    pub sync_s: Option<bool>,
    #[serde(rename = "@sync-tags", skip_serializing_if = "Option::is_none")]
    pub sync_tags: Option<bool>,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

impl Defaults {
    /// Returns true if all fields are None/default (an "empty" default element).
    pub fn is_empty(&self) -> bool {
        self.remote.is_none()
            && self.revision.is_none()
            && self.dest_branch.is_none()
            && self.upstream.is_none()
            && self.sync_j.is_none()
            && self.sync_c.is_none()
            && self.sync_s.is_none()
            && self.sync_tags.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Notice {
    #[serde(default, rename = "$value")]
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManifestServer {
    #[serde(rename = "@url")]
    pub url: String,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

// === Project child elements ===

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Annotation {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@value")]
    pub value: String,
    #[serde(
        rename = "@keep",
        default = "ret_true",
        skip_serializing_if = "is_true"
    )]
    pub keep: bool,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CopyFile {
    #[serde(rename = "@src")]
    pub src: String,
    #[serde(rename = "@dest")]
    pub dest: String,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkFile {
    #[serde(rename = "@src")]
    pub src: String,
    #[serde(rename = "@dest")]
    pub dest: String,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectChild {
    CopyFile(CopyFile),
    LinkFile(LinkFile),
    Annotation(Annotation),
    Project(Project),

    #[serde(other)]
    Unknown,
}

// === Project ===

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Project {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@remote", skip_serializing_if = "is_default")]
    pub remote: Option<String>,
    #[serde(rename = "@revision", skip_serializing_if = "is_default")]
    pub revision: Option<String>,
    #[serde(rename = "@path", skip_serializing_if = "is_default")]
    pub path: Option<String>,
    #[serde(
        rename = "@rebase",
        default = "ret_true",
        skip_serializing_if = "is_true"
    )]
    pub rebase: bool,
    #[serde(rename = "@sync-c", default, skip_serializing_if = "is_default")]
    pub sync_c: bool,
    #[serde(rename = "@sync-s", skip_serializing_if = "Option::is_none")]
    pub sync_s: Option<bool>,
    #[serde(rename = "@sync-tags", skip_serializing_if = "Option::is_none")]
    pub sync_tags: Option<bool>,
    #[serde(rename = "@clone-depth", skip_serializing_if = "Option::is_none")]
    pub clone_depth: Option<u32>,
    #[serde(rename = "@dest-branch", skip_serializing_if = "Option::is_none")]
    pub dest_branch: Option<String>,
    #[serde(rename = "@upstream", skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(rename = "@groups", default, skip_serializing_if = "String::is_empty")]
    pub groups: String,
    #[serde(rename = "@force-path", default, skip_serializing_if = "is_default")]
    pub force_path: bool,

    #[serde(default, rename = "$value")]
    pub children: Vec<ProjectChild>,
}

// === Extend/Remove project ===

/// Child elements that can appear inside `<extend-project>`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtendProjectChild {
    CopyFile(CopyFile),
    LinkFile(LinkFile),
    Annotation(Annotation),

    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExtendProject {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@path")]
    pub path: Option<String>,
    #[serde(rename = "@groups", default)]
    pub groups: String,
    #[serde(rename = "@revision")]
    pub revision: Option<String>,
    #[serde(rename = "@remote")]
    pub remote: Option<String>,
    // MR-002: dest-path
    #[serde(rename = "@dest-path")]
    pub dest_path: Option<String>,
    // MR-003: dest-branch, upstream
    #[serde(rename = "@dest-branch")]
    pub dest_branch: Option<String>,
    #[serde(rename = "@upstream")]
    pub upstream: Option<String>,
    // MR-004: base-rev
    #[serde(rename = "@base-rev")]
    pub base_rev: Option<String>,

    // MR-005: Parse children instead of ignoring
    #[serde(default, rename = "$value")]
    pub children: Vec<ExtendProjectChild>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RemoveProject {
    // MR-007/008: name is now optional (need name OR path)
    #[serde(rename = "@name")]
    pub name: Option<String>,
    // MR-008: path attribute
    #[serde(rename = "@path")]
    pub path: Option<String>,
    // MR-007: optional attribute
    #[serde(rename = "@optional")]
    pub optional: Option<bool>,
    // MR-009: base-rev
    #[serde(rename = "@base-rev")]
    pub base_rev: Option<String>,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

// === Other top-level elements ===

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RepoHooks {
    #[serde(rename = "@in-project")]
    pub in_project: String,
    #[serde(rename = "@enabled-list")]
    pub enabled_list: String,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Superproject {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@remote")]
    pub remote: Option<String>,
    #[serde(rename = "@revision")]
    pub revision: Option<String>,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Contactinfo {
    #[serde(rename = "@bugurl")]
    pub bugurl: String,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Include {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@groups", default, skip_serializing_if = "String::is_empty")]
    pub groups: String,
    // MR-001: revision attribute for include
    #[serde(rename = "@revision")]
    pub revision: Option<String>,

    #[serde(default, rename = "$value", deserialize_with = "ignore_all")]
    pub children: (),
}

/// Child elements that can appear inside `<submanifest>`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmanifestChild {
    Annotation(Annotation),

    #[serde(other)]
    Unknown,
}

// MR-010: Submanifest support
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Submanifest {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "@remote")]
    pub remote: Option<String>,
    #[serde(rename = "@project")]
    pub project: Option<String>,
    #[serde(rename = "@manifest-name")]
    pub manifest_name: Option<String>,
    #[serde(rename = "@revision")]
    pub revision: Option<String>,
    #[serde(rename = "@path")]
    pub path: Option<String>,
    #[serde(rename = "@groups", default)]
    pub groups: String,
    #[serde(rename = "@default-groups", default)]
    pub default_groups: String,

    #[serde(default, rename = "$value")]
    pub children: Vec<SubmanifestChild>,
}

// === Root manifest element ===

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestChild {
    Include(Include),
    Remote(Remote),
    Default(Defaults),
    Notice(Notice),
    ManifestServer(ManifestServer),
    Project(Project),
    ExtendProject(ExtendProject),
    RepoHooks(RepoHooks),
    Superproject(Superproject),
    Contactinfo(Contactinfo),
    RemoveProject(RemoveProject),
    // MR-010: Submanifest
    Submanifest(Submanifest),

    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename = "manifest")]
pub struct Manifest {
    #[serde(default, rename = "$value")]
    pub children: Vec<ManifestChild>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_manifest() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com/" />
                <default remote="origin" revision="main" />
                <project name="platform/art" path="art" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(manifest.children.len(), 3);
    }

    #[test]
    fn test_parse_project_with_children() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com/" />
                <default remote="origin" revision="main" />
                <project name="platform/art" path="art" groups="pdk">
                    <copyfile src="Makefile" dest="Makefile" />
                    <linkfile src="." dest="art-link" />
                    <annotation name="owner" value="art-team" keep="true" />
                </project>
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        // Find the project
        let project = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Project(p) => Some(p),
            _ => None,
        });
        let project = project.unwrap();
        assert_eq!(project.name, "platform/art");
        assert_eq!(project.path.as_deref(), Some("art"));
        assert_eq!(project.groups, "pdk");
        assert_eq!(project.children.len(), 3);
    }

    #[test]
    fn test_parse_all_top_level_elements() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com/" review="https://review.example.com" />
                <default remote="origin" revision="main" sync-j="4" sync-c="true" />
                <manifest-server url="https://manifest.example.com/" />
                <superproject name="super" remote="origin" />
                <repo-hooks in-project="hooks" enabled-list="pre-upload" />
                <contactinfo bugurl="https://bugs.example.com/" />
                <notice>This is a notice</notice>
                <project name="foo" path="foo" />
                <extend-project name="foo" groups="extra" />
                <remove-project name="bar" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        assert_eq!(manifest.children.len(), 10);
    }

    #[test]
    fn test_parse_defaults_with_all_fields() {
        let xml = r#"
            <manifest>
                <default remote="origin" revision="main" dest-branch="dev"
                         upstream="refs/heads/main" sync-j="8"
                         sync-c="true" sync-s="false" sync-tags="true" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let defaults = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Default(d) => Some(d),
            _ => None,
        });
        let defaults = defaults.unwrap();
        assert_eq!(defaults.remote.as_deref(), Some("origin"));
        assert_eq!(defaults.revision.as_deref(), Some("main"));
        assert_eq!(defaults.dest_branch.as_deref(), Some("dev"));
        assert_eq!(defaults.upstream.as_deref(), Some("refs/heads/main"));
        assert_eq!(defaults.sync_j, Some(8));
        assert_eq!(defaults.sync_c, Some(true));
        assert_eq!(defaults.sync_s, Some(false));
        assert_eq!(defaults.sync_tags, Some(true));
    }

    #[test]
    fn test_parse_remote_with_all_fields() {
        let xml = r#"
            <manifest>
                <remote name="origin" alias="o" fetch="https://example.com/"
                        pushurl="https://push.example.com/"
                        review="https://review.example.com/"
                        revision="main" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let remote = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Remote(r) => Some(r),
            _ => None,
        });
        let remote = remote.unwrap();
        assert_eq!(remote.name, "origin");
        assert_eq!(remote.alias.as_deref(), Some("o"));
        assert_eq!(remote.fetch, "https://example.com/");
        assert_eq!(remote.pushurl.as_deref(), Some("https://push.example.com/"));
        assert_eq!(
            remote.review.as_deref(),
            Some("https://review.example.com/")
        );
        assert_eq!(remote.revision.as_deref(), Some("main"));
    }

    #[test]
    fn test_project_defaults() {
        let xml = r#"
            <manifest>
                <project name="test" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let project = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Project(p) => Some(p),
            _ => None,
        });
        let project = project.unwrap();
        assert_eq!(project.name, "test");
        assert!(project.path.is_none());
        assert!(project.remote.is_none());
        assert!(project.revision.is_none());
        assert!(project.rebase); // defaults to true
        assert!(!project.sync_c); // defaults to false
        assert!(project.sync_s.is_none());
        assert!(project.sync_tags.is_none());
        assert!(project.clone_depth.is_none());
        assert!(project.groups.is_empty());
        assert!(!project.force_path);
    }

    #[test]
    fn test_unknown_elements_ignored() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com/" />
                <some-future-element foo="bar" />
                <project name="test" path="test" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        // Should have 3 children: remote, unknown, project
        assert_eq!(manifest.children.len(), 3);
    }

    // MR-001: Include revision attribute
    #[test]
    fn test_parse_include_with_revision() {
        let xml = r#"
            <manifest>
                <include name="extra.xml" groups="g1" revision="v2" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let inc = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Include(i) => Some(i),
            _ => None,
        });
        let inc = inc.unwrap();
        assert_eq!(inc.name, "extra.xml");
        assert_eq!(inc.groups, "g1");
        assert_eq!(inc.revision.as_deref(), Some("v2"));
    }

    // MR-002/003/004/005: ExtendProject with new attrs and children
    #[test]
    fn test_parse_extend_project_full() {
        let xml = r#"
            <manifest>
                <extend-project name="foo" dest-path="new/path" dest-branch="dev"
                                upstream="refs/heads/dev" base-rev="abc123">
                    <copyfile src="a" dest="b" />
                    <linkfile src="c" dest="d" />
                    <annotation name="k" value="v" />
                </extend-project>
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let ep = manifest.children.iter().find_map(|c| match c {
            ManifestChild::ExtendProject(e) => Some(e),
            _ => None,
        });
        let ep = ep.unwrap();
        assert_eq!(ep.name, "foo");
        assert_eq!(ep.dest_path.as_deref(), Some("new/path"));
        assert_eq!(ep.dest_branch.as_deref(), Some("dev"));
        assert_eq!(ep.upstream.as_deref(), Some("refs/heads/dev"));
        assert_eq!(ep.base_rev.as_deref(), Some("abc123"));
        assert_eq!(ep.children.len(), 3);
    }

    // MR-007/008/009: RemoveProject with new attrs
    #[test]
    fn test_parse_remove_project_full() {
        let xml = r#"
            <manifest>
                <remove-project name="foo" path="bar" optional="true" base-rev="abc" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let rp = manifest.children.iter().find_map(|c| match c {
            ManifestChild::RemoveProject(r) => Some(r),
            _ => None,
        });
        let rp = rp.unwrap();
        assert_eq!(rp.name.as_deref(), Some("foo"));
        assert_eq!(rp.path.as_deref(), Some("bar"));
        assert_eq!(rp.optional, Some(true));
        assert_eq!(rp.base_rev.as_deref(), Some("abc"));
    }

    // MR-010: Submanifest
    #[test]
    fn test_parse_submanifest() {
        let xml = r#"
            <manifest>
                <submanifest name="sub1" remote="origin" project="sub/proj"
                             manifest-name="sub.xml" revision="v1"
                             path="subdir" groups="g1,g2" default-groups="dg1" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let sm = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Submanifest(s) => Some(s),
            _ => None,
        });
        let sm = sm.unwrap();
        assert_eq!(sm.name, "sub1");
        assert_eq!(sm.remote.as_deref(), Some("origin"));
        assert_eq!(sm.project.as_deref(), Some("sub/proj"));
        assert_eq!(sm.manifest_name.as_deref(), Some("sub.xml"));
        assert_eq!(sm.revision.as_deref(), Some("v1"));
        assert_eq!(sm.path.as_deref(), Some("subdir"));
        assert_eq!(sm.groups, "g1,g2");
        assert_eq!(sm.default_groups, "dg1");
    }

    // MR-012: Remote annotations
    #[test]
    fn test_parse_remote_with_annotations() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com/">
                    <annotation name="ci" value="true" />
                </remote>
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let remote = manifest.children.iter().find_map(|c| match c {
            ManifestChild::Remote(r) => Some(r),
            _ => None,
        });
        let remote = remote.unwrap();
        assert_eq!(remote.children.len(), 1);
        match &remote.children[0] {
            RemoteChildWrapper::Annotation(a) => {
                assert_eq!(a.name, "ci");
                assert_eq!(a.value, "true");
            }
            _ => panic!("Expected annotation"),
        }
    }

    // MR-008: RemoveProject with path only (no name)
    #[test]
    fn test_parse_remove_project_path_only() {
        let xml = r#"
            <manifest>
                <remove-project path="some/path" />
            </manifest>
        "#;
        let manifest: Manifest = quick_xml::de::from_str(xml).unwrap();
        let rp = manifest.children.iter().find_map(|c| match c {
            ManifestChild::RemoveProject(r) => Some(r),
            _ => None,
        });
        let rp = rp.unwrap();
        assert!(rp.name.is_none());
        assert_eq!(rp.path.as_deref(), Some("some/path"));
    }
}
