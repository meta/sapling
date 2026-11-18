/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Attribute resolution for manifest elements.
//!
//! Transforms raw XML types into fully resolved types by applying
//! the inheritance chain: project → remote → default.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

use crate::ManifestNodes;
use crate::ProjectElement;
use crate::groups::parse_group_list;
use crate::raw;
use crate::types;
use crate::validation;

/// Resolve a raw remote into a resolved remote.
fn resolve_remote(raw: &raw::Remote) -> types::Remote {
    let fetch_url = raw.fetch.trim_end_matches('/').to_string();

    // MR-012: Collect remote annotations
    let annotations = raw
        .children
        .iter()
        .filter_map(|child| match child {
            raw::RemoteChildWrapper::Annotation(a) => Some(types::Annotation {
                name: a.name.clone(),
                value: a.value.clone(),
                keep: a.keep,
            }),
            _ => None,
        })
        .collect();

    types::Remote {
        name: raw.name.clone(),
        alias: raw.alias.clone(),
        fetch_url,
        push_url: raw.pushurl.clone(),
        review: raw.review.clone(),
        revision: raw.revision.clone(),
        annotations,
    }
}

/// Resolve raw defaults into resolved defaults.
fn resolve_defaults(raw: &raw::Defaults) -> types::Defaults {
    types::Defaults {
        remote: raw.remote.clone(),
        revision: raw.revision.clone(),
        dest_branch: raw.dest_branch.clone(),
        upstream: raw.upstream.clone(),
        sync_j: raw.sync_j,
        sync_c: raw.sync_c.unwrap_or(false),
        sync_s: raw.sync_s.unwrap_or(false),
        sync_tags: raw.sync_tags.unwrap_or(true),
    }
}

