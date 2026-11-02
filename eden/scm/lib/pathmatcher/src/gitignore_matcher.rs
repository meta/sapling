/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use ignore::Match;
use ignore::gitignore;
use ignore::gitignore::Glob;
use parking_lot::RwLock;
use types::RepoPath;

use crate::DirectoryMatch;
use crate::Matcher;

/// Lazy `.gitignore` matcher that loads `.gitignore` files on demand.
pub struct GitignoreMatcher {
    ignore: gitignore::Gitignore,

    // Absolute path of this directory. Don't use `ignore.path()` instead:
    // it is empty if the directory has no rules.
    // See https://github.com/BurntSushi/ripgrep/issues/3584.
    dir: PathBuf,

    // PERF: Each Gitignore object stores "root" as "PathBuf" to support
    // matching against an absolute path. Since we enforce relative path
    // in the API, removing that "PathBuf" could reduce memory footprint.
    submatchers: RwLock<HashMap<PathBuf, Box<GitignoreMatcher>>>,

    // Whether this directory is ignored or not.
    ignored: bool,

    case_sensitive: bool,
}

/// Return (next_component, remaining_path), or None if remaining_path is empty.
fn split_path(path: &Path) -> Option<(&Path, &Path)> {
    let mut comps = path.components();
    let comp = comps.next();
    comp.and_then(|c| {
        let rest = comps.as_path();
        if let Component::Normal(s) = c {
            if rest.as_os_str().is_empty() {
                None
            } else {
                Some((Path::new(s), rest))
            }
        } else {
            panic!("ProgrammingError: unexpected path component {c:?}");
        }
    })
}

#[derive(PartialEq)]
enum MatchResult {
    Unspecified,
    Ignored,
    Included,
}

impl<T> From<ignore::Match<T>> for MatchResult {
    fn from(v: ignore::Match<T>) -> MatchResult {
        match v {
            ignore::Match::None => MatchResult::Unspecified,
            ignore::Match::Ignore(_) => MatchResult::Ignored,
            ignore::Match::Whitelist(_) => MatchResult::Included,
        }
    }
}

impl GitignoreMatcher {
    /// Initialize `GitignoreMatch` for the given root directory.
    ///
    /// The `.gitignore` in the root directory will be parsed immediately.
    /// `.gitignore` in subdirectories are parsed lazily.
    ///
    /// `global_gitignore_paths` is an additional list of gitignore files
    /// to be parsed.
    pub fn new<P: AsRef<Path>>(
        root: P,
        global_gitignore_paths: Vec<&Path>,
        case_sensitive: bool,
    ) -> Self {
        let root = root.as_ref();
        let mut builder = gitignore::GitignoreBuilder::new(root);

        // It's safe to ignore the Result, since it's always Ok().
        let _ = builder.case_insensitive(!case_sensitive);

        for path in global_gitignore_paths {
            builder.add(path);
        }
        builder.add(root.join(".gitignore"));
        let ignore = builder
            .build()
            .unwrap_or_else(|_| gitignore::Gitignore::empty());

        let submatchers = RwLock::new(HashMap::new());
        GitignoreMatcher {
            ignore,
            dir: root.to_path_buf(),
            submatchers,
            ignored: false,
            case_sensitive,
        }
    }

    /// Like `new`, but might mark the subtree as "ignored" entirely.
    /// Used internally by `match_subdir_path`.
    fn new_with_rootmatcher(dir: &Path, root: &GitignoreMatcher) -> Self {
        let dir_root_relative = dir
            .strip_prefix(&root.dir)
            .expect("should be a subdirectory of the root");
        let submatchers = RwLock::new(HashMap::new());
        let (ignored, ignore) = if root.match_relative(dir_root_relative, true) {
            (true, gitignore::Gitignore::empty())
        } else {
            let mut builder = gitignore::GitignoreBuilder::new(dir);
            // It's safe to ignore the Result, since it's always Ok().
            let _ = builder.case_insensitive(!root.case_sensitive);
            builder.add(dir.join(".gitignore"));
            (
                false,
                builder
                    .build()
                    .unwrap_or_else(|_| gitignore::Gitignore::empty()),
            )
        };
        GitignoreMatcher {
            ignore,
            dir: dir.to_path_buf(),
            ignored,
            submatchers,
            case_sensitive: root.case_sensitive,
        }
    }

