//! Bounded image previews for the owned fullscreen transcript.
//!
//! Only decoded or generated image pixels reach the terminal encoders. Previews are never source text;
//! their reserved rows belong to layout. The terminal retains visible placements and any local
//! transport files until those placements change.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;
use crossterm::cursor::MoveTo;
use crossterm::cursor::RestorePosition;
use crossterm::cursor::SavePosition;
use crossterm::queue;
use image::DynamicImage;
use image::ImageFormat;
use image::RgbaImage;
use ratatui::layout::Rect;

use crate::pets::ImageProtocol;

pub(crate) mod svg;

const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PIXELS: u32 = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_ROWS: u32 = 60;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Eq)]
enum ImageSource {
    File(PathBuf, u64, SystemTime),
    Data(Arc<[u8]>),
}

impl PartialEq for ImageSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::File(path, len, modified),
                Self::File(other_path, other_len, other_modified),
            ) => (path, len, modified) == (other_path, other_len, other_modified),
            (Self::Data(data), Self::Data(other)) => Arc::ptr_eq(data, other),
            _ => false,
        }
    }
}

impl ImageSource {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::File(..) => 0,
            Self::Data(data) => data.len(),
        }
    }
}

#[derive(Eq, PartialEq)]
struct ImageCacheKey {
    source: ImageSource,
    width: u16,
    protocol: ImageProtocol,
    cell_size: (u32, u32),
}

#[derive(Default)]
struct ImageCache {
    entries: VecDeque<(ImageCacheKey, Option<Arc<TranscriptImage>>)>,
    bytes: usize,
}

impl ImageCache {
    fn get_or_load(
        &mut self,
        key: ImageCacheKey,
        load: impl FnOnce() -> Option<Arc<TranscriptImage>>,
    ) -> Option<Arc<TranscriptImage>> {
        if let Some(index) = self.entries.iter().position(|(cached, _)| *cached == key)
            && let Some(entry) = self.entries.remove(index)
        {
            let image = entry.1.clone();
            self.entries.push_back(entry);
            return image;
        }
        let image = load();
        let bytes = key.source.retained_bytes()
            + image
                .as_ref()
                .map_or(0, |image| image.pixels.as_raw().len());
        if bytes > MAX_CACHE_BYTES {
            return image;
        }
        while self.entries.len() >= 16 || self.bytes + bytes > MAX_CACHE_BYTES {
            let (key, evicted) = self.entries.pop_front()?;
            self.bytes -= key.source.retained_bytes()
                + evicted
                    .as_ref()
                    .map_or(0, |image| image.pixels.as_raw().len());
        }
        self.bytes += bytes;
        self.entries.push_back((key, image.clone()));
        image
    }
}

thread_local! {
    static IMAGE_CACHE: RefCell<ImageCache> = RefCell::default();
}

// Separate from the two image ids reserved for pets (0xC0DE and 0xC0DF).
pub(crate) const FIRST_IMAGE_ID: u32 = 0xC100;

pub(crate) struct ImagePlacement {
    pub(crate) image: Arc<TranscriptImage>,
    pub(crate) area: Rect,
    pub(crate) first_row: usize,
    pub(crate) file: Option<tempfile::NamedTempFile>,
}

impl PartialEq for ImagePlacement {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.image, &other.image)
            && self.area == other.area
            && self.first_row == other.first_row
    }
}

pub(crate) struct TranscriptImage {
    pub(crate) protocol: ImageProtocol,
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) pixels: RgbaImage,
    pub(crate) cell_width: u32,
    pub(crate) cell_height: u32,
}

pub(crate) fn terminal_cell_size() -> Option<(u32, u32)> {
    crossterm::terminal::window_size()
        .ok()
        .filter(|size| size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0)
        .map(|size| {
            (
                u32::from(size.width / size.columns).max(1),
                u32::from(size.height / size.rows).max(1),
            )
        })
}

