//! Mermaid images preserve transcript source, navigation, and text fallbacks.

use super::*;
use crate::clipboard_copy::CopyStatus;
use crate::history_cell::AgentMarkdownCell;
use crate::pets::ImageProtocol;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;
use std::path::Path;

const CODE: &str = "flowchart LR\nA[Request] --> B[Reply]\n";

fn cell(markdown: &str) -> Arc<dyn HistoryCell> {
    Arc::new(AgentMarkdownCell::new(markdown.into(), Path::new("/")))
}

#[test]
fn mermaid_images_stay_between_paragraphs_and_follow_presentation() {
    let markdown = format!(
        "Before.\n\n```mermaid\n{CODE}```\n\nBetween.\n\n```mermaid\n{CODE}```\n\nAfter.\n"
    );
    let cells = vec![cell(&markdown)];
    let mut view = TranscriptView::default();
    let fallback = render(&mut view, &cells, /*width*/ 100, /*height*/ 70);
    assert!(view.image_placements().is_empty());
    view.set_mermaid_image_protocol(Some(ImageProtocol::Kitty));
    let pixels = render(&mut view, &cells, /*width*/ 100, /*height*/ 70);
    let placements = view.image_placements();
    assert_eq!(placements.len(), 2);
    let displayed = text(&pixels);
    let row = |label| {
        displayed
            .lines()
            .position(|line| line.contains(label))
            .unwrap() as u16
    };
    assert!(row("Before.") < placements[0].area.y);
    assert!(placements[0].area.bottom() <= row("Between."));
    assert!(row("Between.") < placements[1].area.y);
    assert!(placements[1].area.bottom() <= row("After."));
    assert!(!displayed.contains("Request"));
    assert_eq!(
        view.layout(&cells, 0)
            .unwrap()
            .copy_block_ranges()
            .into_iter()
            .map(|(_, _, source)| source.unwrap())
            .collect::<Vec<_>>(),
        [CODE, CODE],
    );
    render(&mut view, &cells, /*width*/ 100, /*height*/ 70);
    let reused = view.image_placements();
    assert!(Arc::ptr_eq(&placements[0].image, &reused[0].image));
    assert!(Arc::ptr_eq(&placements[1].image, &reused[1].image));

    for protocol in [None, Some(ImageProtocol::Sixel)] {
        view.set_mermaid_image_protocol(protocol);
        assert_eq!(
            render(&mut view, &cells, /*width*/ 100, /*height*/ 70),
            fallback,
        );
        assert!(view.image_placements().is_empty());
    }
    view.set_mermaid_image_protocol(Some(ImageProtocol::Kitty));
    view.set_presentation(/*detailed*/ false, HistoryRenderMode::Raw);
    let raw = render(&mut view, &cells, /*width*/ 100, /*height*/ 70);
    assert!(view.image_placements().is_empty());
    assert!(text(&raw).contains("```mermaid"));
}

#[test]
fn mermaid_images_copy_source_and_reveal_find_results() {
    let markdown = format!("```mermaid\n{CODE}```\n");
    let cells = vec![cell(&markdown)];
    let mut view = TranscriptView::default();
    view.set_mermaid_image_protocol(Some(ImageProtocol::Kitty));
    render(&mut view, &cells, /*width*/ 80, /*height*/ 30);
    assert_eq!(view.image_placements().len(), 1);
    view.begin_copy_mode(&cells, /*guard*/ None);
    view.handle_key(KeyCode::Up.into(), &cells);
    let selected = view.selected_text(&cells).unwrap();
    let mut copied = String::new();
    view.copy_selected_text_with(
        &cells,
        &selected,
        /*clear_selection*/ false,
        |source, _| {
            copied = source.to_owned();
            Ok(CopyStatus::Confirmed)
        },
    )
    .unwrap();
    assert_eq!(copied, CODE);
    view.handle_key(KeyCode::Esc.into(), &cells);

    view.begin_search();
    assert!(view.paste_search("Request"));
    assert!(!view.advance_search(&cells));
    let found = render(&mut view, &cells, /*width*/ 80, /*height*/ 30);
    assert!(text(&found).contains("Request"));
    assert!(view.image_placements().is_empty());
    view.cancel_search();
    let restored = render(&mut view, &cells, /*width*/ 80, /*height*/ 30);
    assert!(!text(&restored).contains("Request"));
    assert_eq!(view.image_placements().len(), 1);
}

