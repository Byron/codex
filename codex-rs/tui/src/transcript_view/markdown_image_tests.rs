//! Local image artifacts retain accessible text, selection, and viewport behavior.

use super::*;
use crate::clipboard_copy::CopyFormat;
use crate::clipboard_copy::CopyStatus;
use crate::history_cell::AgentMarkdownCell;
use crate::pets::ImageProtocol;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use pretty_assertions::assert_eq;
use std::path::Path;

fn image_files() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    image::RgbaImage::from_pixel(400, 240, image::Rgba([220, 30, 20, 255]))
        .save(dir.path().join("plot.png"))
        .unwrap();
    std::fs::write(
        dir.path().join("diagram.svg"),
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="240"><rect width="600" height="240" fill="#143cdc"/></svg>"##,
    )
    .unwrap();
    dir
}

fn cell(markdown: &str, cwd: &Path) -> Arc<dyn HistoryCell> {
    Arc::new(AgentMarkdownCell::new(markdown.into(), cwd))
}

#[test]
fn markdown_images_keep_png_svg_alt_text_and_surrounding_prose() {
    let dir = image_files();
    let cells = vec![cell(
        "Before.\n\n![Revenue chart](plot.png)\n\nBetween.\n\n![State diagram](diagram.svg)\n\nAfter.",
        dir.path(),
    )];
    let mut view = TranscriptView::default();
    let fallback = render(&mut view, &cells, 100, 100);
    let source = view.layout(&cells, 0).unwrap().text().to_owned();
    assert!(view.image_placements().is_empty());

    view.image_protocol = Some(ImageProtocol::Kitty);
    let graphical = render(&mut view, &cells, 100, 100);
    assert_eq!(view.layout(&cells, 0).unwrap().text(), source);
    let placements = view.image_placements();
    assert_eq!(placements.len(), 2);
    let displayed = text(&graphical);
    let row = |label| {
        displayed
            .lines()
            .position(|line| line.contains(label))
            .unwrap() as u16
    };
    assert!(row("Before.") < row("Revenue chart"));
    assert!(row("Revenue chart") < placements[0].area.y);
    assert!(placements[0].area.bottom() <= row("Between."));
    assert!(row("Between.") < row("State diagram"));
    assert!(row("State diagram") < placements[1].area.y);
    assert!(placements[1].area.bottom() <= row("After."));

    render(&mut view, &cells, 100, 100);
    for (previous, current) in placements.iter().zip(view.image_placements()) {
        assert!(Arc::ptr_eq(&previous.image, &current.image));
        assert_eq!(previous.area, current.area);
    }
    view.image_protocol = None;
    assert_eq!(render(&mut view, &cells, 100, 100), fallback);
    assert!(view.image_placements().is_empty());
}

#[test]
fn markdown_images_keep_multiple_references_in_source_order() {
    let dir = image_files();
    let cells = vec![cell(
        "![First](plot.png) ![Second](diagram.svg) ![](plot.png)",
        dir.path(),
    )];
    let mut view = TranscriptView::default();
    view.image_protocol = Some(ImageProtocol::Kitty);
    let buffer = render(&mut view, &cells, 100, 100);
    assert!(text(&buffer).contains("First Second Image"));
    let placements = view.image_placements();
    assert_eq!(placements.len(), 3);
    assert!(placements[0].area.bottom() <= placements[1].area.y);
    assert!(placements[1].area.bottom() <= placements[2].area.y);
    assert_eq!(
        placements[0].image.pixels.get_pixel(0, 0).0,
        [220, 30, 20, 255]
    );
    assert_eq!(
        placements[1].image.pixels.get_pixel(0, 0).0,
        [20, 60, 220, 255]
    );
    assert!(Arc::ptr_eq(&placements[0].image, &placements[2].image));
}

#[test]
fn markdown_images_preserve_wrapped_source_offsets_and_nested_indentation() {
    let dir = image_files();
    let markdown = "Before the chart.\n\n> - ![A long label with Unicode 数据](plot.png) followed by prose.\n\nAfter the chart.";
    let mut lines = cell(markdown, dir.path()).transcript_hyperlink_lines(24);
    for line in &mut lines {
        if let Some(source) = &mut line.source {
            source.right_reserve = 3;
        }
    }
    let fallback = TextLayout::new(lines, 24);
    let layout = fallback
        .rewrap(24)
        .with_markdown_images(ImageProtocol::Kitty);
    let images = layout.images().collect::<Vec<_>>();
    assert_eq!(images.len(), 1);
    let (row, column, image) = images[0];
    let label_end = layout.text().find("数据").unwrap() + "数据".len();
    assert_eq!(row, layout.row_for_offset(label_end - 1) + 1);
    assert!(column >= 4);
    assert!(column + image.columns <= 21);
    assert_eq!(layout.text(), fallback.text());
    assert_eq!(
        layout.row_count(),
        fallback.row_count() + usize::from(image.rows)
    );

    let narrow = layout.rewrap(2);
    assert!(narrow.images().next().is_none());
    assert_eq!(narrow.text(), layout.text());
    let restored = narrow.rewrap(24);
    assert_eq!(restored.row_count(), layout.row_count());
    assert_eq!(restored.images().next().unwrap().0, row);
}