impl TranscriptImage {
    pub(crate) fn load(path: &Path, width: u16, protocol: ImageProtocol) -> Option<Arc<Self>> {
        let path = path.canonicalize().ok()?;
        // Opening a named pipe can block before a file handle is available for validation.
        if !path.metadata().ok()?.is_file() {
            return None;
        }
        let file = std::fs::File::open(&path).ok()?;
        let metadata = file.metadata().ok()?;
        // Bound encoded input as well as the raster: metadata alone can dwarf a tiny image.
        if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES as u64 {
            return None;
        }
        let cell_size = image_cell_size(protocol)?;
        let load = || {
            let mut data = Vec::new();
            file.take(MAX_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut data)
                .ok()?;
            Self::from_bytes_with_cell_size(&data, width, protocol, cell_size).map(Arc::new)
        };
        let Ok(modified) = metadata.modified() else {
            return load();
        };
        let key = ImageCacheKey {
            source: ImageSource::File(path, metadata.len(), modified),
            width,
            protocol,
            cell_size,
        };
        IMAGE_CACHE.with(|cache| cache.borrow_mut().get_or_load(key, load))
    }

    pub(crate) fn from_data(
        data: &Arc<[u8]>,
        width: u16,
        protocol: ImageProtocol,
    ) -> Option<Arc<Self>> {
        let cell_size = image_cell_size(protocol)?;
        let key = ImageCacheKey {
            source: ImageSource::Data(Arc::clone(data)),
            width,
            protocol,
            cell_size,
        };
        IMAGE_CACHE.with(|cache| {
            cache.borrow_mut().get_or_load(key, || {
                Self::from_bytes_with_cell_size(data, width, protocol, cell_size).map(Arc::new)
            })
        })
    }

    fn from_bytes_with_cell_size(
        data: &[u8],
        width: u16,
        protocol: ImageProtocol,
        (cell_width, cell_height): (u32, u32),
    ) -> Option<Self> {
        let columns = width.saturating_sub(2);
        if columns == 0
            || data.len() > MAX_INPUT_BYTES
            || cell_width == 0
            || cell_height == 0
            || cell_width > 128
            || cell_height > 256
        {
            return None;
        }
        // The sprite encoder scans each active color per Sixel band. Bound both its CPU
        // work and worst-case output even with large terminal cells or high-entropy images.
        let (max_width, max_height) = match protocol {
            ImageProtocol::Sixel => (
                (u32::from(columns.min(48)) * cell_width).min(160),
                (6 * cell_height).min(80),
            ),
            ImageProtocol::Kitty | ImageProtocol::KittyLocalFile => (
                (u32::from(columns) * cell_width).min(MAX_DIMENSION),
                (MAX_ROWS * cell_height).min(MAX_DIMENSION),
            ),
        };
        let pixels = if image::guess_format(data).is_ok() {
            let image = decode_image(data)?;
            let (width, height) = fitted_size(
                image.width() as f32,
                image.height() as f32,
                max_width,
                max_height,
            )?;
            image.thumbnail(width, height).to_rgba8()
        } else {
            let scale = match protocol {
                ImageProtocol::Sixel => 1.0,
                ImageProtocol::Kitty | ImageProtocol::KittyLocalFile => cell_height as f32 / 16.0,
            };
            svg::render(data, max_width, max_height, scale)?
        };
        Some(Self {
            protocol,
            columns: pixels.width().div_ceil(cell_width) as u16,
            rows: pixels.height().div_ceil(cell_height) as u16,
            pixels,
            cell_width,
            cell_height,
        })
    }