#[test]
fn mermaid_images_respect_nested_indentation_and_reserved_columns() {
    for markdown in [
        "> ```mermaid\n> flowchart LR\n> A[Request] --> B[Reply]\n> ```\n",
        "- Diagram:\n\n  ```mermaid\n  flowchart LR\n  A[Request] --> B[Reply]\n  ```\n",
        "- ```mermaid\n  flowchart LR\n  A[Request] --> B[Reply]\n  ```\n",
    ] {
        let mut lines = cell(markdown).transcript_hyperlink_lines(/*width*/ 52);
        for line in &mut lines {
            if let Some(source) = &mut line.source {
                source.right_reserve = 8;
            }
        }
        let fallback = TextLayout::new(lines, /*width*/ 52);
        let layout = fallback
            .rewrap(52)
            .with_mermaid_images(ImageProtocol::Kitty);
        let images = layout.images().collect::<Vec<_>>();
        assert_eq!(images.len(), 1);
        let (start_row, column, image) = images[0];
        assert!(column >= 4);
        assert!(column + image.columns <= 44);
        assert_eq!(layout.copy_block_ranges()[0].2.as_deref(), Some(CODE));
        let area = Rect::new(0, 0, 52, 1);
        let mut original = Buffer::empty(area);
        let mut graphical = Buffer::empty(area);
        fallback.render(
            area,
            &mut original,
            fallback.row_for_offset(layout.position_at(start_row, 0)),
        );
        layout.render(area, &mut graphical, start_row);
        for x in 0..column {
            assert_eq!(graphical[(x, 0)], original[(x, 0)]);
        }
    }
}

#[test]
fn mermaid_images_wait_for_live_closer_and_survive_history_handoff() {
    let pending = format!("```mermaid\n{CODE}");
    let completed = format!("{pending}```\n");
    let mut view = TranscriptView::default();
    view.set_mermaid_image_protocol(Some(ImageProtocol::Kitty));
    for (revision, markdown, expected_images) in [(0, &pending, 0), (1, &completed, 1)] {
        let key = Some(ActiveCellTranscriptKey {
            cacheable: true,
            revision,
            is_stream_continuation: false,
            animation_tick: None,
        });
        assert!(view.sync_live_tail(/*width*/ 80, key, |width| {
            Some(
                crate::markdown_render::render_streaming_markdown_lines_with_width_and_cwd(
                    markdown,
                    Some(usize::from(width)),
                    Some(Path::new("/")),
                    &|_| false,
                    crate::markdown_render::ListSpacing::AfterMultiline,
                )
                .lines,
            )
        }));
        render(&mut view, &[], /*width*/ 80, /*height*/ 30);
        assert_eq!(view.image_placements().len(), expected_images);
        assert!(!view.sync_live_tail(/*width*/ 80, key, |_| {
            panic!("an unchanged live key must reuse its image layout")
        }));
    }
    assert!(view.sync_live_tail(/*width*/ 80, None, |_| None));
    let cells = vec![cell(&completed)];
    render(&mut view, &cells, /*width*/ 80, /*height*/ 30);
    assert_eq!(view.image_placements().len(), 1);
    assert_eq!(
        view.layout(&cells, 0).unwrap().copy_block_ranges()[0]
            .2
            .as_deref(),
        Some(CODE),
    );
}

#[test]
fn mermaid_images_rewrap_and_clip_while_scrolling() {
    let cells = vec![cell(
        "```mermaid\nflowchart TD\nA[Request] --> B[Reply] --> C[Complete]\n```\n",
    )];
    let mut view = TranscriptView::default();
    view.set_mermaid_image_protocol(Some(ImageProtocol::Kitty));
    render(&mut view, &cells, /*width*/ 100, /*height*/ 50);
    let placements = view.image_placements();
    assert_eq!(placements.len(), 1);
    let layout = view.layout(&cells, 0).unwrap();
    let source = &layout.copy_block_ranges()[0].0;
    let (first_image_row, _, image) = layout.images().next().unwrap();
    assert_eq!(layout.row_for_offset(source.start), first_image_row);
    assert_eq!(
        layout.row_for_offset(source.end - 1),
        first_image_row + usize::from(image.rows) - 1,
    );
    let narrow = layout.rewrap(/*width*/ 4);
    assert_eq!(narrow.text(), layout.text());
    assert_eq!(narrow.copy_block_ranges(), layout.copy_block_ranges());
    assert_eq!(narrow.rewrap(/*width*/ 100).row_count(), layout.row_count());
    render(&mut view, &cells, /*width*/ 4, /*height*/ 50);
    assert!(view.image_placements().is_empty());
    render(&mut view, &cells, /*width*/ 100, /*height*/ 50);
    assert_eq!(view.image_placements()[0].area, placements[0].area);

    let short = Rect::new(
        /*x*/ 7, /*y*/ 5, /*width*/ 100, /*height*/ 3,
    );
    view.jump_to_latest();
    view.render(short, &mut Buffer::empty(short), &cells);
    let clipped = view.image_placements();
    assert_eq!(clipped.len(), 1);
    let placement = &clipped[0];
    assert!(Arc::ptr_eq(&placements[0].image, &placement.image));
    assert!(placement.first_row > 1);
    assert!(short.contains(placement.area.as_position()));
    assert!(placement.area.right() <= short.right());
    assert!(placement.area.bottom() <= short.bottom());
    view.scroll(&cells, -1);
    view.render(short, &mut Buffer::empty(short), &cells);
    assert_eq!(
        view.image_placements()[0].first_row + 1,
        placement.first_row,
    );
    assert!(Arc::ptr_eq(
        &placement.image,
        &view.image_placements()[0].image,
    ));
}