    /// Return true if the normalized relative path should be ignored.
    ///
    /// Panic if the path is not relative, or contains components like
    /// ".." or ".".
    pub fn match_relative<P: AsRef<Path>>(&self, path: P, is_dir: bool) -> bool {
        let path = path.as_ref();
        self.match_path(path, is_dir, self, &mut None) == MatchResult::Ignored
    }

    /// Check .gitignore for the relative path.
    fn match_path<P: AsRef<Path>>(
        &self,
        path: P,
        is_dir: bool,
        root: &GitignoreMatcher,
        explain: &mut Option<&mut Explain>,
    ) -> MatchResult {
        let path = path.as_ref();
        // Everything is ignored regardless if this directory is ignored.
        if self.ignored {
            if let Some(explain) = explain {
                explain.parent_ignored(path, root);
            }
            return MatchResult::Ignored;
        }

        // If explain information is requested, always check this (parent)
        // directory to explain overridden rules.
        if let Some(explain) = explain {
            let matched = self.ignore.matched(path, is_dir);
            match matched {
                Match::Ignore(glob) => explain.add_glob(glob),
                Match::Whitelist(glob) => explain.add_glob(glob),
                _ => {}
            }
        }

        // Check subdir first. It can override this (parent) directory.
        let subdir_result = match split_path(path) {
            None => MatchResult::Unspecified,
            Some((dir, rest)) => self.match_subdir_path(dir, rest, is_dir, root, explain),
        };

        match subdir_result {
            MatchResult::Included => MatchResult::Included,
            MatchResult::Ignored => MatchResult::Ignored,
            MatchResult::Unspecified => self.ignore.matched(path, is_dir).into(),
        }
    }

    /// Check .gitignore in the subdirectory `name` for the path `rest`.
    /// Create submatcher on demand.
    fn match_subdir_path(
        &self,
        name: &Path,
        rest: &Path,
        is_dir: bool,
        root: &GitignoreMatcher,
        explain: &mut Option<&mut Explain>,
    ) -> MatchResult {
        {
            let submatchers = self.submatchers.read_recursive();
            if let Some(m) = submatchers.get(name) {
                return m.as_ref().match_path(rest, is_dir, root, explain);
            }
        }
        {
            let dir = self.dir.join(name);
            if dir.is_dir() {
                let m = GitignoreMatcher::new_with_rootmatcher(&dir, root);
                let result = m.match_path(rest, is_dir, root, explain);
                let mut submatchers = self.submatchers.write();
                submatchers.insert(name.to_path_buf(), Box::new(m));
                result
            } else {
                MatchResult::Unspecified
            }
        }
    }

    /// Explain why a path is ignored or not included. This includes rules
    /// including and excluding the given path, or why parent directories
    /// are ignored.
    ///
    /// Return human-readable text.
    pub fn explain(&self, path: impl AsRef<Path>, is_dir: bool) -> String {
        let mut explain = Explain::new();
        let path = path.as_ref().to_path_buf();
        explain.start_explain(path.clone(), is_dir, self);
        explain.human_text(path, self)
    }
}

/// Context related for the "explain" feature.
struct Explain {
    /// Path being currently explained. The "current" input.
    path: PathBuf,

    /// Related rules affecting the glob. The output.
    rules: Vec<(Glob, PathBuf)>,
}

impl Explain {
    fn new() -> Self {
        let path = PathBuf::new();
        let rules = Vec::new();
        Self { path, rules }
    }

