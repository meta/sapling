/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Include dependency tracking for manifest files.
//!
//! Provides a public API to find all transitively included manifest files.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::raw::Manifest;
use crate::raw::ManifestChild;

/// Find all transitively included manifest files.
///
/// Returns a map of include name → canonical path for all files
/// reachable from the root manifest via `<include>` tags.
///
/// The root file is included in the result using its filename as key.
/// Circular dependencies are detected and reported as errors.
pub fn find_include_dependencies<P: AsRef<Path>>(file_path: P) -> Result<HashMap<String, PathBuf>> {
    let mut files = HashMap::new();
    let mut dependency_chain = HashSet::new();

    let canonical_root = fs::canonicalize(&file_path).with_context(|| {
        format!(
            "Failed to canonicalize root path: {}",
            file_path.as_ref().display()
        )
    })?;

    dependency_chain.insert(canonical_root.clone());

    files.insert(
        file_path
            .as_ref()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("root.xml")
            .to_string(),
        canonical_root.clone(),
    );

    find_includes_recursive(&canonical_root, &mut files, &mut dependency_chain)?;
    Ok(files)
}

/// Extract the `<include name="...">` names referenced directly by a single
/// manifest XML document (one level, not transitive).
///
/// Byte-oriented and filesystem-free — for callers that fetch manifest content
/// themselves (e.g. over the network) and drive their own include resolution.
/// Uses the same `raw::Manifest` deserialization as the full parser, so the set
/// of recognized includes never diverges from what parsing would resolve.
pub fn include_names(xml: &[u8]) -> Result<Vec<String>> {
    let manifest: Manifest = quick_xml::de::from_reader(xml)
        .map_err(|e| anyhow::anyhow!("Failed to parse manifest XML for includes: {e}"))?;
    Ok(manifest
        .children
        .into_iter()
        .filter_map(|node| match node {
            ManifestChild::Include(inc) => Some(inc.name),
            _ => None,
        })
        .collect())
}

fn find_includes_recursive(
    file_path: &Path,
    files: &mut HashMap<String, PathBuf>,
    dependency_chain: &mut HashSet<PathBuf>,
) -> Result<()> {
    let file_content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

    let base_dir = file_path
        .parent()
        .with_context(|| format!("Cannot get parent directory of {}", file_path.display()))?;

    let mut reader = Reader::from_str(&file_content);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(ref e)) if e.name().as_ref() == "include" => {
                for attr in e.attributes() {
                    let attr = attr.with_context(|| "Failed to read XML attribute")?;
                    if attr.key.as_ref() == "name" {
                        let include_name = attr.value.as_ref();
                        let full_path = base_dir.join(include_name);
                        let canonical_path = fs::canonicalize(&full_path).with_context(|| {
                            format!("Failed to canonicalize path: {}", full_path.display())
                        })?;

                        // Circular dependency detection
                        if dependency_chain.contains(&canonical_path) {
                            let cycle_path = dependency_chain
                                .iter()
                                .map(|p| {
                                    p.file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned()
                                })
                                .collect::<Vec<_>>()
                                .join(" -> ");

                            anyhow::bail!(
                                "Circular dependency detected: {cycle_path} -> {include_name}"
                            );
                        }

                        // Only process new files
                        if !files.values().any(|p| p == &canonical_path) {
                            files.insert(include_name.to_owned(), canonical_path.clone());

                            dependency_chain.insert(canonical_path.clone());
                            find_includes_recursive(&canonical_path, files, dependency_chain)
                                .with_context(|| {
                                    format!(
                                        "Failed to process include file: {}",
                                        full_path.display()
                                    )
                                })?;
                            dependency_chain.remove(&canonical_path);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => anyhow::bail!("Error parsing XML: {e}"),
            _ => (),
        }
        buf.clear();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_include_dependencies_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("manifest.xml");
        std::fs::write(
            &root,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="origin" fetch="https://example.com" />
  <default remote="origin" revision="main" />
  <project name="foo" path="foo" />
</manifest>"#,
        )
        .unwrap();

        let deps = find_include_dependencies(&root).unwrap();
        assert_eq!(deps.len(), 1);
        assert!(deps.contains_key("manifest.xml"));
    }

    #[test]
    fn test_find_include_dependencies_with_includes() {
        let tmp = tempfile::tempdir().unwrap();

        std::fs::write(
            tmp.path().join("root.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <include name="a.xml" />
  <include name="b.xml" />
</manifest>"#,
        )
        .unwrap();

        std::fs::write(
            tmp.path().join("a.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="a" path="a" />
</manifest>"#,
        )
        .unwrap();

        std::fs::write(
            tmp.path().join("b.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <include name="c.xml" />
</manifest>"#,
        )
        .unwrap();

        std::fs::write(
            tmp.path().join("c.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="c" path="c" />
</manifest>"#,
        )
        .unwrap();

        let deps = find_include_dependencies(tmp.path().join("root.xml")).unwrap();
        assert_eq!(deps.len(), 4); // root, a, b, c
        assert!(deps.contains_key("root.xml"));
        assert!(deps.contains_key("a.xml"));
        assert!(deps.contains_key("b.xml"));
        assert!(deps.contains_key("c.xml"));
    }

    #[test]
    fn test_find_include_dependencies_circular() {
        let tmp = tempfile::tempdir().unwrap();

        std::fs::write(
            tmp.path().join("a.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <include name="b.xml" />
</manifest>"#,
        )
        .unwrap();

        std::fs::write(
            tmp.path().join("b.xml"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <include name="a.xml" />
</manifest>"#,
        )
        .unwrap();

        let result = find_include_dependencies(tmp.path().join("a.xml"));
        assert!(result.is_err());
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("Circular"),
            "Expected 'Circular' in error: {err_msg}"
        );
    }

    #[test]
    fn test_include_names_extracts_direct_includes() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="origin" fetch="https://example.com" />
  <include name="a.xml" />
  <project name="foo" path="foo" />
  <include name="b.xml" />
</manifest>"#;
        assert_eq!(include_names(xml).unwrap(), vec!["a.xml", "b.xml"]);
    }

    #[test]
    fn test_include_names_empty_when_no_includes() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <remote name="origin" fetch="https://example.com" />
  <project name="foo" path="foo" />
</manifest>"#;
        assert!(include_names(xml).unwrap().is_empty());
    }
}