/// Resolve a single project from raw XML into a fully resolved project.
fn resolve_project(
    raw_project: &raw::Project,
    remotes: &HashMap<String, types::Remote>,
    defaults: &types::Defaults,
    parent_path: Option<&str>,
    parent_name: Option<&str>,
    projects: &mut Vec<types::Project>,
    path_set: &mut HashSet<String>,
    parent_revision: Option<&str>,
    deny_unknown: bool,
) -> Result<types::ProjectId> {
    // Name: for subprojects, prepend parent name
    let name = match parent_name {
        Some(parent) => format!("{}/{}", parent, raw_project.name),
        None => raw_project.name.clone(),
    };

    // Validate project name (rejects .git, .GIT, .repo*, traversal, etc.)
    validation::validate_project_name(&name)
        .with_context(|| format!("Invalid project name '{name}'"))?;

    // Path: defaults to name; for subprojects, join with parent path
    let raw_path = raw_project.path.as_deref().unwrap_or(&raw_project.name);
    let path_str = match parent_path {
        Some(parent) => format!("{parent}/{raw_path}"),
        None => raw_path.to_string(),
    };

    validation::validate_project_path(&path_str)
        .with_context(|| format!("Invalid path for project '{name}'"))?;

    // Track paths for dedup — duplicates are allowed at parse time so that
    // validators can report all issues. Use `Manifest::ensure_repo_valid()`
    // to enforce repo's stricter rules.
    path_set.insert(path_str.clone());

    // Remote resolution: project → defaults
    let remote_name = raw_project
        .remote
        .as_deref()
        .or(defaults.remote.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!("No remote specified for project '{name}' and no default remote set")
        })?
        .to_string();

    // Validate remote exists
    if !remotes.contains_key(&remote_name) {
        anyhow::bail!("Project '{name}' references undefined remote '{remote_name}'");
    }

    // MR-001: Revision resolution: project → parent_revision (from include) → remote → defaults
    let remote = &remotes[&remote_name];
    let revision = raw_project
        .revision
        .as_deref()
        .or(parent_revision)
        .or(remote.revision.as_deref())
        .or(defaults.revision.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No revision specified for project '{name}' and no default revision set"
            )
        })?
        .to_string();

    // sync flags
    let sync_c = raw_project.sync_c || defaults.sync_c;
    let sync_s = raw_project.sync_s.unwrap_or(defaults.sync_s);
    let sync_tags = raw_project.sync_tags.unwrap_or(defaults.sync_tags);

    // dest_branch and upstream: project → defaults
    let dest_branch = raw_project
        .dest_branch
        .clone()
        .or_else(|| defaults.dest_branch.clone());
    let upstream = raw_project
        .upstream
        .clone()
        .or_else(|| defaults.upstream.clone());

    // Groups
    let mut groups = parse_group_list(&raw_project.groups);

    // MR-013: Add automatic groups
    groups.insert("all".to_string());
    groups.insert(format!("name:{name}"));
    groups.insert(format!("path:{path_str}"));
    if !groups.contains("notdefault") {
        groups.insert("default".to_string());
    }

    // Clone depth validation
    if let Some(depth) = raw_project.clone_depth {
        if depth == 0 {
            anyhow::bail!("clone-depth must be > 0 for project '{name}'");
        }
    }

    // Process children: copyfiles, linkfiles, annotations, subprojects
    let mut copy_files = Vec::new();
    let mut link_files = Vec::new();
    let mut annotations = Vec::new();
    let mut subproject_ids = Vec::new();

    for child in &raw_project.children {
        match child {
            raw::ProjectChild::CopyFile(cf) => {
                validation::validate_file_dest(&cf.dest)
                    .with_context(|| format!("Invalid copyfile dest in project '{name}'"))?;
                copy_files.push(types::CopyFile {
                    src: cf.src.clone(),
                    dest: cf.dest.clone(),
                });
            }
            raw::ProjectChild::LinkFile(lf) => {
                validation::validate_file_dest(&lf.dest)
                    .with_context(|| format!("Invalid linkfile dest in project '{name}'"))?;
                link_files.push(types::LinkFile {
                    src: lf.src.clone(),
                    dest: lf.dest.clone(),
                });
            }
            raw::ProjectChild::Annotation(ann) => {
                annotations.push(types::Annotation {
                    name: ann.name.clone(),
                    value: ann.value.clone(),
                    keep: ann.keep,
                });
            }
            raw::ProjectChild::Project(sub) => {
                // git-repo parses nested projects from their own attributes: an include
                // revision applies to the included file's top-level projects only.
                let sub_id = resolve_project(
                    sub,
                    remotes,
                    defaults,
                    Some(&path_str),
                    Some(&name),
                    projects,
                    path_set,
                    None,
                    deny_unknown,
                )?;
                subproject_ids.push(sub_id);
            }
            raw::ProjectChild::Unknown => {
                if deny_unknown {
                    anyhow::bail!("Unknown element in <project> '{name}' (strict parse mode)");
                }
            }
        }
    }

    // Sort and dedup link files
    link_files.sort();
    link_files.dedup();

    let is_top_level = parent_name.is_none();
    let project = types::Project {
        name,
        path: PathBuf::from(path_str),
        remote_name,
        revision,
        dest_branch,
        upstream,
        groups,
        sync_c,
        sync_s,
        sync_tags,
        clone_depth: raw_project.clone_depth,
        rebase: raw_project.rebase,
        annotations,
        copy_files,
        link_files,
        subprojects: subproject_ids,
        is_top_level,
    };

    let id = types::ProjectId(projects.len());
    projects.push(project);
    Ok(id)
}