    /// Draw only the visible vertical slice, never covering the prompt header or composer.
    pub(crate) fn draw(
        &self,
        writer: &mut impl Write,
        area: Rect,
        first_row: usize,
        id: u32,
    ) -> Result<Option<tempfile::NamedTempFile>> {
        let (top, bottom) = match self.protocol {
            ImageProtocol::Sixel => (
                first_row as u32 * self.cell_height,
                (first_row as u32 + u32::from(area.height)) * self.cell_height,
            ),
            // Kitty scales the entire raster to r=rows; clip at those scaled boundaries.
            ImageProtocol::Kitty | ImageProtocol::KittyLocalFile => (
                first_row as u32 * self.pixels.height() / u32::from(self.rows),
                (first_row as u32 + u32::from(area.height)) * self.pixels.height()
                    / u32::from(self.rows),
            ),
        };
        let top = top.min(self.pixels.height());
        let height = bottom.min(self.pixels.height()) - top;
        if height == 0 {
            return Ok(None);
        }
        let cropped =
            image::imageops::crop_imm(&self.pixels, 0, top, self.pixels.width(), height).to_image();
        let mut file = None;
        // Encoding and temp-file failures must not abort the TUI. Only writes to the actual
        // terminal below are fatal; the existing text entry remains usable without its preview.
        let payload = (|| -> Result<_> {
            Ok(match self.protocol {
                ImageProtocol::Kitty | ImageProtocol::KittyLocalFile => {
                    let mut png = tempfile::Builder::new().suffix(".png").tempfile()?;
                    DynamicImage::ImageRgba8(cropped).write_to(&mut png, ImageFormat::Png)?;
                    png.flush()?;
                    let transmit = if self.protocol == ImageProtocol::KittyLocalFile {
                        crate::pets::kitty_transmit_png_file_with_id
                    } else {
                        crate::pets::kitty_transmit_png_with_id
                    };
                    let command = transmit(png.path(), self.columns, area.height, Some(id))?;
                    file = (self.protocol == ImageProtocol::KittyLocalFile).then_some(png);
                    command.into_bytes()
                }
                ImageProtocol::Sixel => {
                    crate::pets::encode_rgba(cropped.as_raw(), cropped.width(), cropped.height())?
                }
            })
        })();
        let Ok(payload) = payload else {
            return Ok(None);
        };
        queue!(writer, SavePosition, MoveTo(area.x, area.y))?;
        writer.write_all(&payload)?;
        queue!(writer, RestorePosition)?;
        Ok(file)
    }
}

fn image_cell_size(protocol: ImageProtocol) -> Option<(u32, u32)> {
    // Sixel paints physical pixels: guessing could cover the composer outside reserved rows.
    match protocol {
        ImageProtocol::Sixel => terminal_cell_size(),
        ImageProtocol::Kitty | ImageProtocol::KittyLocalFile => {
            Some(terminal_cell_size().unwrap_or((8, 16)))
        }
    }
}

pub(crate) fn decode_image(data: &[u8]) -> Option<DynamicImage> {
    if data.len() > MAX_INPUT_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_INPUT_BYTES as u64);
    reader.limits(limits);
    reader.decode().ok()
}

pub(crate) fn validate_image(data: &[u8]) -> bool {
    if image::guess_format(data).is_ok() {
        decode_image(data).is_some()
    } else {
        svg::parse(data).is_some()
    }
}

