//! Inline pixels keep source text available to copy and Find.

use std::ops::Range;
use std::sync::Arc;

use super::TextLayout;
use super::TextRow;
use crate::pets::ImageProtocol;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::transcript_image::TranscriptImage;

const MAX_LAYOUT_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct InlineImage {
    pub(super) start_row: usize,
    column: u16,
    image: Arc<TranscriptImage>,
    source: Range<usize>,
}

impl TextLayout {
    pub(in crate::transcript_view) fn with_markdown_images(
        mut self,
        protocol: ImageProtocol,
    ) -> Self {
        self.markdown_image_protocol = Some(protocol);
        let mut offset = 0;
        let mut images = Vec::new();
        let mut remaining = MAX_LAYOUT_IMAGE_BYTES.saturating_sub(self.image_bytes());
        for line in &self.logical {
            if let Some(copy) = &line.origin.copy {
                for (end, path) in &copy.images {
                    if *end <= line.origin.range.start || *end > line.origin.range.end {
                        continue;
                    }
                    let row = self.row_for_offset(offset + end - line.origin.range.start - 1);
                    let column = u16::try_from(line.subsequent_indent.width()).unwrap_or(u16::MAX);
                    let width = self
                        .width
                        .saturating_sub(column)
                        .saturating_sub(line.right_reserve);
                    if let Some(image) = TranscriptImage::load(path, width, protocol) {
                        if image.pixels.len() > remaining {
                            continue;
                        }
                        remaining -= image.pixels.len();
                        images.push((
                            row + 1,
                            column,
                            self.rows[row].source.end,
                            line.subsequent_indent.clone(),
                            image,
                        ));
                    }
                }
            }
            offset += line.origin.range.len() + 1;
        }
        // Insert from the bottom so references on the same row retain their source order.
        for (row, column, anchor, indent, image) in images.into_iter().rev() {
            let count = usize::from(image.rows);
            self.rows.splice(
                row..row,
                (0..count).map(|_| TextRow {
                    line: HyperlinkLine::new(indent.clone()),
                    source: anchor..anchor,
                    line_end: None,
                    content_width: self.width,
                    first_column: 0,
                    prefix_columns: 0,
                    tabs: super::tabs::TabStops::default(),
                }),
            );
            if let Some(control) = &mut self.disclosure_control
                && control.row >= row
            {
                control.row += count;
            }
            for previous in &mut self.inline_images {
                if previous.start_row >= row {
                    previous.start_row += count;
                }
            }
            self.inline_images.push(InlineImage {
                start_row: row,
                column,
                image,
                source: anchor..anchor,
            });
        }
        self.inline_images.sort_by_key(|image| image.start_row);
        self
    }

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
            if self.image_bytes() + image.pixels.len() > MAX_LAYOUT_IMAGE_BYTES {
                continue;
            }
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

    pub(in crate::transcript_view) fn without_inline_images(&self) -> Self {
        self.reflow(self.width, None, None)
    }

    pub(in crate::transcript_view) fn has_inline_images(&self) -> bool {
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
            .chain(
                self.visible_attached_images()
                    .scan(self.text_row_count(), |row, image| {
                        let start = *row;
                        *row += usize::from(image.rows);
                        Some((start, self.width.saturating_sub(image.columns) / 2, image))
                    }),
            )
    }

    pub(in crate::transcript_view) fn image_bytes(&self) -> usize {
        self.inline_images
            .iter()
            .map(|image| image.image.pixels.len())
            .sum::<usize>()
            + self
                .attached_images
                .iter()
                .map(|image| image.pixels.len())
                .sum::<usize>()
    }

    pub(in crate::transcript_view) fn add_attached_image(&mut self, image: Arc<TranscriptImage>) {
        // Several individually bounded tool images must not dwarf the layout cache together.
        if self.image_bytes() + image.pixels.len() <= MAX_LAYOUT_IMAGE_BYTES {
            self.attached_images.push(image);
        }
    }
}
