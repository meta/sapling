/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub mod ascii;
pub mod ascii_large;
pub mod box_drawing;
mod text_writer;

use self::text_writer::TextWriter;
use crate::pipeline::types::DashStyle;
use crate::pipeline::types::GraphRowShape;
use crate::pipeline::types::PadLine;
use crate::pipeline::types::PrefixLineKind;

fn ancestry_lines<N>(
    row: &GraphRowShape<N>,
) -> impl Iterator<Item = (PrefixLineKind, impl Iterator<Item = PadLine> + '_)> + '_ {
    [
        row.has_indirect_ancestor()
            .then_some(PrefixLineKind::Ancestry),
        Some(PrefixLineKind::PostAncestry),
    ]
    .into_iter()
    .flatten()
    .map(move |kind| {
        let solid = kind == PrefixLineKind::PostAncestry && row.style.dash_style == DashStyle::Once;
        let columns = row.ancestry_line.iter().copied().map(move |pad| {
            if solid && pad == PadLine::Ancestor {
                PadLine::Parent
            } else {
                pad
            }
        });
        (kind, columns)
    })
}

// DEC switches enclose graph text but do not belong to a drawable stroke.
pub(super) fn strip_dec(text: &str) -> (&str, &str, &str) {
    match text
        .strip_prefix("\x1b(0")
        .and_then(|text| text.strip_suffix("\x1b(B"))
    {
        Some(text) => ("\x1b(0", text, "\x1b(B"),
        None => ("", text, ""),
    }
}

fn split_glyph(text: &str) -> (&str, &str, &str, &str) {
    let (prefix, text, suffix) = strip_dec(text);
    // Spacing zero leaves only the column character.
    let split = text
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| index);
    let (column, fill) = text.split_at(split);
    (prefix, column, fill, suffix)
}
