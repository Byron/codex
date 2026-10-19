//! Tool-returned pixels share disclosure, source selection, and viewport clipping.

use super::*;
use crate::clipboard_copy::CopyStatus;
use crate::history_cell::McpInvocation;
use crate::history_cell::new_active_mcp_tool_call;
use crate::pets::ImageProtocol;
use crate::transcript_view::tests::render;
use crate::transcript_view::tests::text;
use base64::Engine;
use codex_protocol::mcp::CallToolResult;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Cursor;
use std::time::Duration;

fn image_call() -> Arc<dyn HistoryCell> {
    Arc::new(image_call_for("charts"))
}

fn image_call_for(server: &str) -> crate::history_cell::McpToolCallCell {
    let mut png = Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(320, 160, image::Rgba([255, 0, 0, 255]))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="320" height="160"><rect width="320" height="160" fill="green"/></svg>"#;
    let mut call = new_active_mcp_tool_call(
        "plots".into(),
        McpInvocation {
            server: server.into(),
            tool: "render".into(),
            arguments: None,
        },
        /*animations_enabled*/ false,
    );
    call.complete(
        Duration::ZERO,
        Ok(CallToolResult {
            content: vec![
                json!({"type": "text", "text": "Chart title\nData loaded\nAxes prepared\nneedle legend\nComplete"}),
                json!({"type": "image", "mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png.into_inner())}),
                json!({"type": "image", "mimeType": "image/svg+xml", "data": base64::engine::general_purpose::STANDARD.encode(svg)}),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        }),
    );
    call
}

#[test]
fn tool_images_follow_disclosure_and_work_without_local_file_access() {
    let cells = vec![image_call()];
    let mut view = TranscriptView::default();
    let collapsed = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    view.embedded_image_protocol = Some(ImageProtocol::Kitty);
    assert_eq!(view.image_protocol, None);
    assert_eq!(
        render(&mut view, &cells, /*width*/ 80, /*height*/ 40),
        collapsed,
    );
    assert!(view.image_placements().is_empty());
    assert!(
        view.entry_layout(&cells, /*index*/ 0, /*full_content*/ true)
            .unwrap()
            .attached_images
            .is_empty()
    );

    let row = view
        .visible
        .iter()
        .position(|row| row.layout.disclosure_row() == Some(row.row))
        .unwrap() as u16;
    assert!(view.toggle_disclosure_at(&cells, /*column*/ 4, row));
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    let placements = view.image_placements();
    assert_eq!(placements.len(), 2);
    assert!(placements[0].area.bottom() <= placements[1].area.top());
    assert_eq!(
        placements[0].image.pixels.get_pixel(0, 0).0,
        [255, 0, 0, 255]
    );
    assert_eq!(
        placements[1].image.pixels.get_pixel(0, 0).0,
        [0, 128, 0, 255]
    );
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    let reused = view.image_placements();
    assert!(Arc::ptr_eq(&placements[0].image, &reused[0].image));
    assert!(Arc::ptr_eq(&placements[1].image, &reused[1].image));

    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert_eq!(view.image_placements().len(), 2);
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Raw);
    let raw = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert!(view.image_placements().is_empty());
    assert_eq!(text(&raw).matches("Returned image").count(), 2);

    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    view.embedded_image_protocol = None;
    let fallback = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert!(view.image_placements().is_empty());
    assert_eq!(text(&fallback).matches("Returned image").count(), 2);
}

#[test]
fn tool_images_preserve_copy_and_find_without_expanding_graphics_for_search() {
    let cells = vec![image_call()];
    let mut view = TranscriptView::default();
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    let source = view.layout(&cells, 0).unwrap().text().to_owned();
    view.embedded_image_protocol = Some(ImageProtocol::Kitty);
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert_eq!(view.layout(&cells, 0).unwrap().text(), source);
    assert_eq!(view.image_placements().len(), 2);
    view.select_copy_range(&cells, /*index*/ 0, 0..source.len());
    let selected = view.selected_text(&cells).unwrap();
    assert_eq!(selected, source);
    let mut copied = String::new();
    view.copy_selected_text_with(
        &cells,
        &selected,
        /*clear_selection*/ false,
        |text, _| {
            copied = text.to_owned();
            Ok(CopyStatus::Confirmed)
        },
    )
    .unwrap();
    assert_eq!(copied, source);
    assert_eq!(copied.matches("Returned image").count(), 2);

    view.jump_to_latest();
    view.set_presentation(/*detailed*/ false, HistoryRenderMode::Rich);
    let collapsed = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert!(!text(&collapsed).contains("needle legend"));
    view.begin_search();
    assert!(view.paste_search("needle legend"));
    assert!(!view.advance_search(&cells));
    let found = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert!(text(&found).contains("needle legend"));
    assert!(view.image_placements().is_empty());
    view.cancel_search();
    let restored = render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    assert!(!text(&restored).contains("needle legend"));
    assert!(view.image_placements().is_empty());
}

#[test]
fn adjacent_tool_images_clip_and_reuse_placements_when_scrolled() {
    let cells = vec![image_call()];
    let mut view = TranscriptView::default();
    view.embedded_image_protocol = Some(ImageProtocol::Kitty);
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    let full = view.image_placements();
    assert_eq!(full.len(), 2);
    let short = Rect::new(
        /*x*/ 7,
        /*y*/ 5,
        /*width*/ 80,
        full[1].image.rows + 2,
    );
    view.jump_to_latest();
    view.render(short, &mut Buffer::empty(short), &cells);
    let clipped = view.image_placements();
    assert_eq!(clipped.len(), 2);
    assert!(clipped[0].first_row > 0);
    assert_eq!(clipped[1].first_row, 0);
    assert_eq!(clipped[0].area.bottom(), clipped[1].area.top());
    for (full, clipped) in full.iter().zip(&clipped) {
        assert!(Arc::ptr_eq(&full.image, &clipped.image));
        assert!(short.contains(clipped.area.as_position()));
        assert!(clipped.area.right() <= short.right());
        assert!(clipped.area.bottom() <= short.bottom());
    }
    view.scroll(&cells, -1);
    view.render(short, &mut Buffer::empty(short), &cells);
    let scrolled = view.image_placements();
    assert_eq!(scrolled.len(), 2);
    assert_eq!(scrolled[0].first_row + 1, clipped[0].first_row);
    assert_eq!(scrolled[1].area.bottom(), short.bottom());
    assert!(scrolled[1].area.height < full[1].image.rows);
}

#[test]
fn multiple_tool_images_share_a_raster_budget_without_losing_markers() {
    let dir = tempfile::tempdir().unwrap();
    image::RgbaImage::new(1, 1)
        .save(dir.path().join("plot.png"))
        .unwrap();
    let markdown =
        crate::history_cell::AgentMarkdownCell::new("![Plot](plot.png)".into(), dir.path());
    let mut lines = vec!["Returned image".into(); 8];
    lines.extend(markdown.transcript_hyperlink_lines(/*width*/ 80));
    let image = Arc::new(crate::transcript_image::TranscriptImage {
        protocol: ImageProtocol::Kitty,
        columns: 64,
        rows: 64,
        pixels: image::RgbaImage::new(1024, 1024),
        cell_width: 16,
        cell_height: 16,
    });
    let mut layout = TextLayout::new(lines, /*width*/ 80);
    let text = layout.text().to_owned();
    for _ in 0..8 {
        layout.add_attached_image(Arc::clone(&image));
    }
    let layout = layout.with_markdown_images(ImageProtocol::Kitty);
    assert_eq!(layout.attached_images.len(), 4);
    assert_eq!(layout.image_bytes(), 16 * 1024 * 1024);
    assert!(!layout.has_inline_images());
    assert_eq!(layout.text(), text);
    assert_eq!(layout.text().matches("Returned image").count(), 8);

    let rewrapped = layout.rewrap(/*width*/ 100);
    assert_eq!(rewrapped.attached_images.len(), 4);
    assert_eq!(rewrapped.image_bytes(), layout.image_bytes());
    assert!(!rewrapped.has_inline_images());
    assert_eq!(rewrapped.text(), text);
}

#[test]
fn live_computer_images_follow_disclosure_and_reuse_pixels_across_ticks_and_handoff() {
    let mut group = crate::history_cell::ComputerActivityCell::default();
    group.start(image_call_for("cua_repl"));
    group.start(new_active_mcp_tool_call(
        "pending".into(),
        McpInvocation {
            server: "cua_repl".into(),
            tool: "js".into(),
            arguments: None,
        },
        /*animations_enabled*/ false,
    ));
    let mut view = TranscriptView::default();
    view.embedded_image_protocol = Some(ImageProtocol::Kitty);
    let key = Some(ActiveCellTranscriptKey {
        cacheable: true,
        revision: 1,
        is_stream_continuation: false,
        animation_tick: Some(0),
    });
    let sync = |view: &mut TranscriptView, key| {
        let detailed = view.is_detailed();
        let expanded = view.sync_live_activity(&[], group.activity_ids()) && !detailed;
        view.sync_live_activity_tail(/*width*/ 80, key, expanded, |width| {
            Some(ActivityTranscriptLines {
                activity: if detailed || expanded {
                    group.transcript_hyperlink_lines(width)
                } else {
                    group.compact_hyperlink_lines(width)
                },
                auxiliary: Vec::new(),
                disclosure: if detailed {
                    None
                } else {
                    group.activity_disclosure(width)
                },
                images: group.image_data(),
            })
        })
    };
    assert!(sync(&mut view, key));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    assert!(view.image_placements().is_empty());
    let row = view
        .visible
        .iter()
        .position(|row| row.layout.disclosure_row() == Some(row.row))
        .unwrap() as u16;
    assert!(view.toggle_disclosure_at(&[], /*column*/ 4, row));
    assert!(sync(&mut view, key));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    let first = view.image_placements();
    assert_eq!(first.len(), 2);
    assert!(!sync(&mut view, key));

    let tick = Some(ActiveCellTranscriptKey {
        animation_tick: Some(1),
        ..key.unwrap()
    });
    assert!(sync(&mut view, tick));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    for (first, current) in first.iter().zip(view.image_placements()) {
        assert!(Arc::ptr_eq(&first.image, &current.image));
    }

    view.embedded_image_protocol = None;
    assert!(sync(&mut view, tick));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    assert!(view.image_placements().is_empty());
    view.embedded_image_protocol = Some(ImageProtocol::Kitty);
    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Raw);
    assert!(sync(&mut view, tick));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    assert!(view.image_placements().is_empty());

    view.set_presentation(/*detailed*/ true, HistoryRenderMode::Rich);
    assert!(sync(&mut view, tick));
    render(&mut view, &[], /*width*/ 80, /*height*/ 40);
    assert_eq!(view.image_placements().len(), 2);
    let cells = vec![Arc::new(group) as Arc<dyn HistoryCell>];
    view.sync_live_activity(&cells, Vec::new());
    assert!(view.sync_live_tail(/*width*/ 80, /*key*/ None, |_| None));
    render(&mut view, &cells, /*width*/ 80, /*height*/ 40);
    let committed = view.image_placements();
    assert_eq!(committed.len(), 2);
    for (first, committed) in first.iter().zip(committed) {
        assert!(Arc::ptr_eq(&first.image, &committed.image));
    }
}