    /// Explain why `path` is ignored.
    fn start_explain(&mut self, path: PathBuf, is_dir: bool, root: &GitignoreMatcher) {
        self.path = path.clone();
        root.match_path(&path, is_dir, root, &mut Some(self));
    }

    /// The glob affects whether `self.path` is ignored or not ignored.
    fn add_glob(&mut self, glob: &Glob) {
        self.rules.push((glob.clone(), self.path.clone()));
    }

    /// `self.path` is ignored because a parent directory is ignored.
    fn parent_ignored(&mut self, suffix: &Path, root: &GitignoreMatcher) {
        // self.path (= prefix + suffix) is ignored because prefix is ignored.
        let mut prefix = self.path.clone();
        for _ in 0..suffix.components().count() {
            prefix.pop();
        }
        self.start_explain(prefix, true, root);
    }

    /// Return human readable text.
    fn human_text(&self, path: PathBuf, root: &GitignoreMatcher) -> String {
        let mut text = String::new();
        let mut current_path = path;
        let mut current_count = 0;

        if self.rules.is_empty() {
            text.push_str(&format!("{}: not ignored\n", self.path.to_string_lossy()));
        }

        let get_overrides = |count: usize| {
            if count > 0 {
                " (overrides previous rules)"
            } else {
                ""
            }
        };

        for (glob, path) in self.rules.iter() {
            let action = if glob.is_whitelist() {
                "unignored"
            } else {
                "ignored"
            };

            let from = match glob.from() {
                Some(path) => {
                    let path = path.strip_prefix(&root.dir).unwrap_or(path);
                    format!("from {}", path.to_string_lossy())
                }
                None => String::new(),
            };

            if path != &current_path {
                text.push_str(&format!(
                    "{}: ignored because {} is ignored{}\n",
                    current_path.to_string_lossy(),
                    path.to_string_lossy(),
                    get_overrides(current_count),
                ));
                current_path = path.clone();
                current_count = 0;
            }

            text.push_str(&format!(
                "{}: {} by rule {} {}{}\n",
                path.to_string_lossy(),
                action,
                glob.original(),
                from,
                get_overrides(current_count),
            ));

            current_count += 1;
        }

        text
    }
}

impl Matcher for GitignoreMatcher {
    fn matches_directory(&self, path: &RepoPath) -> Result<DirectoryMatch> {
        let dm = match self.match_path(path.as_str(), true, self, &mut None) {
            MatchResult::Ignored => DirectoryMatch::Everything,
            MatchResult::Included => DirectoryMatch::Nothing,
            MatchResult::Unspecified => DirectoryMatch::ShouldTraverse,
        };
        Ok(dm)
    }

