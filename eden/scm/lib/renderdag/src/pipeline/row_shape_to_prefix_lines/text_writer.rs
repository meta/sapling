/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::marker::PhantomData;

#[cfg(feature = "coloring")]
use super::strip_dec;
#[cfg(feature = "coloring")]
use crate::pipeline::coloring::GraphRowLineIds;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::LinkLine;
use crate::pipeline::types::PrefixLine;
use crate::pipeline::types::PrefixLineKind;
use crate::pipeline::types::PrefixLinePart;

pub(super) struct TextWriter<'a> {
    #[cfg(feature = "coloring")]
    line_ids: Option<&'a GraphRowLineIds>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> TextWriter<'a> {
    pub(super) fn new<N>(_row: &'a GraphRowShape<N>) -> Self {
        Self {
            #[cfg(feature = "coloring")]
            line_ids: _row.line_ids.as_deref(),
            _lifetime: PhantomData,
        }
    }

    pub(super) fn is_plain(&self) -> bool {
        #[cfg(feature = "coloring")]
        return self.line_ids.is_none();
        #[cfg(not(feature = "coloring"))]
        true
    }

    #[inline]
    pub(super) fn write_column(
        &self,
        _index: usize,
        _kind: PrefixLineKind,
        text: &str,
        out: &mut PrefixLine,
    ) {
        #[cfg(feature = "coloring")]
        if let Some(ids) = self.line_ids {
            let columns = match _kind {
                PrefixLineKind::PreNode | PrefixLineKind::Node | PrefixLineKind::PostNode => {
                    &ids.before_link
                }
                _ => &ids.after_link,
            };
            self.write_text(text, columns[_index].iter().next(), out);
            return;
        }
        self.write_plain(text, out);
    }

    pub(super) fn push_column(&self, line: &mut PrefixLine, index: usize, text: &str) {
        self.write_column(index, line.kind, text, line);
    }

    pub(super) fn write_plain(&self, text: &str, out: &mut PrefixLine) {
        self.append(
            text,
            #[cfg(feature = "coloring")]
            None,
            out,
        );
    }

    pub(super) fn write_junction(
        &self,
        index: usize,
        segments: LinkLine,
        text: &str,
        out: &mut PrefixLine,
    ) {
        self.write_links(&[(index, segments)], true, text, out);
    }

    pub(super) fn write_link(
        &self,
        index: usize,
        segments: LinkLine,
        text: &str,
        out: &mut PrefixLine,
    ) {
        self.write_links(&[(index, segments)], false, text, out);
    }

    fn write_links(
        &self,
        _parts: &[(usize, LinkLine)],
        _junction: bool,
        text: &str,
        out: &mut PrefixLine,
    ) {
        #[cfg(feature = "coloring")]
        if let Some(row_ids) = self.line_ids
            && let Some(cells) = &row_ids.link_line
        {
            // A terminal character has one foreground, even at an intersection.
            for &(index, segments) in _parts {
                let cell = &cells[index];
                if segments.intersects(LinkLine::VERTICAL)
                    && let Some(id) = cell.vertical.iter().next()
                {
                    self.write_text(text, Some(id), out);
                    return;
                }
            }
            // Otherwise, use the lowest ID of the drawn segments.
            let mut id = None;
            for &(index, segments) in _parts {
                let cell = &cells[index];
                for (flag, part) in [
                    (LinkLine::HORIZONTAL, &cell.horizontal),
                    (LinkLine::LEFT_FORK, &cell.left_fork),
                    (LinkLine::RIGHT_FORK, &cell.right_fork),
                    (LinkLine::LEFT_MERGE, &cell.left_merge),
                    (LinkLine::RIGHT_MERGE, &cell.right_merge),
                ] {
                    if segments.intersects(flag) && (!_junction || flag != LinkLine::HORIZONTAL) {
                        id = id.into_iter().chain(part.iter().next()).min();
                    }
                }
            }
            if _junction && id.is_none() {
                // Junctions prefer their local fork/merge IDs over a passing horizontal line.
                for &(index, segments) in _parts {
                    if segments.intersects(LinkLine::HORIZONTAL) {
                        let horizontal = cells[index].horizontal.iter().next();
                        id = id.into_iter().chain(horizontal).min();
                    }
                }
            }
            self.write_text(text, id, out);
            return;
        }
        self.write_plain(text, out);
    }

    #[cfg(feature = "coloring")]
    fn write_text(&self, text: &str, id: Option<usize>, out: &mut PrefixLine) {
        if id.is_none() {
            self.write_plain(text, out);
            return;
        }
        // Character-set switches and padding have no line identity.
        let (prefix, text, suffix) = strip_dec(text);
        let leading = text.len() - text.trim_start_matches(' ').len();
        let body = text[leading..].trim_end_matches(' ');
        self.write_plain(prefix, out);
        self.write_plain(&text[..leading], out);
        self.append(body, id, out);
        self.write_plain(&text[leading + body.len()..], out);
        self.write_plain(suffix, out);
    }

    #[inline]
    fn append(
        &self,
        text: &str,
        #[cfg(feature = "coloring")] id: Option<usize>,
        out: &mut PrefixLine,
    ) {
        if text.is_empty() {
            return;
        }
        if let Some((PrefixLinePart::Text(previous), _previous_id)) = out.parts.last_mut() {
            #[cfg(feature = "coloring")]
            let same_id = *_previous_id == id;
            #[cfg(not(feature = "coloring"))]
            let same_id = true;
            if same_id {
                previous.push_str(text);
                return;
            }
        }
        #[cfg(not(feature = "coloring"))]
        let id = ();
        let mut owned = String::new();
        owned.push_str(text);
        out.parts.push((PrefixLinePart::Text(owned), id));
    }
}
