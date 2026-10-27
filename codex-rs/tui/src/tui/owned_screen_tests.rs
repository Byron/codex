//! Exercise terminal updates and session ownership in isolation so global modes cannot race tests.

#![cfg(unix)]

use super::MouseCapture;
use super::TuiEvent;
use crate::pager_overlay::Overlay;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Style;
use std::process::Command;

const CHILD_ENV: &str = "CODEX_TUI_OWNED_SCREEN_CHILD";

#[test]
fn unchanged_inline_frames_are_silent_but_history_insertion_is_synchronized() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tui::owned_screen_tests::unchanged_frames_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = String::from_utf8(output.stdout).unwrap();
    let unchanged = output.split("<unchanged>").skip(1).collect::<Vec<_>>();
    assert_eq!(unchanged.len(), 2);
    for frame in unchanged {
        assert_eq!(frame.split_once("</unchanged>").unwrap().0, "");
    }
    let history = output.split("<history>").skip(1).collect::<Vec<_>>();
    assert_eq!(history.len(), 2);
    for frame in history {
        let frame = frame.split_once("</history>").unwrap().0;
        assert!(frame.starts_with("\x1b[?2026h"));
        assert!(frame.ends_with("\x1b[?2026l"));
        assert!(frame.contains("Inserted history"));
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spawned by the unchanged inline-frame test"]
async fn unchanged_frames_child() {
    use std::io::Write;
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let mut tui = super::test_support::make_test_tui().unwrap();
    tui.terminal.set_viewport_area(Rect::new(0, 4, 80, 1));
    let size = Size::new(80, 24);
    let mut renders = 0;
    for reflow in [false, true] {
        for index in 0..12 {
            if index == 1 {
                std::io::stdout().write_all(b"<unchanged>").unwrap();
            }
            if index == 11 {
                std::io::stdout()
                    .write_all(b"</unchanged><history>")
                    .unwrap();
                tui.insert_history_lines(vec!["Inserted history".into()]);
            }
            let draw = |frame: &mut crate::custom_terminal::Frame<'_>| {
                renders += 1;
                let area = frame.area();
                frame
                    .buffer_mut()
                    .set_string(area.x, area.y, "Working", Style::default());
                frame.set_cursor_position((area.x + 2, area.y));
            };
            if reflow {
                tui.draw_with_resize_reflow(1, size, /*hud*/ None, draw)
                    .unwrap();
            } else {
                tui.draw(1, draw).unwrap();
            }
        }
        std::io::stdout().write_all(b"</history>").unwrap();
    }
    assert_eq!(renders, 24);
}