    fn matches_file(&self, path: &RepoPath) -> Result<bool> {
        Ok(self.match_relative(path.as_str(), false))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use fs_err::File;
    use fs_err::create_dir_all;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_split_path() {
        let p = Path::new("proc/self/stat");

        let (c, p) = split_path(p).unwrap();
        assert_eq!(c, Path::new("proc"));
        assert_eq!(p, Path::new("self/stat"));

        let (c, p) = split_path(p).unwrap();
        assert_eq!(c, Path::new("self"));
        assert_eq!(p, Path::new("stat"));

        assert!(split_path(p).is_none());
    }

    #[test]
    fn test_gitignore_match_directory() {
        let dir = tempdir().unwrap();
        write(dir.path().join(".gitignore"), b"FILE\nDIR/\n");

        let m = GitignoreMatcher::new(dir.path(), Vec::new(), true);
        assert!(m.match_relative("x/FILE", false));
        assert!(m.match_relative("x/FILE", true));
        assert!(!m.match_relative("x/DIR", false));
        assert!(m.match_relative("x/DIR", true));

        assert_eq!(
            m.explain("x/FILE", true),
            "x/FILE: ignored by rule FILE from .gitignore\n"
        );
        assert_eq!(
            m.explain("x/DIR/bar/baz", true),
            "x/DIR/bar/baz: not ignored\n"
        );
    }

    #[test]
    fn test_gitignore_match_subdir() {
        let dir = tempdir().unwrap();

        create_dir_all(dir.path().join("a/b")).expect("mkdir");
        create_dir_all(dir.path().join("c/d")).expect("mkdir");
        write(dir.path().join(".gitignore"), b"a/b\n!c/d/*");
        write(dir.path().join("a/b/.gitignore"), b"!c");
        write(dir.path().join("a/.gitignore"), b"!b/d");
        write(dir.path().join("c/.gitignore"), b"d/e\n!d/f");
        write(dir.path().join("c/d/.gitignore"), b"!e\nf");

        let m = GitignoreMatcher::new(dir.path(), Vec::new(), true);
        assert!(m.match_relative("a/b", false));
        assert!(m.match_relative("a/b/c", false));
        assert!(m.match_relative("a/b/d", false));
        assert!(m.match_relative("c/d/f", false));
        assert!(!m.match_relative("c/d/e", false));

        assert_eq!(
            m.explain("a/b", false),
            "a/b: ignored by rule a/b from .gitignore\n"
        );
        assert_eq!(
            m.explain("a/b/c", false),
            r#"a/b/c: ignored because a/b is ignored
a/b: ignored by rule a/b from .gitignore
"#
        );

        // Windows uses `\` instead of `/` as path separator
        #[cfg(unix)]
        {
            assert_eq!(
                m.explain("a/b/d", false),
                r#"a/b/d: unignored by rule !b/d from a/.gitignore
a/b/d: ignored because a/b is ignored (overrides previous rules)
a/b: ignored by rule a/b from .gitignore
"#
            );
            assert_eq!(
                m.explain("c/d/f", false),
                r#"c/d/f: unignored by rule !c/d/* from .gitignore
c/d/f: unignored by rule !d/f from c/.gitignore (overrides previous rules)
c/d/f: ignored by rule f from c/d/.gitignore (overrides previous rules)
"#
            );
            assert_eq!(
                m.explain("c/d/e", false),
                r#"c/d/e: unignored by rule !c/d/* from .gitignore
c/d/e: ignored by rule d/e from c/.gitignore (overrides previous rules)
c/d/e: unignored by rule !e from c/d/.gitignore (overrides previous rules)
"#
            );
        }
    }

    #[test]
    fn test_gitignore_below_dirs_without_rules() {
        let dir = tempdir().unwrap();
        create_dir_all(dir.path().join("a/b/DIR")).expect("mkdir");
        write(dir.path().join("a/b/.gitignore"), b"DIR/\n");

        // Neither the root nor "a" has a .gitignore.
        let m = GitignoreMatcher::new(dir.path(), Vec::new(), true);
        assert!(m.match_relative("a/b/DIR/f", false));

        #[cfg(unix)]
        assert_eq!(
            m.explain("a/b/DIR", true),
            "a/b/DIR: ignored by rule DIR/ from a/b/.gitignore\n"
        );
    }

    #[test]
    fn test_global_gitignore() {
        let dir = tempdir().unwrap();
        let ignore1_path = dir.path().join("ignore1");
        let ignore2_path = dir.path().join("ignore2");

        write(&ignore1_path, b"a*");
        write(&ignore2_path, b"b*");

        let m = GitignoreMatcher::new(dir.path(), vec![&ignore1_path, &ignore2_path], true);
        assert!(m.match_relative("a1", true));
        assert!(m.match_relative("b1", true));

        assert_eq!(
            m.explain("a1", true),
            "a1: ignored by rule a* from ignore1\n"
        );
        assert_eq!(
            m.explain("b1", true),
            "b1: ignored by rule b* from ignore2\n"
        );
    }

    #[test]
    fn test_explain() {
        let dir = tempdir().unwrap();
        create_dir_all(dir.path().join("a/b")).unwrap();
        create_dir_all(dir.path().join("c/d/e")).unwrap();
        create_dir_all(dir.path().join("c/f/g")).unwrap();
        create_dir_all(dir.path().join("c/g")).unwrap();
        write(dir.path().join(".gitignore"), b"*.pyc\nd/\ng/");
        write(dir.path().join("a/.gitignore"), b"!a*.pyc");
        write(dir.path().join("a/b/.gitignore"), b"a1*.pyc");
        write(dir.path().join("c/.gitignore"), b"!g/");
        write(dir.path().join("c/f/.gitignore"), b"g/");

        let m = GitignoreMatcher::new(dir.path(), Vec::new(), true);
        assert_eq!(
            m.explain("1.pyc", true),
            "1.pyc: ignored by rule *.pyc from .gitignore\n"
        );

        // Windows uses `\` instead of `/` as path separator
        #[cfg(unix)]
        {
            assert_eq!(
                m.explain("a/a1.pyc", true),
                r#"a/a1.pyc: ignored by rule *.pyc from .gitignore
a/a1.pyc: unignored by rule !a*.pyc from a/.gitignore (overrides previous rules)
"#
            );
            assert_eq!(
                m.explain("a/b/a10.pyc", true),
                r#"a/b/a10.pyc: ignored by rule *.pyc from .gitignore
a/b/a10.pyc: unignored by rule !a*.pyc from a/.gitignore (overrides previous rules)
a/b/a10.pyc: ignored by rule a1*.pyc from a/b/.gitignore (overrides previous rules)
"#
            );
            assert_eq!(
                m.explain("a/b/a2.pyc", true),
                r#"a/b/a2.pyc: ignored by rule *.pyc from .gitignore
a/b/a2.pyc: unignored by rule !a*.pyc from a/.gitignore (overrides previous rules)
"#
            );
            assert_eq!(m.explain("a/b/a2.py", true), "a/b/a2.py: not ignored\n");

            assert_eq!(
                m.explain("c/d/e/f", true),
                r#"c/d/e/f: ignored because c/d is ignored
c/d: ignored by rule d/ from .gitignore
"#
            );
            assert_eq!(
                m.explain("c/d", true),
                "c/d: ignored by rule d/ from .gitignore\n"
            );
            assert_eq!(m.explain("c/d", false), "c/d: not ignored\n");

            assert_eq!(
                m.explain("c/f/g/1/2", true),
                r#"c/f/g/1/2: ignored because c/f/g is ignored
c/f/g: ignored by rule g/ from .gitignore
c/f/g: unignored by rule !g/ from c/.gitignore (overrides previous rules)
c/f/g: ignored by rule g/ from c/f/.gitignore (overrides previous rules)
"#
            );
        }
        assert_eq!(m.explain("c/g/1/2", true), "c/g/1/2: not ignored\n");

        assert_eq!(m.explain("c/h/1", true), "c/h/1: not ignored\n");
    }

    fn write<P: Into<PathBuf>, C: AsRef<[u8]>>(path: P, contents: C) {
        File::create(path)
            .expect("create")
            .write_all(contents.as_ref())
            .expect("write");
    }

    #[test]
    fn test_case_insensitive() {
        let dir = tempdir().unwrap();
        write(dir.path().join(".gitignore"), b"FILE\nDIR/\n");

        let case_sensitive = [true, false];
        for sensitive in case_sensitive {
            let m = GitignoreMatcher::new(dir.path(), Vec::new(), sensitive);
            assert!(m.match_relative("x/FILE", false));
            assert_eq!(!sensitive, m.match_relative("x/file", false));
            assert!(m.match_relative("x/FILE", true));
            assert_eq!(!sensitive, m.match_relative("x/file", true));
            assert!(!m.match_relative("x/DIR", false));
            assert!(!m.match_relative("x/dir", false));
            assert!(m.match_relative("x/DIR", true));
            assert_eq!(!sensitive, m.match_relative("x/dir", true));
        }
    }
}
