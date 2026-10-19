//! Diagram pixels replace display rows while original source remains available to copy and Find.

use std::ops::Range;
use std::sync::Arc;

use super::TextLayout;
use super::TextRow;
use crate::pets::ImageProtocol;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::transcript_image::TranscriptImage;

pub(super) struct InlineImage {
    pub(super) start_row: usize,
    column: u16,
    image: Arc<TranscriptImage>,
    source: Range<usize>,
}

impl TextLayout {
    pub(in crate::transcript_view) fn with_mermaid_images(
        mut self,
        protocol: ImageProtocol,
    ) -> Self {
        self.mermaid_protocol = Some(protocol);
        let mut offset = 0;
        let mut index = 0;
        while index < self.logical.len() {
            let line = &self.logical[index];
            let source = line
                .origin
                .copy
                .as_ref()
                .filter(|copy| copy.mermaid)
                .and_then(|copy| copy.code_source.clone());
            let Some(source) = source else {
                offset += line.origin.range.len() + 1;
                index += 1;
                continue;
            };
            let start = offset;
            let initial_indent = line.initial_indent.clone();
            let subsequent_indent = line.subsequent_indent.clone();
            let column = line
                .initial_indent
                .width()
                .max(line.subsequent_indent.width());
            let columns = self.width.saturating_sub(
                u16::try_from(column)
                    .unwrap_or(u16::MAX)
                    .saturating_add(line.right_reserve),
            );
            loop {
                offset += self.logical[index].origin.range.len() + 1;
                index += 1;
                if !self.logical.get(index).is_some_and(|line| {
                    line.origin.copy.as_ref().is_some_and(|copy| {
                        copy.mermaid
                            && copy
                                .code_source
                                .as_ref()
                                .is_some_and(|next| Arc::ptr_eq(&source, next))
                    })
                }) {
                    break;
                }
            }
            let Some(image) = crate::mermaid_image::render(&source, columns, protocol) else {
                continue;
            };
            let first = self.rows.partition_point(|row| row.source.start < start);
            let mut matching_rows = self
                .rows
                .iter()
                .enumerate()
                .skip(first)
                .take_while(|(_, row)| row.source.start < offset)
                .filter(|(_, row)| row.line_end.is_some() || !row.source.is_empty());
            let Some((first, _)) = matching_rows.next() else {
                continue;
            };
            let end = matching_rows.last().map_or(first + 1, |(last, _)| last + 1);
            let count = usize::from(image.rows);
            let source = start..offset.saturating_sub(1).min(self.text.len());
            self.rows.splice(
                first..end,
                (0..count).map(|row| {
                    // Anchor scrolling within the retained block without inventing selectable glyphs.
                    let anchor = self.text.floor_char_boundary(
                        start
                            + source.len().saturating_sub(1) * row / count.saturating_sub(1).max(1),
                    );
                    TextRow {
                        line: HyperlinkLine::new(if row == 0 {
                            initial_indent.clone()
                        } else {
                            subsequent_indent.clone()
                        }),
                        source: anchor..anchor,
                        line_end: None,
                        content_width: self.width,
                        first_column: 0,
                        prefix_columns: 0,
                        tabs: super::tabs::TabStops::default(),
                    }
                }),
            );
            if let Some(control) = &mut self.disclosure_control
                && control.row >= end
            {
                control.row = control.row - (end - first) + count;
            }
            self.inline_images.push(InlineImage {
                start_row: first,
                column: column as u16,
                image,
                source,
            });
        }
        self
    }

    pub(in crate::transcript_view) fn without_mermaid_images(&self) -> Self {
        self.reflow(self.width, None)
    }

    pub(in crate::transcript_view) fn has_mermaid_images(&self) -> bool {
        !self.inline_images.is_empty()
    }

    pub(super) fn image_row_for_offset(&self, offset: usize) -> Option<usize> {
        let image = self
            .inline_images
            .iter()
            .find(|image| image.source.contains(&offset))?;
        Some(
            image.start_row
                + (offset - image.source.start) * usize::from(image.image.rows - 1)
                    / image.source.len().saturating_sub(1).max(1),
        )
    }

    pub(in crate::transcript_view) fn images(
        &self,
    ) -> impl Iterator<Item = (usize, u16, &Arc<TranscriptImage>)> {
        self.inline_images
            .iter()
            .map(|image| (image.start_row, image.column, &image.image))
            .chain(self.visible_image().map(|image| {
                (
                    self.text_row_count(),
                    self.width.saturating_sub(image.columns) / 2,
                    image,
                )
            }))
    }

    pub(in crate::transcript_view) fn image_bytes(&self) -> usize {
        self.inline_images
            .iter()
            .map(|image| image.image.pixels.len())
            .sum::<usize>()
            + self.image.as_ref().map_or(0, |image| image.pixels.len())
    }
}
