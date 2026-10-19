use super::*;
use crate::pets::ImageProtocol;
use crate::transcript_image::ImagePlacement;
use crate::transcript_image::TranscriptImage;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn terminal() -> Terminal<CaptureBackend> {
    let mut terminal = Terminal::with_options(CaptureBackend::new(40, 16)).unwrap();
    terminal.set_viewport_area(Rect::new(0, 0, 40, 16));
    terminal
}

fn image(protocol: ImageProtocol) -> Arc<TranscriptImage> {
    Arc::new(TranscriptImage {
        protocol,
        columns: 16,
        rows: 40,
        pixels: image::RgbaImage::from_pixel(128, 640, image::Rgba([10, 110, 200, 255])),
        cell_width: 8,
        cell_height: 16,
    })
}

fn uploads(output: &str) -> usize {
    output.matches("_Ga=t").count() + output.matches("_Ga=T").count()
}

fn draw_image(terminal: &mut Terminal<CaptureBackend>, image: &Arc<TranscriptImage>, footer: &str) {
    terminal
        .draw_with_size(terminal.last_known_screen_size, |frame| {
            Line::from(footer).render(Rect::new(0, 15, 40, 1), frame.buffer_mut());
            frame.transcript_images.push(ImagePlacement {
                image: Arc::clone(image),
                area: Rect::new(2, 2, image.columns, 6),
                first_row: 0,
            });
        })
        .unwrap();
}

#[test]
fn scrolling_kitty_images_uploads_pixels_once() {
    for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
        let image = image(protocol);
        let mut terminal = terminal();
        let size = terminal.last_known_screen_size;
        for first_row in 0..10 {
            let before = terminal.backend().output.len();
            terminal
                .draw_with_size(size, |frame| {
                    frame.transcript_images.push(ImagePlacement {
                        image: Arc::clone(&image),
                        area: Rect::new(2, 2 + first_row as u16 % 2, image.columns, 6),
                        first_row,
                    });
                })
                .unwrap();
            if first_row > 0 {
                assert!(terminal.backend().output.len() - before < 256);
            }
        }
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        assert_eq!(output.matches("_Ga=p").count(), 10);
    }
}

#[test]
fn duplicate_kitty_images_share_upload_and_have_separate_placements() {
    for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
        let image = image(protocol);
        let mut terminal = terminal();
        terminal
            .draw_with_size(terminal.last_known_screen_size, |frame| {
                for x in [2, 20] {
                    frame.transcript_images.push(ImagePlacement {
                        image: Arc::clone(&image),
                        area: Rect::new(x, 2, image.columns, 6),
                        first_row: 0,
                    });
                }
            })
            .unwrap();
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        let placements = output
            .split("\x1b_G")
            .filter_map(|command| command.strip_prefix("a=p,"))
            .map(|command| command.split_once(';').unwrap().0)
            .collect::<Vec<_>>();
        assert_eq!(placements.len(), 2);
        let parameter = |index: usize, key| {
            placements[index]
                .split(',')
                .filter_map(|part| part.split_once('='))
                .find_map(|(name, value)| (name == key).then_some(value))
                .unwrap()
        };
        assert_eq!(parameter(0, "i"), parameter(1, "i"));
        assert_ne!(parameter(0, "p"), parameter(1, "p"));
    }
}

#[test]
fn unrelated_redraws_and_viewport_clear_preserve_kitty_uploads() {
    for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
        let image = image(protocol);
        let mut terminal = terminal();
        for footer in ["", "a", "ab"] {
            draw_image(&mut terminal, &image, footer);
        }
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        assert_eq!(output.matches("_Ga=p").count(), 1);

        terminal.clear().unwrap();
        draw_image(&mut terminal, &image, "ab");
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        assert_eq!(output.matches("_Ga=p").count(), 2);
    }
}

#[test]
fn hiding_and_showing_an_overlay_reuses_kitty_uploads() {
    for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
        let image = image(protocol);
        let mut terminal = terminal();
        draw_image(&mut terminal, &image, "");
        terminal
            .draw_with_size(terminal.last_known_screen_size, |frame| {
                Line::from("Overlay").render(Rect::new(2, 2, 20, 1), frame.buffer_mut());
            })
            .unwrap();
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        assert_eq!(output.matches("_Ga=p").count(), 1);
        assert!(output.contains("_Ga=d,d=i,"));
        assert!(!output.contains("_Ga=d,d=I,"));

        draw_image(&mut terminal, &image, "");
        let output = terminal.backend().output();
        assert_eq!(uploads(&output), 1);
        assert_eq!(output.matches("_Ga=p").count(), 2);
    }
}

#[test]
fn external_screen_changes_forget_uploads_without_deleting_from_the_new_screen() {
    let image = image(ImageProtocol::Kitty);
    let mut terminal = terminal();
    draw_image(&mut terminal, &image, "");
    let output = terminal.backend().output();
    terminal.forget_transcript_images();
    assert_eq!(terminal.backend().output(), output);
    draw_image(&mut terminal, &image, "");
    let output = terminal.backend().output();
    assert_eq!(uploads(&output), 2);
    assert!(!output.contains("_Ga=d"));
}

#[test]
fn terminal_resets_and_full_screen_clears_reupload_kitty_pixels() {
    for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
        let image = image(protocol);
        let mut terminal = terminal();
        draw_image(&mut terminal, &image, "");
        assert_eq!(uploads(&terminal.backend().output()), 1);

        terminal.reset_transcript_images().unwrap();
        draw_image(&mut terminal, &image, "");
        assert_eq!(uploads(&terminal.backend().output()), 2);

        terminal.clear_visible_screen().unwrap();
        draw_image(&mut terminal, &image, "");
        assert_eq!(uploads(&terminal.backend().output()), 3);

        terminal.clear_scrollback_and_visible_screen_ansi().unwrap();
        draw_image(&mut terminal, &image, "");
        assert_eq!(uploads(&terminal.backend().output()), 4);
        assert_eq!(terminal.backend().output().matches("_Ga=p").count(), 4);
    }
}
