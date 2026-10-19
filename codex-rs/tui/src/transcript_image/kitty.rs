use std::io;
use std::io::Write;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crossterm::cursor::MoveTo;
use crossterm::cursor::RestorePosition;
use crossterm::cursor::SavePosition;
use crossterm::queue;
use image::ImageEncoder;
use image::codecs::png::CompressionType;
use image::codecs::png::FilterType;
use image::codecs::png::PngEncoder;
use ratatui::layout::Rect;

use super::TranscriptImage;
use crate::pets::ImageProtocol;

const CHUNK_SIZE: usize = 4096;

/// Asset failures preserve the text fallback; only terminal writes are fatal. A successful
/// local-file upload returns the file so it survives until the terminal has read it.
pub(super) fn upload(
    writer: &mut impl Write,
    image: &TranscriptImage,
    id: u32,
) -> io::Result<Option<Option<tempfile::NamedTempFile>>> {
    let mut png = Vec::new();
    if PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            image.pixels.as_raw(),
            image.pixels.width(),
            image.pixels.height(),
            image::ExtendedColorType::Rgba8,
        )
        .is_err()
    {
        return Ok(None);
    }

    if image.protocol == ImageProtocol::KittyLocalFile {
        let file = (|| {
            let mut file = tempfile::Builder::new().suffix(".png").tempfile().ok()?;
            file.write_all(&png).ok()?;
            file.flush().ok()?;
            let path = file.path().canonicalize().ok()?;
            Some((file, STANDARD.encode(path.to_string_lossy().as_bytes())))
        })();
        let Some((file, path)) = file else {
            return Ok(None);
        };
        write!(writer, "\x1b_Ga=t,t=f,f=100,i={id},q=2;{path}\x1b\\")?;
        return Ok(Some(Some(file)));
    }

    let payload = STANDARD.encode(png);
    let mut chunks = payload.as_bytes().chunks(CHUNK_SIZE).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            write!(writer, "\x1b_Ga=t,t=d,f=100,i={id},q=2,m={more};")?;
            first = false;
        } else {
            write!(writer, "\x1b_Gm={more};")?;
        }
        writer.write_all(chunk)?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(Some(None))
}

pub(super) fn place(
    writer: &mut impl Write,
    image: &TranscriptImage,
    area: Rect,
    first_row: usize,
    id: u32,
    placement_id: u32,
) -> io::Result<()> {
    if area.is_empty() || image.rows == 0 {
        return Ok(());
    }
    let rows = u32::from(image.rows);
    let first_row = first_row.min(usize::from(image.rows)) as u32;
    // Match the full image's scaling to terminal rows, including a partial final pixel row.
    let top = first_row * image.pixels.height() / rows;
    let bottom = (first_row + u32::from(area.height)).min(rows) * image.pixels.height() / rows;
    let height = bottom - top;
    if height == 0 {
        return Ok(());
    }
    let width = image.pixels.width();
    let columns = image.columns;
    let visible_rows = area.height;
    queue!(writer, SavePosition, MoveTo(area.x, area.y))?;
    write!(
        writer,
        "\x1b_Ga=p,i={id},p={placement_id},x=0,y={top},w={width},h={height},c={columns},r={visible_rows},C=1,q=2;\x1b\\"
    )?;
    queue!(writer, RestorePosition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn image(protocol: ImageProtocol) -> TranscriptImage {
        TranscriptImage {
            protocol,
            columns: 16,
            rows: 4,
            pixels: image::RgbaImage::from_fn(128, 49, |x, y| {
                let value = (x + y * 128).wrapping_mul(0x9E37_79B9);
                image::Rgba(value.to_le_bytes())
            }),
            cell_width: 8,
            cell_height: 16,
        }
    }

    #[test]
    fn inline_upload_encodes_full_png_without_creating_a_placement() {
        let image = image(ImageProtocol::Kitty);
        let mut output = Vec::new();
        assert!(upload(&mut output, &image, 42).unwrap().unwrap().is_none());
        let output = String::from_utf8(output).unwrap();
        let commands = output
            .split("\x1b\\")
            .filter(|command| !command.is_empty())
            .collect::<Vec<_>>();
        assert!(commands.len() > 1);
        assert!(commands[0].starts_with("\x1b_Ga=t,t=d,f=100,i=42,q=2,m=1;"));
        assert!(commands.last().unwrap().starts_with("\x1b_Gm=0;"));
        let mut encoded = String::new();
        for command in commands {
            let (_, payload) = command.split_once(';').unwrap();
            assert!(payload.len() <= CHUNK_SIZE);
            encoded.push_str(payload);
        }
        let png = STANDARD.decode(encoded).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded, image.pixels);
        assert!(!output.contains("a=T") && !output.contains("a=p"));
    }

    #[test]
    fn local_upload_keeps_the_full_png_file_alive() {
        let image = image(ImageProtocol::KittyLocalFile);
        let mut output = Vec::new();
        let file = upload(&mut output, &image, 42).unwrap().unwrap().unwrap();
        let output = String::from_utf8(output).unwrap();
        let encoded = output
            .strip_prefix("\x1b_Ga=t,t=f,f=100,i=42,q=2;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let path = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
        assert_eq!(
            std::path::Path::new(&path),
            file.path().canonicalize().unwrap()
        );
        assert_eq!(image::open(path).unwrap().to_rgba8(), image.pixels);
    }

    #[test]
    fn placements_crop_cached_pixels_at_the_scaled_row_boundaries() {
        let image = image(ImageProtocol::Kitty);
        for (first_row, visible_rows, top, height) in
            [(0, 4, 0, 49), (1, 2, 12, 24), (3, 1, 36, 13)]
        {
            let mut output = Vec::new();
            place(
                &mut output,
                &image,
                Rect::new(2, 3, image.columns, visible_rows),
                first_row,
                42,
                7,
            )
            .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(&format!(
                "\x1b_Ga=p,i=42,p=7,x=0,y={top},w=128,h={height},c=16,r={visible_rows},C=1,q=2;\x1b\\"
            )));
            assert!(!output.contains("a=t") && !output.contains("a=T"));
        }
        let mut output = Vec::new();
        place(
            &mut output,
            &image,
            Rect::new(2, 3, 16, 1),
            usize::MAX,
            42,
            7,
        )
        .unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn upload_propagates_terminal_write_errors() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            upload(&mut BrokenPipe, &image(ImageProtocol::Kitty), 42)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe,
        );
    }
}