#[test]
fn markdown_images_follow_live_completion_and_history_handoff() {
    let dir = image_files();
    let pending = "![Live chart](plot.png";
    let completed = "![Live chart](plot.png)";
    let mut view = TranscriptView::default();
    view.image_protocol = Some(ImageProtocol::Kitty);
    let mut previous_image = None;
    for (revision, markdown, expected_images) in [
        (0, pending, 0),
        (1, completed, 1),
        (2, "![Live chart](plot.png) with a caption.", 1),
    ] {
        let key = Some(ActiveCellTranscriptKey {
            cacheable: true,
            revision,
            is_stream_continuation: false,
            animation_tick: None,
        });
        assert!(view.sync_live_tail(80, key, |width| {
            Some(
                crate::markdown_render::render_streaming_markdown_lines_with_width_and_cwd(
                    markdown,
                    Some(usize::from(width)),
                    Some(dir.path()),
                    &|_| false,
                    crate::markdown_render::ListSpacing::AfterMultiline,
                )
                .lines,
            )
        }));
        render(&mut view, &[], 80, 50);
        let placements = view.image_placements();
        assert_eq!(placements.len(), expected_images);
        if let Some(placement) = placements.first() {
            if let Some(previous) = previous_image {
                assert!(Arc::ptr_eq(&previous, &placement.image));
            }
            previous_image = Some(Arc::clone(&placement.image));
        }
        assert!(!view.sync_live_tail(80, key, |_| {
            panic!("an unchanged live key must reuse its image layout")
        }));
    }
    assert!(view.sync_live_tail(80, None, |_| None));
    let cells = vec![cell(completed, dir.path())];
    let buffer = render(&mut view, &cells, 80, 50);
    assert_eq!(view.image_placements().len(), 1);
    assert!(text(&buffer).contains("Live chart"));
}

#[test]
fn markdown_images_preserve_raw_and_missing_file_fallbacks() {
    let dir = image_files();
    let cells = vec![cell(
        "![Present](plot.png)\n\n![Missing](missing.png)",
        dir.path(),
    )];
    let mut view = TranscriptView::default();
    let fallback = render(&mut view, &cells, 80, 50);
    view.image_protocol = Some(ImageProtocol::Kitty);
    let displayed = render(&mut view, &cells, 80, 50);
    assert_eq!(view.image_placements().len(), 1);
    assert!(text(&displayed).contains("Missing"));
    view.set_presentation(false, HistoryRenderMode::Raw);
    let raw = render(&mut view, &cells, 80, 50);
    assert!(view.image_placements().is_empty());
    assert!(text(&raw).contains("![Present](plot.png)"));
    std::fs::remove_file(dir.path().join("plot.png")).unwrap();
    view.set_presentation(false, HistoryRenderMode::Rich);
    assert_eq!(render(&mut view, &cells, 80, 50), fallback);
    assert!(view.image_placements().is_empty());
}

#[test]
fn markdown_images_resize_and_clip_inside_inset_viewports() {
    let dir = image_files();
    image::RgbaImage::from_pixel(400, 800, image::Rgba([220, 30, 20, 255]))
        .save(dir.path().join("plot.png"))
        .unwrap();
    let cells = vec![cell("![Tall chart](plot.png)", dir.path())];
    let mut view = TranscriptView::default();
    view.image_protocol = Some(ImageProtocol::Kitty);
    render(&mut view, &cells, 100, 100);
    let original = view.image_placements();
    assert_eq!(original.len(), 1);
    render(&mut view, &cells, 2, 100);
    assert!(view.image_placements().is_empty());
    render(&mut view, &cells, 100, 100);
    assert_eq!(view.image_placements()[0].area, original[0].area);

    let short = Rect::new(7, 5, 100, 3);
    view.jump_to_latest();
    view.render(short, &mut Buffer::empty(short), &cells);
    let clipped = view.image_placements();
    assert_eq!(clipped.len(), 1);
    let placement = &clipped[0];
    assert!(placement.first_row > 1);
    assert!(short.contains(placement.area.as_position()));
    assert!(placement.area.right() <= short.right());
    assert!(placement.area.bottom() <= short.bottom());
    view.scroll(&cells, -1);
    view.render(short, &mut Buffer::empty(short), &cells);
    assert_eq!(
        view.image_placements()[0].first_row + 1,
        placement.first_row
    );
}

#[test]
fn markdown_images_keep_alt_text_in_copy_and_find() {
    let dir = image_files();
    let cells = vec![cell(
        "Before.\n\n![Revenue **2026**](plot.png)\n\nAfter.",
        dir.path(),
    )];
    let mut view = TranscriptView::default();
    view.image_protocol = Some(ImageProtocol::Kitty);
    render(&mut view, &cells, 80, 50);
    assert_eq!(view.image_placements().len(), 1);
    view.begin_selection(&cells, 2, 0, 1);
    view.extend_selection(79, 49);
    let plain = view.selected_text(&cells).unwrap();
    assert!(plain.contains("Revenue 2026"));
    view.copy_selected_text_with(&cells, &plain, true, |copy, format| {
        let CopyFormat::MarkdownSelection(source) = format else {
            panic!("expected rich selection");
        };
        assert_eq!(copy, plain);
        assert!(source.contains("Revenue **2026**"));
        assert!(copy.contains("Before."));
        assert!(copy.contains("After."));
        assert!(!copy.contains("plot.png"));
        Ok(CopyStatus::Confirmed)
    })
    .unwrap();

    view.begin_search();
    assert!(view.paste_search("Revenue"));
    assert!(!view.advance_search(&cells));
    let found = render(&mut view, &cells, 80, 50);
    assert!(text(&found).contains("Revenue 2026"));
    assert!(view.image_placements().is_empty());
    view.cancel_search();
    render(&mut view, &cells, 80, 50);
    assert_eq!(view.image_placements().len(), 1);
}
