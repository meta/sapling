/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::Ordering;
use std::str;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use crate::BonsaiChangeset;
use crate::NonRootMPath;

/// Extra carrying the depot revision of each file a changelist touched, one
/// `<rev>\t<repo path>\n` line per file, sorted by repo path (`NonRootMPath` order).
/// Deletions are revisions too. The importer (`p4_transform`) writes it;
/// [`P4FileRevs`] reads it back.
pub const P4_FILE_REVS_EXTRA: &str = "p4-file-revs";

/// The depot revision each file of a changeset's changelist has in that changelist, as
/// recorded in its [`P4_FILE_REVS_EXTRA`].
///
/// Holds the extra's value as written: a lookup binary-searches its sorted lines, so a
/// changelist touching a great many files costs nothing to hold and `O(log n)` to query.
/// The value is only checked where a lookup reads it; [`Self::iter`] reads it all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct P4FileRevs(Vec<u8>);

impl P4FileRevs {
    /// The revisions among a changeset's hg extras, or `None` if the extra is absent: the
    /// changeset is not a Perforce import, or predates the extra.
    pub fn from_hg_extras<'a>(
        extras: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Option<Self> {
        extras
            .into_iter()
            .find_map(|(key, value)| (key == P4_FILE_REVS_EXTRA).then(|| Self(value.to_vec())))
    }

    /// [`Self::from_hg_extras`] for a loaded changeset.
    pub fn from_bcs(bcs: &BonsaiChangeset) -> Option<Self> {
        Self::from_hg_extras(bcs.hg_extra())
    }

    /// The value as the importer wrote it.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Whether the changelist touched no file.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The depot revision `path` has in this changelist, if it touched `path`. Errors if
    /// a line the search reads is malformed.
    pub fn revision(&self, path: &NonRootMPath) -> Result<Option<u32>> {
        if self.is_empty() {
            return Ok(None);
        }
        if self.0.last() != Some(&b'\n') {
            bail!("malformed {P4_FILE_REVS_EXTRA} extra: missing trailing newline");
        }
        // `low` is always the start of a line and `high` the end of one (just past its
        // newline), so the line around `mid` lies within them.
        let (mut low, mut high) = (0, self.0.len());
        while low < high {
            let mid = low + (high - low) / 2;
            let start = self.0[low..mid]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(low, |newline| low + newline + 1);
            let end = self.0[mid..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(high, |newline| mid + newline + 1);
            let (line_path, rev) = self.parse_line(start, end)?;
            match line_path.cmp(path) {
                Ordering::Equal => return Ok(Some(rev)),
                Ordering::Less => low = end,
                Ordering::Greater => high = start,
            }
        }
        Ok(None)
    }

    /// Every file's revision, in path order; an error for a malformed line.
    pub fn iter(&self) -> impl Iterator<Item = Result<(NonRootMPath, u32)>> + '_ {
        let has_trailing_newline = self.0.last() == Some(&b'\n');
        let mut start = 0;
        std::iter::from_fn(move || {
            if start >= self.0.len() {
                return None;
            }
            if !has_trailing_newline {
                start = self.0.len();
                return Some(Err(anyhow!(
                    "malformed {P4_FILE_REVS_EXTRA} extra: missing trailing newline"
                )));
            }
            let end = self.0[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(self.0.len(), |newline| start + newline + 1);
            let line = self.parse_line(start, end);
            start = end;
            Some(line)
        })
    }

    /// The line at `start..end` (including its newline), naming it on error.
    fn parse_line(&self, start: usize, end: usize) -> Result<(NonRootMPath, u32)> {
        let line = self.0[start..end]
            .strip_suffix(b"\n")
            .unwrap_or(&self.0[start..end]);
        parse_line(line).map_err(|error| {
            let number = self.0[..start]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1;
            error.context(format!(
                "malformed {P4_FILE_REVS_EXTRA} extra, line {number}"
            ))
        })
    }
}

impl FromIterator<(NonRootMPath, u32)> for P4FileRevs {
    /// Writes the lines the importer would for these revisions. They must be in path order.
    fn from_iter<I: IntoIterator<Item = (NonRootMPath, u32)>>(iter: I) -> Self {
        let mut value = Vec::new();
        for (path, rev) in iter {
            value.extend_from_slice(rev.to_string().as_bytes());
            value.push(b'\t');
            value.extend_from_slice(&path.to_vec());
            value.push(b'\n');
        }
        Self(value)
    }
}

/// One `<rev>\t<repo path>` line, without its newline. Only the first tab separates: a
/// path may contain tabs.
fn parse_line(line: &[u8]) -> Result<(NonRootMPath, u32)> {
    let lossy = || String::from_utf8_lossy(line).into_owned();
    let tab = line
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| anyhow!("{:?} has no tab", lossy()))?;
    let (rev, path) = (&line[..tab], &line[tab + 1..]);
    let rev = str::from_utf8(rev)
        .ok()
        .and_then(|rev| rev.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("{:?} has no revision number", lossy()))?;
    if rev == 0 {
        bail!(
            "{:?} has revision 0, which Perforce never allocates",
            lossy()
        );
    }
    let path = NonRootMPath::new(path)
        .map_err(|error| error.context(format!("{:?} has an invalid path", lossy())))?;
    Ok((path, rev))
}

#[cfg(test)]
mod tests;