fn fitted_size(width: f32, height: f32, max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = 1.0_f32
        .min(max_width as f32 / width)
        .min(max_height as f32 / height)
        .min((MAX_PIXELS as f32 / (width * height)).sqrt());
    let width = (width * scale).floor().max(1.0) as u32;
    let height = (height * scale).floor().max(1.0) as u32;
    (width <= max_width && height <= max_height && width.checked_mul(height)? <= MAX_PIXELS)
        .then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut data = Cursor::new(Vec::new());
        RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut data, ImageFormat::Png)
            .unwrap();
        data.into_inner()
    }

    #[test]
    fn kitty_charts_use_available_width_and_readable_height() {
        let data = png(1600, 1000);
        for protocol in [ImageProtocol::Kitty, ImageProtocol::KittyLocalFile] {
            let image =
                TranscriptImage::from_bytes_with_cell_size(&data, 102, protocol, (8, 16)).unwrap();
            assert_eq!((image.columns, image.rows), (100, 32));
            assert_eq!(image.pixels.dimensions(), (800, 500));
            let retina =
                TranscriptImage::from_bytes_with_cell_size(&data, 102, protocol, (16, 32)).unwrap();
            assert_eq!((retina.columns, retina.rows), (100, 32));
            assert_eq!(retina.pixels.dimensions(), (1600, 1000));
        }
    }

    #[test]
    fn previews_bound_height_rasters_and_sixel_work() {
        let tall = TranscriptImage::from_bytes_with_cell_size(
            &png(100, 5000),
            102,
            ImageProtocol::Kitty,
            (8, 16),
        )
        .unwrap();
        assert_eq!(tall.rows, 60);
        let sixel = TranscriptImage::from_bytes_with_cell_size(
            &png(1600, 1000),
            102,
            ImageProtocol::Sixel,
            (8, 16),
        )
        .unwrap();
        assert_eq!(sixel.pixels.dimensions(), (128, 80));
        assert!(sixel.rows <= 6 && sixel.columns <= 48);
        let (width, height) = fitted_size(50_000.0, 50_000.0, 4096, 4096).unwrap();
        assert!(width * height <= MAX_PIXELS);
        assert!(fitted_size(f32::INFINITY, 100.0, 4096, 4096).is_none());
    }

    #[test]
    fn invalid_input_keeps_text_fallback() {
        for data in [b"not an image".as_slice(), &png(32, 32)[..30]] {
            assert!(!validate_image(data));
            assert!(
                TranscriptImage::from_data(&Arc::from(data), 100, ImageProtocol::Kitty).is_none()
            );
        }
        assert!(decode_image(&vec![0; MAX_INPUT_BYTES + 1]).is_none());
        for (width, cell_size) in [(2, (8, 16)), (100, (0, 16)), (100, (8, 257))] {
            assert!(
                TranscriptImage::from_bytes_with_cell_size(
                    &png(32, 32),
                    width,
                    ImageProtocol::Kitty,
                    cell_size,
                )
                .is_none()
            );
        }
    }

    #[test]
    fn file_cache_reuses_pixels_and_invalidates_on_size_or_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chart.png");
        std::fs::write(&path, png(640, 480)).unwrap();
        let protocol = ImageProtocol::Kitty;
        let first = TranscriptImage::load(&path, 100, protocol).unwrap();
        let unchanged = TranscriptImage::load(&path, 100, protocol).unwrap();
        assert!(Arc::ptr_eq(&first, &unchanged));
        let narrow = TranscriptImage::load(&path, 40, protocol).unwrap();
        assert!(!Arc::ptr_eq(&first, &narrow));
        std::fs::write(&path, b"invalid changed image").unwrap();
        assert!(TranscriptImage::load(&path, 100, protocol).is_none());
        assert!(TranscriptImage::load(&path, 100, protocol).is_none());
    }

    #[test]
    fn file_cache_bounds_pixels_and_remembers_failed_loads() {
        let key = |width| ImageCacheKey {
            source: ImageSource::File("chart.png".into(), 0, SystemTime::UNIX_EPOCH),
            width,
            protocol: ImageProtocol::Kitty,
            cell_size: (8, 16),
        };
        let image = Arc::new(TranscriptImage {
            protocol: ImageProtocol::Kitty,
            columns: 256,
            rows: 60,
            pixels: RgbaImage::new(2048, 2048),
            cell_width: 8,
            cell_height: 16,
        });
        let mut cache = ImageCache::default();
        cache.get_or_load(key(100), || Some(image.clone()));
        cache.get_or_load(key(101), || Some(image.clone()));
        cache.get_or_load(key(102), || Some(image.clone()));
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bytes, MAX_CACHE_BYTES);
        for width in 103..120 {
            cache.get_or_load(key(width), || None);
        }
        assert_eq!(cache.entries.len(), 16);
        assert_eq!(cache.bytes, 0);
        assert!(
            cache
                .get_or_load(key(119), || panic!("failed load was retried"))
                .is_none()
        );
    }

    #[test]
    fn data_cache_uses_arc_identity_and_counts_encoded_bytes_even_for_failures() {
        let data: Arc<[u8]> = png(32, 32).into();
        let key = |data: &Arc<[u8]>| ImageCacheKey {
            source: ImageSource::Data(Arc::clone(data)),
            width: 100,
            protocol: ImageProtocol::Kitty,
            cell_size: (8, 16),
        };
        let image = TranscriptImage::from_data(&data, 100, ImageProtocol::Kitty).unwrap();
        let unchanged = TranscriptImage::from_data(&data, 100, ImageProtocol::Kitty).unwrap();
        assert!(Arc::ptr_eq(&image, &unchanged));
        let separate: Arc<[u8]> = data.to_vec().into();
        let separately_loaded =
            TranscriptImage::from_data(&separate, 100, ImageProtocol::Kitty).unwrap();
        assert!(!Arc::ptr_eq(&image, &separately_loaded));

        let mut cache = ImageCache::default();
        cache.get_or_load(key(&data), || None);
        assert_eq!(cache.bytes, data.len());
        assert!(
            cache
                .get_or_load(key(&data), || panic!("failed data was decoded again"))
                .is_none()
        );
        let oversized: Arc<[u8]> = vec![0; MAX_CACHE_BYTES].into();
        let uncached = cache
            .get_or_load(key(&oversized), || Some(Arc::clone(&image)))
            .unwrap();
        assert!(Arc::ptr_eq(&uncached, &image));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, data.len());
    }
}