#[test]
fn owned_screen_preserves_inline_viewport_across_overlay_handoff_and_resume() {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tui::owned_screen_tests::owned_screen_lifecycle_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("run isolated owned-screen child");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let mut terminal = vt100::Parser::new(
        /*rows*/ 24, /*cols*/ 80, /*scrollback_len*/ 0,
    );
    terminal.process(&output.stdout);
    assert_eq!(
        (
            terminal.screen().alternate_screen(),
            terminal.screen().mouse_protocol_mode()
        ),
        (false, vt100::MouseProtocolMode::None),
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spawned by the owned-screen lifecycle test"]
async fn owned_screen_lifecycle_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let mut tui = super::test_support::make_test_tui().expect("test tui");
    let inline = Rect::new(
        /*x*/ 0, /*y*/ 4, /*width*/ 80, /*height*/ 5,
    );
    tui.terminal.set_viewport_area(inline);
    tui.set_alt_screen_enabled(/*enabled*/ false);
    tui.set_owned_screen(/*owned*/ true).expect("honor no-alt");
    tui.set_mouse_capture(MouseCapture::Enabled)
        .expect("no-alt overlay");
    tui.set_mouse_capture(MouseCapture::Inherit)
        .expect("close no-alt overlay");
    assert_eq!(
        (
            tui.is_owned_screen(),
            tui.is_alt_screen_active(),
            tui.terminal.viewport_area
        ),
        (false, false, inline),
    );

    let mut pager = Overlay::new_static_with_lines(
        vec!["static pager".into()],
        "DETAILS".to_string(),
        crate::keymap::RuntimeKeymap::defaults().pager,
    );
    pager
        .handle_event(&mut tui, TuiEvent::FocusLost)
        .expect("request no-alt static input");
    assert!(tui.mouse_capture == MouseCapture::Disabled);
    assert!(!tui.is_alt_screen_active());
    pager
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        )
        .expect("close no-alt static pager");
    assert!(pager.is_done());
    assert!(tui.mouse_capture == MouseCapture::Inherit);
    assert_eq!(tui.terminal.viewport_area, inline);

    tui.set_alt_screen_enabled(/*enabled*/ true);
    // A startup overlay may already hold the alternate screen when App selects ownership.
    tui.enter_alt_screen().expect("startup overlay");
    tui.set_mouse_capture(MouseCapture::Enabled)
        .expect("nested transcript input");
    tui.leave_alt_screen_for_handoff()
        .expect("yield transcript to editor");
    assert!(tui.mouse_capture == MouseCapture::Enabled);
    tui.enter_alt_screen()
        .expect("restore transcript after editor");
    tui.set_mouse_capture(MouseCapture::Inherit)
        .expect("return to picker input");
    assert!(tui.is_alt_screen_active());
    let mut pager = Overlay::new_static_with_lines(
        vec!["static pager".into()],
        "DETAILS".to_string(),
        crate::keymap::RuntimeKeymap::defaults().pager,
    );
    pager
        .handle_event(&mut tui, TuiEvent::FocusLost)
        .expect("request native static input");
    assert!(tui.mouse_capture == MouseCapture::Disabled);
    tui.leave_alt_screen_for_handoff()
        .expect("yield static pager to editor");
    assert!(tui.mouse_capture == MouseCapture::Disabled);
    assert_eq!(tui.terminal.viewport_area, inline);
    tui.enter_alt_screen().expect("restore static pager");
    super::ALTERNATE_SCREEN
        .leave(tui.terminal.backend_mut())
        .expect("suspend static pager");
    super::job_control::PreparedResumeAction::RestoreAltScreen
        .apply(
            &mut tui.terminal,
            Size::new(/*width*/ 80, /*height*/ 24),
            /*owned*/ false,
            tui.mouse_capture.captures_mouse(/*owned_screen*/ false),
        )
        .expect("resume static pager");
    pager
        .handle_event(
            &mut tui,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        )
        .expect("close static pager back to picker");
    assert!(pager.is_done());
    assert!(tui.mouse_capture == MouseCapture::Inherit);
    assert!(tui.is_alt_screen_active());
    tui.set_owned_screen(/*owned*/ true)
        .expect("promote overlay");
    let owned = Rect::from(Size::new(/*width*/ 80, /*height*/ 24));
    for _ in 0..2 {
        tui.enter_alt_screen().expect("open overlay");
        tui.leave_alt_screen().expect("close overlay");
        assert_eq!(
            (
                tui.is_owned_screen(),
                tui.is_alt_screen_active(),
                tui.terminal.viewport_area,
                tui.alt_saved_viewport,
            ),
            (true, true, owned, Some(inline)),
        );
    }

    tui.leave_alt_screen_for_handoff().expect("yield to editor");
    assert_eq!(
        (
            tui.is_owned_screen(),
            tui.is_alt_screen_active(),
            tui.terminal.viewport_area,
            tui.alt_saved_viewport,
        ),
        (true, false, inline, None),
    );
    tui.enter_alt_screen().expect("return from editor");
    // Suspension leaves the real screen while retaining the session's ownership and viewport.
    super::ALTERNATE_SCREEN
        .leave(tui.terminal.backend_mut())
        .expect("suspend screen");
    let resumed = Size::new(/*width*/ 100, /*height*/ 30);
    super::job_control::PreparedResumeAction::RestoreAltScreen
        .apply(
            &mut tui.terminal,
            resumed,
            /*owned*/ true,
            /*capture_mouse*/ true,
        )
        .expect("resume owned screen");
    assert_eq!(
        (
            tui.is_owned_screen(),
            tui.terminal.viewport_area,
            tui.alt_saved_viewport
        ),
        (true, Rect::from(resumed), Some(inline)),
    );
    tui.set_alt_screen_enabled(/*enabled*/ false);
    assert_eq!(
        (
            tui.is_owned_screen(),
            tui.is_alt_screen_active(),
            tui.terminal.viewport_area,
            tui.alt_saved_viewport,
        ),
        (false, false, inline, None),
    );
    // Exercise direct owned entry as well as promotion of an existing overlay.
    tui.set_alt_screen_enabled(/*enabled*/ true);
    tui.set_owned_screen(/*owned*/ true)
        .expect("own session screen");
    tui.set_owned_screen(/*owned*/ false)
        .expect("release session screen");
    assert_eq!(tui.terminal.viewport_area, inline);

    // A fresh fullscreen launch defers terminal entry until its first frame.
    tui.terminal.tmux_kitty_image_base = Some(0x80_0100);
    let support =
        crate::pets::PetImageSupport::Unsupported(crate::pets::PetImageUnsupportedReason::Tmux);
    tui.prepare_owned_screen(/*owned*/ true)
        .expect("prepare fullscreen");
    assert_eq!(support.protocol(), Some(crate::pets::ImageProtocol::Kitty));
    tui.set_owned_screen(/*owned*/ false)
        .expect("return to scrollback");
    assert_eq!(support.protocol(), None);
}
