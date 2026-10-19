use super::*;
use codex_protocol::mcp::CallToolResult;
use pretty_assertions::assert_eq;
use serde_json::json;

fn call(id: &str, title: &str) -> McpToolCallCell {
    McpToolCallCell::new(
        id.to_string(),
        McpInvocation {
            server: "cua_repl".to_string(),
            tool: "js".to_string(),
            arguments: Some(json!({"title": title, "code": "await cua.getState()"})),
        },
        /*animations_enabled*/ false,
    )
}

fn result(text: &str) -> Result<CallToolResult, String> {
    Ok(CallToolResult {
        content: vec![json!({"type": "text", "text": text})],
        structured_content: None,
        is_error: Some(false),
        meta: None,
    })
}

fn render(cell: &ComputerActivityCell, width: u16) -> String {
    cell.display_lines(width)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn prepending_completed_history_preserves_the_pending_call_clock() {
    let mut older = ComputerActivityCell::default();
    older.complete(
        call("older", "Read the page"),
        Duration::ZERO,
        result("read"),
    );
    let mut live = ComputerActivityCell::default();
    live.start(call("pending", "Inspect selection"));
    let start_time = live.group.calls[0].start_time;

    live.prepend(older);
    assert_eq!(
        (
            live.call_ids().collect::<Vec<_>>(),
            live.group.calls[1].start_time,
            live.is_active()
        ),
        (vec!["older", "pending"], start_time, true),
    );
    live.complete(
        call("pending", "Inspect selection"),
        Duration::ZERO,
        result("inspected"),
    );
    assert_eq!(
        (live.call_ids().collect::<Vec<_>>(), live.is_active()),
        (vec!["older", "pending"], false),
    );
}

#[test]
fn computer_activity_active_and_interrupted() {
    let mut cell = ComputerActivityCell::default();
    cell.complete(call("1", "Opened Chrome"), Duration::ZERO, result("ready"));
    cell.start(call("2", "Load the terminal"));
    cell.start(call("2", "Duplicate start"));
    insta::assert_snapshot!("computer_activity_active", render(&cell, /*width*/ 80));
    cell.mark_failed();
    insta::assert_snapshot!("computer_activity_interrupted", render(&cell, /*width*/ 80));
    assert_eq!(
        cell.group
            .calls
            .iter()
            .map(McpToolCallCell::success)
            .collect::<Vec<_>>(),
        vec![Some(true), Some(false)]
    );
}

#[test]
fn computer_activity_prioritizes_errors_and_images_without_reordering() {
    let mut cell = ComputerActivityCell::default();
    cell.complete(
        call("1", "Open browser"),
        Duration::ZERO,
        Err("No browser is available\n\n## Computer Use\nFull tool documentation".to_string()),
    );
    cell.complete(
        call("2", "Connect to Chrome"),
        Duration::ZERO,
        result("connected"),
    );
    cell.complete(
        call("3", "Reproduce the clear-command bug"),
        Duration::ZERO,
        result("reproduced"),
    );
    let mut image = result("screenshot metadata").unwrap();
    image.content.push(json!({"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="}));
    cell.complete(
        call("4", "Capture the reproduced bug"),
        Duration::ZERO,
        Ok(image),
    );
    cell.complete(
        call("5", "Check the final page"),
        Duration::ZERO,
        result("checked"),
    );
    insta::assert_snapshot!("computer_activity_mixed", render(&cell, /*width*/ 90));
    insta::assert_snapshot!("computer_activity_narrow", render(&cell, /*width*/ 24));
    insta::assert_snapshot!(
        "computer_activity_compact",
        cell.compact_hyperlink_lines(/*width*/ 24)
            .iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let transcript = cell
        .transcript_lines(/*width*/ 100)
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("computer_activity_expanded", transcript);
    assert!(transcript.contains("Full tool documentation"));
    assert_eq!(
        cell.raw_lines(),
        plain_lines(cell.transcript_lines(u16::MAX))
    );
    assert!(
        cell.display_lines(/*width*/ 24)
            .iter()
            .all(|line| line.width() <= 24)
    );
}

#[test]
fn computer_activity_completed_and_transport_errors_keep_full_details() {
    let mut cell = ComputerActivityCell::default();
    cell.complete(
        call("1", "Inspect page"),
        Duration::ZERO,
        result("A successful result with details"),
    );
    cell.complete(call("2", "Reload page"), Duration::ZERO, result("Loaded"));
    insta::assert_snapshot!("computer_activity_success", render(&cell, /*width*/ 80));
    let mut failure =
        result("Browser state changed\nRe-query the browser before continuing").unwrap();
    failure.is_error = Some(true);
    cell.complete(
        call("3", "Inspect refreshed page"),
        Duration::ZERO,
        Ok(failure),
    );
    insta::assert_snapshot!(
        "computer_activity_failed_result",
        render(&cell, /*width*/ 90)
    );
    cell.mark_failed();
    assert_eq!(
        cell.group
            .calls
            .iter()
            .map(McpToolCallCell::success)
            .collect::<Vec<_>>(),
        vec![Some(true), Some(true), Some(false)]
    );
}

#[test]
fn computer_activity_preview_handles_unicode_and_long_diagnostics() {
    assert_eq!(preview(" 你好\nworld ", /*width*/ 6), "你好 …");
    let mut cell = ComputerActivityCell::default();
    cell.complete(
        call("1", "检查浏览器"),
        Duration::ZERO,
        Err("diagnostic ".repeat(200)),
    );
    insta::assert_snapshot!("computer_activity_unicode", render(&cell, /*width*/ 24));
    assert!(
        cell.display_lines(/*width*/ 24)
            .iter()
            .all(|line| line.width() <= 24)
    );
    assert!(cell.transcript_lines(/*width*/ 100).len() > 20);
}

#[test]
fn computer_activity_keeps_recent_images_and_all_capture_markers() {
    let image = json!({"type": "image", "mimeType": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="});
    let mut older = ComputerActivityCell::default();
    older.complete(
        call("older", "Older screenshot"),
        Duration::ZERO,
        Ok(CallToolResult {
            content: vec![image.clone()],
            ..result("").unwrap()
        }),
    );
    let mut cell = ComputerActivityCell::default();
    let mut latest_images = Vec::new();
    for index in 0..result::MAX_IMAGES + 1 {
        cell.complete(
            call(&index.to_string(), "Capture screenshot"),
            Duration::ZERO,
            Ok(CallToolResult {
                content: vec![image.clone()],
                ..result("").unwrap()
            }),
        );
        latest_images.push(cell.image_data().pop().unwrap());
    }
    cell.prepend(older);
    let retained = cell.image_data();
    assert_eq!(retained.len(), result::MAX_IMAGES);
    assert!(
        retained
            .iter()
            .zip(&latest_images[1..])
            .all(|(a, b)| Arc::ptr_eq(a, b))
    );
    assert!(cell.group.calls.iter().all(has_image));
    assert!(cell.group.calls[0].image_data().is_empty());
    assert!(cell.group.calls[1].image_data().is_empty());
}

#[test]
fn computer_activity_image_bytes_share_one_budget() {
    let mut cell = ComputerActivityCell::default();
    for index in 0..3 {
        let mut call = call(&index.to_string(), "Large image");
        call.result = Some(Ok(McpToolResult {
            content: Vec::new(),
            is_error: false,
            has_image: true,
            images: vec![vec![index as u8; result::MAX_IMAGE_BYTES / 2 + 1].into()],
        }));
        cell.start(call);
    }
    assert_eq!(cell.image_data().len(), 1);
    assert_eq!(cell.image_data()[0][0], 2);
    assert!(cell.group.calls.iter().all(has_image));
}