/// Resolve an entire manifest from raw nodes into a fully resolved Manifest.
pub(crate) fn resolve_manifest(
    nodes: ManifestNodes,
    deny_unknown: bool,
) -> Result<crate::Manifest> {
    // MR-016: Resolve remotes with merging (not erroring on duplicates)
    let mut remotes = HashMap::new();
    for raw_remote in &nodes.remotes {
        // `push` is a legacy Meta duplicate of `pushurl`; we accept-and-ignore it, which
        // is lossless ONLY when it equals `pushurl`. Fail loud on divergence rather than
        // silently drop a distinct push URL.
        if let Some(push) = &raw_remote.push {
            if raw_remote.pushurl.as_deref() != Some(push.as_str()) {
                anyhow::bail!(
                    "remote '{}': nonstandard `push` attribute diverges from `pushurl` (push={push:?}, pushurl={:?}); refusing to silently drop it",
                    raw_remote.name,
                    raw_remote.pushurl,
                );
            }
        }
        if deny_unknown
            && raw_remote
                .children
                .iter()
                .any(|c| matches!(c, raw::RemoteChildWrapper::Unknown))
        {
            anyhow::bail!(
                "Unknown element in <remote> '{}' (strict parse mode)",
                raw_remote.name
            );
        }
        if let Some(existing) = remotes.get_mut(&raw_remote.name) {
            // Merge: later non-None fields override
            let existing: &mut types::Remote = existing;
            if raw_remote.alias.is_some() {
                existing.alias = raw_remote.alias.clone();
            }
            if raw_remote.pushurl.is_some() {
                existing.push_url = raw_remote.pushurl.clone();
            }
            if raw_remote.review.is_some() {
                existing.review = raw_remote.review.clone();
            }
            if raw_remote.revision.is_some() {
                existing.revision = raw_remote.revision.clone();
            }
            // Merge fetch URL (later wins if non-empty)
            if !raw_remote.fetch.is_empty() {
                existing.fetch_url = raw_remote.fetch.trim_end_matches('/').to_string();
            }
            // Merge annotations
            for child in &raw_remote.children {
                if let raw::RemoteChildWrapper::Annotation(a) = child {
                    existing.annotations.push(types::Annotation {
                        name: a.name.clone(),
                        value: a.value.clone(),
                        keep: a.keep,
                    });
                }
            }
        } else {
            remotes.insert(raw_remote.name.clone(), resolve_remote(raw_remote));
        }
    }

    // MR-017: Resolve defaults — error on multiple non-empty defaults
    let non_empty_defaults: Vec<_> = nodes.defaults.iter().filter(|d| !d.is_empty()).collect();
    if non_empty_defaults.len() > 1 {
        anyhow::bail!("Multiple non-empty <default> elements");
    }
    let defaults = nodes
        .defaults
        .last()
        .map(resolve_defaults)
        .unwrap_or_default();

    // MR-018: Validate sync-j > 0
    if defaults.sync_j == Some(0) {
        anyhow::bail!("sync-j must be > 0");
    }

    // MR-022: Validate no duplicate notice/manifest-server
    if nodes.notices.len() > 1 {
        anyhow::bail!("Multiple <notice> elements");
    }
    if nodes.manifest_servers.len() > 1 {
        anyhow::bail!("Multiple <manifest-server> elements");
    }

    // 3. Process project elements in order
    let mut projects: Vec<types::Project> = Vec::new();
    let mut project_map: BTreeMap<String, Vec<types::ProjectId>> = BTreeMap::new();
    let mut path_set: HashSet<String> = HashSet::new();
    let mut repo_hooks: Option<types::RepoHooks> = None;

    for element in &nodes.project_elements {
        match element {
            ProjectElement::Project(raw_project, parent_revision) => {
                let id = resolve_project(
                    raw_project,
                    &remotes,
                    &defaults,
                    None,
                    None,
                    &mut projects,
                    &mut path_set,
                    parent_revision.as_deref(),
                    deny_unknown,
                )?;
                project_map
                    .entry(projects[id.0].name.clone())
                    .or_default()
                    .push(id);
            }
            ProjectElement::ExtendProject(ext) => {
                // Find the project to extend
                let ids = project_map.get(&ext.name).ok_or_else(|| {
                    anyhow::anyhow!("extend-project references undefined project '{}'", ext.name)
                })?;

                // If path is specified, find the specific project at that path
                let target_ids: Vec<types::ProjectId> = if let Some(ref path) = ext.path {
                    ids.iter()
                        .copied()
                        .filter(|id| projects[id.0].path.to_str() == Some(path.as_str()))
                        .collect()
                } else {
                    ids.clone()
                };

                if target_ids.is_empty() {
                    anyhow::bail!(
                        "extend-project: no project '{}' found{}",
                        ext.name,
                        ext.path
                            .as_ref()
                            .map(|p| format!(" at path '{p}'"))
                            .unwrap_or_default()
                    );
                }

                for id in target_ids {
                    let project = &mut projects[id.0];

                    // MR-004: base-rev check
                    if let Some(ref base) = ext.base_rev {
                        if project.revision != *base {
                            anyhow::bail!(
                                "extend-project '{}': base-rev '{}' does not match current revision '{}'",
                                ext.name,
                                base,
                                project.revision
                            );
                        }
                    }

                    // Extend groups
                    if !ext.groups.is_empty() {
                        let new_groups = parse_group_list(&ext.groups);
                        project.groups.extend(new_groups);
                    }

                    // Drop local groups to prevent them from leaking into
                    // superproject override manifests (upstream: cfef234).
                    project.groups.retain(|g| !g.starts_with("local:"));

                    // Override revision if specified
                    if let Some(ref rev) = ext.revision {
                        project.revision = rev.clone();
                    }

                    // Override remote if specified
                    if let Some(ref remote) = ext.remote {
                        if !remotes.contains_key(remote) {
                            anyhow::bail!(
                                "extend-project for '{}' references undefined remote '{}'",
                                ext.name,
                                remote
                            );
                        }
                        project.remote_name = remote.clone();
                    }

                    // MR-003: dest-branch and upstream
                    if let Some(ref db) = ext.dest_branch {
                        project.dest_branch = Some(db.clone());
                    }
                    if let Some(ref us) = ext.upstream {
                        project.upstream = Some(us.clone());
                    }

                    // MR-002: dest-path — move project to new path
                    if let Some(ref dest_path) = ext.dest_path {
                        // Remove old path from path_set
                        if let Some(old_path) = project.path.to_str() {
                            path_set.remove(old_path);
                        }
                        // Set new path
                        let new_path = dest_path.clone();
                        if !path_set.insert(new_path.clone()) {
                            anyhow::bail!(
                                "extend-project dest-path '{new_path}' conflicts with existing project path"
                            );
                        }
                        project.path = PathBuf::from(new_path);
                        // Update the path: group
                        project.groups.retain(|g| !g.starts_with("path:"));
                        project
                            .groups
                            .insert(format!("path:{}", project.path.display()));
                    }

                    // MR-005: Merge child elements (copyfile, linkfile, annotation)
                    for child in &ext.children {
                        match child {
                            raw::ExtendProjectChild::CopyFile(cf) => {
                                validation::validate_file_dest(&cf.dest).with_context(|| {
                                    format!(
                                        "Invalid copyfile dest in extend-project '{}'",
                                        ext.name
                                    )
                                })?;
                                project.copy_files.push(types::CopyFile {
                                    src: cf.src.clone(),
                                    dest: cf.dest.clone(),
                                });
                            }
                            raw::ExtendProjectChild::LinkFile(lf) => {
                                validation::validate_file_dest(&lf.dest).with_context(|| {
                                    format!(
                                        "Invalid linkfile dest in extend-project '{}'",
                                        ext.name
                                    )
                                })?;
                                project.link_files.push(types::LinkFile {
                                    src: lf.src.clone(),
                                    dest: lf.dest.clone(),
                                });
                            }
                            raw::ExtendProjectChild::Annotation(ann) => {
                                project.annotations.push(types::Annotation {
                                    name: ann.name.clone(),
                                    value: ann.value.clone(),
                                    keep: ann.keep,
                                });
                            }
                            raw::ExtendProjectChild::Unknown => {
                                if deny_unknown {
                                    anyhow::bail!(
                                        "Unknown element in <extend-project> '{}' (strict parse mode)",
                                        ext.name
                                    );
                                }
                            }
                        }
                    }

                    // Re-sort and dedup link files after merging
                    project.link_files.sort();
                    project.link_files.dedup();
                }
            }
            ProjectElement::RemoveProject(rm) => {
                // MR-008: Support removal by name, path, or both
                let name = rm.name.as_deref();
                let path = rm.path.as_deref();
                let optional = rm.optional.unwrap_or(false);

                if name.is_none() && path.is_none() {
                    anyhow::bail!("remove-project must specify name or path (or both)");
                }

                let mut found = false;

                if let Some(rm_name) = name {
                    if let Some(ref rm_path) = rm.path {
                        // Both name and path: remove only the project matching both
                        if let Some(ids) = project_map.get(rm_name) {
                            let matching: Vec<types::ProjectId> = ids
                                .iter()
                                .copied()
                                .filter(|id| projects[id.0].path.to_str() == Some(rm_path.as_str()))
                                .collect();
                            if !matching.is_empty() {
                                found = true;
                                for id in &matching {
                                    // MR-009: base-rev check
                                    if let Some(ref base) = rm.base_rev {
                                        if projects[id.0].revision != *base {
                                            anyhow::bail!(
                                                "remove-project '{}': base-rev '{}' does not match current revision '{}'",
                                                rm_name,
                                                base,
                                                projects[id.0].revision
                                            );
                                        }
                                    }

                                    if let Some(p) = projects[id.0].path.to_str() {
                                        path_set.remove(p);
                                    }
                                    // Check if this project is the repo-hooks target
                                    if let Some(ref rh) = repo_hooks {
                                        if rh.in_project == projects[id.0].name {
                                            repo_hooks = None;
                                        }
                                    }
                                    projects[id.0].is_top_level = false;
                                }
                                // Remove matching IDs from the project_map entry
                                if let Some(ids) = project_map.get_mut(rm_name) {
                                    ids.retain(|id| {
                                        projects[id.0].path.to_str() != Some(rm_path.as_str())
                                    });
                                    if ids.is_empty() {
                                        project_map.remove(rm_name);
                                    }
                                }
                            }
                        }
                    } else {
                        // Name only: remove all projects with that name
                        if let Some(ids) = project_map.remove(rm_name) {
                            found = true;
                            for id in &ids {
                                // MR-009: base-rev check
                                if let Some(ref base) = rm.base_rev {
                                    if projects[id.0].revision != *base {
                                        anyhow::bail!(
                                            "remove-project '{}': base-rev '{}' does not match current revision '{}'",
                                            rm_name,
                                            base,
                                            projects[id.0].revision
                                        );
                                    }
                                }

                                if let Some(p) = projects[id.0].path.to_str() {
                                    path_set.remove(p);
                                }
                                if let Some(ref rh) = repo_hooks {
                                    if rh.in_project == projects[id.0].name {
                                        repo_hooks = None;
                                    }
                                }
                                projects[id.0].is_top_level = false;
                            }
                        }
                    }
                } else if let Some(rm_path) = path {
                    // Path only: remove project at that path regardless of name
                    let mut to_remove = Vec::new();
                    for (pname, ids) in project_map.iter() {
                        for id in ids {
                            if projects[id.0].path.to_str() == Some(rm_path) {
                                to_remove.push((pname.clone(), *id));
                            }
                        }
                    }
                    if !to_remove.is_empty() {
                        found = true;
                        for (pname, id) in &to_remove {
                            // MR-009: base-rev check
                            if let Some(ref base) = rm.base_rev {
                                if projects[id.0].revision != *base {
                                    anyhow::bail!(
                                        "remove-project path '{}': base-rev '{}' does not match current revision '{}'",
                                        rm_path,
                                        base,
                                        projects[id.0].revision
                                    );
                                }
                            }

                            path_set.remove(rm_path);
                            if let Some(ref rh) = repo_hooks {
                                if rh.in_project == projects[id.0].name {
                                    repo_hooks = None;
                                }
                            }
                            projects[id.0].is_top_level = false;
                            if let Some(ids) = project_map.get_mut(pname) {
                                ids.retain(|i| i != id);
                                if ids.is_empty() {
                                    project_map.remove(pname);
                                }
                            }
                        }
                    }
                }

                // MR-007: If optional and not found, skip instead of erroring
                if !found && !optional {
                    let desc = match (name, path) {
                        (Some(n), Some(p)) => format!("'{n}' at path '{p}'"),
                        (Some(n), None) => format!("'{n}'"),
                        (None, Some(p)) => format!("at path '{p}'"),
                        (None, None) => unreachable!(),
                    };
                    anyhow::bail!("remove-project references undefined project {desc}");
                }
            }
            ProjectElement::RepoHooks(rh) => {
                repo_hooks = Some(types::RepoHooks {
                    in_project: rh.in_project.clone(),
                    enabled_list: rh.enabled_list.clone(),
                });
            }
        }
    }

    // 4. Build manifest
    let notice = nodes.notices.first().map(|n| n.content.clone());
    let manifest_server = nodes.manifest_servers.first().map(|ms| ms.url.clone());
    let superproject = nodes.superprojects.first().map(|sp| types::Superproject {
        name: sp.name.clone(),
        remote: sp.remote.clone(),
        revision: sp.revision.clone(),
    });
    let contactinfo = nodes.contactinfo.map(|ci| types::Contactinfo {
        bugurl: ci.bugurl.clone(),
    });

    // MR-010: Resolve submanifests
    let submanifests: Vec<types::Submanifest> = nodes
        .submanifests
        .iter()
        .map(|sm| -> Result<types::Submanifest> {
            if deny_unknown
                && sm
                    .children
                    .iter()
                    .any(|c| matches!(c, raw::SubmanifestChild::Unknown))
            {
                anyhow::bail!(
                    "Unknown element in <submanifest> '{}' (strict parse mode)",
                    sm.name
                );
            }
            Ok(types::Submanifest {
                name: sm.name.clone(),
                remote: sm.remote.clone(),
                project: sm.project.clone(),
                manifest_name: sm.manifest_name.clone(),
                revision: sm.revision.clone(),
                path: sm.path.clone(),
                groups: parse_group_list(&sm.groups),
                default_groups: sm
                    .default_groups
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                annotations: sm
                    .children
                    .iter()
                    .filter_map(|child| match child {
                        raw::SubmanifestChild::Annotation(a) => Some(types::Annotation {
                            name: a.name.clone(),
                            value: a.value.clone(),
                            keep: a.keep,
                        }),
                        _ => None,
                    })
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(crate::Manifest {
        remotes,
        defaults,
        projects,
        project_map,
        notice,
        manifest_server,
        superproject,
        contactinfo,
        repo_hooks,
        submanifests,
    })
}

#[cfg(test)]
mod tests {
    use crate::Manifest;

    #[test]
    fn test_resolve_simple_manifest() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="platform/art" path="art" />
                <project name="platform/base" path="base" />
            </manifest>
        "#;
        let manifest = Manifest::from_xml(xml.as_bytes(), &mut |_| {
            anyhow::bail!("no includes expected")
        })
        .unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "platform/art");
        assert_eq!(projects[0].path.to_str().unwrap(), "art");
        assert_eq!(projects[0].remote_name, "origin");
        assert_eq!(projects[0].revision, "main");
    }

    #[test]
    fn test_resolve_project_inherits_remote_revision() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" revision="v2" />
                <default remote="origin" revision="main" />
                <project name="uses-default" path="a" />
                <project name="uses-remote" path="b" remote="origin" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        // The cascade is: project.revision → remote.revision → default.revision
        // So "uses-default" has no project revision, remote "origin" has "v2", so it gets "v2".
        assert_eq!(projects[0].revision, "v2"); // from remote
        assert_eq!(projects[1].revision, "v2"); // from remote
    }

    #[test]
    fn test_resolve_project_explicit_revision_overrides() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" revision="v2" />
                <default remote="origin" revision="main" />
                <project name="explicit" path="a" revision="custom" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects[0].revision, "custom");
    }

    #[test]
    fn test_resolve_extend_project() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" groups="base" />
                <extend-project name="foo" groups="extra" revision="v2" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects[0].revision, "v2");
        assert!(projects[0].groups.contains("base"));
        assert!(projects[0].groups.contains("extra"));
    }

    #[test]
    fn test_resolve_remove_project() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="keep" path="keep" />
                <project name="remove-me" path="remove-me" />
                <remove-project name="remove-me" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "keep");
    }

    #[test]
    fn test_duplicate_path_parses_but_ensure_repo_valid_fails() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="same" />
                <project name="bar" path="same" />
            </manifest>
        "#;
        // Parsing should succeed (permissive)
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        assert_eq!(manifest.projects().count(), 2);

        // But ensure_repo_valid should reject it
        let result = manifest.ensure_repo_valid();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("Duplicate"),
            "Expected duplicate path error"
        );
    }

    #[test]
    fn test_resolve_undefined_remote_error() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" remote="nonexistent" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_path_defaults_to_name() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="platform/art" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects[0].path.to_str().unwrap(), "platform/art");
    }

    #[test]
    fn test_resolve_project_with_copy_and_link_files() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="build" path="build">
                    <copyfile src="core/root.mk" dest="Makefile" />
                    <linkfile src="." dest="build-link" />
                </project>
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();

        let projects: Vec<_> = manifest.projects().collect();
        assert_eq!(projects[0].copy_files.len(), 1);
        assert_eq!(projects[0].copy_files[0].src, "core/root.mk");
        assert_eq!(projects[0].copy_files[0].dest, "Makefile");
        assert_eq!(projects[0].link_files.len(), 1);
        assert_eq!(projects[0].link_files[0].src, ".");
        assert_eq!(projects[0].link_files[0].dest, "build-link");
    }

    // MR-002: extend-project dest-path
    #[test]
    fn test_extend_project_dest_path() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="old/path" />
                <extend-project name="foo" dest-path="new/path" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let project = manifest.projects().next().unwrap();
        assert_eq!(project.path.to_str().unwrap(), "new/path");
    }

    // MR-003: extend-project upstream/dest-branch
    #[test]
    fn test_extend_project_upstream_dest_branch() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <extend-project name="foo" upstream="refs/heads/dev" dest-branch="dev" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let project = manifest.projects().next().unwrap();
        assert_eq!(project.upstream.as_deref(), Some("refs/heads/dev"));
        assert_eq!(project.dest_branch.as_deref(), Some("dev"));
    }

    // MR-004: extend-project base-rev mismatch
    #[test]
    fn test_extend_project_base_rev_mismatch() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <extend-project name="foo" base-rev="wrong" revision="v2" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base-rev"));
    }

    // MR-005: extend-project child elements
    #[test]
    fn test_extend_project_child_elements() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <extend-project name="foo">
                    <copyfile src="a" dest="b" />
                    <linkfile src="c" dest="d" />
                    <annotation name="k" value="v" />
                </extend-project>
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let project = manifest.projects().next().unwrap();
        assert_eq!(project.copy_files.len(), 1);
        assert_eq!(project.link_files.len(), 1);
        assert_eq!(project.annotations.len(), 1);
    }

    // MR-007: remove-project optional=true
    #[test]
    fn test_remove_project_optional_not_found() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <remove-project name="nonexistent" optional="true" />
            </manifest>
        "#;
        // Should NOT error because optional=true
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        assert_eq!(manifest.project_count(), 0);
    }

    // MR-008: remove-project by path
    #[test]
    fn test_remove_project_by_path() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="a" />
                <project name="foo" path="b" />
                <remove-project path="a" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        assert_eq!(manifest.project_count(), 1);
        assert_eq!(
            manifest.projects().next().unwrap().path.to_str().unwrap(),
            "b"
        );
    }

    // MR-009: remove-project base-rev
    #[test]
    fn test_remove_project_base_rev_mismatch() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
                <remove-project name="foo" base-rev="wrong" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base-rev"));
    }

    // MR-013: Default groups
    #[test]
    fn test_default_groups_injected() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="bar" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let project = manifest.projects().next().unwrap();
        assert!(project.groups.contains("all"));
        assert!(project.groups.contains("name:foo"));
        assert!(project.groups.contains("path:bar"));
        assert!(project.groups.contains("default"));
    }

    #[test]
    fn test_notdefault_suppresses_default_group() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" groups="notdefault" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let project = manifest.projects().next().unwrap();
        assert!(project.groups.contains("all"));
        assert!(project.groups.contains("notdefault"));
        assert!(!project.groups.contains("default"));
    }

    // MR-016: Duplicate remote merges
    #[test]
    fn test_duplicate_remote_merges() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <remote name="origin" fetch="https://example2.com" pushurl="https://push.com" />
                <default remote="origin" revision="main" />
                <project name="foo" path="foo" />
            </manifest>
        "#;
        let manifest =
            Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes")).unwrap();
        let remote = manifest.remote("origin").unwrap();
        assert_eq!(remote.fetch_url, "https://example2.com");
        assert_eq!(remote.push_url.as_deref(), Some("https://push.com"));
    }

    // MR-017: Duplicate non-empty defaults error
    #[test]
    fn test_duplicate_non_empty_defaults_error() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <default remote="origin" revision="dev" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Multiple non-empty")
        );
    }

    // MR-018: sync-j=0 errors
    #[test]
    fn test_sync_j_zero_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" sync-j="0" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("sync-j"));
    }

    // MR-022: Duplicate notice/manifest-server errors
    #[test]
    fn test_duplicate_notice_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <notice>First</notice>
                <notice>Second</notice>
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Multiple <notice>")
        );
    }

    #[test]
    fn test_duplicate_manifest_server_errors() {
        let xml = r#"
            <manifest>
                <remote name="origin" fetch="https://example.com" />
                <default remote="origin" revision="main" />
                <manifest-server url="https://a.com/" />
                <manifest-server url="https://b.com/" />
            </manifest>
        "#;
        let result = Manifest::from_xml(xml.as_bytes(), &mut |_| anyhow::bail!("no includes"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Multiple <manifest-server>")
        );
    }
}
