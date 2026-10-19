use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::sync::Arc;

use super::FIRST_IMAGE_ID;
use super::ImagePlacement;
use super::MAX_CACHE_BYTES;
use super::TranscriptImage;
use super::kitty;
use crate::pets::ImageProtocol;

const MAX_UPLOADS: usize = 16;

#[derive(Debug)]
struct Upload {
    image: Arc<TranscriptImage>,
    id: u32,
    // Local-file transport is asynchronous; keep the file for the upload's lifetime.
    _file: Option<tempfile::NamedTempFile>,
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptImages {
    pub(crate) placements: Vec<ImagePlacement>,
    uploads: VecDeque<Upload>,
    last_id: u32,
}

impl TranscriptImages {
    /// Delete placements before painting text, preserving uploaded pixels for reuse.
    pub(crate) fn remove_placements(&mut self, writer: &mut impl Write) -> io::Result<bool> {
        let mut removed_sixel = false;
        for (index, placement) in self.placements.iter().enumerate() {
            if placement.image.protocol == ImageProtocol::Sixel {
                removed_sixel = true;
            } else if let Some(upload) = self
                .uploads
                .iter()
                .find(|upload| Arc::ptr_eq(&upload.image, &placement.image))
            {
                let id = upload.id;
                let placement_id = index + 1;
                write!(writer, "\x1b_Ga=d,d=i,i={id},p={placement_id},q=2;\x1b\\")?;
            }
        }
        self.placements.clear();
        Ok(removed_sixel)
    }

    pub(crate) fn reset(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let uploads = std::mem::take(&mut self.uploads);
        self.placements.clear();
        for upload in uploads {
            writer.write_all(crate::pets::kitty_delete_image(upload.id).as_bytes())?;
        }
        Ok(())
    }

    pub(crate) fn forget(&mut self) {
        self.uploads.clear();
        self.placements.clear();
    }

    pub(crate) fn draw(
        &mut self,
        writer: &mut impl Write,
        placements: Vec<ImagePlacement>,
    ) -> io::Result<()> {
        for (index, placement) in placements.iter().enumerate() {
            let image = &placement.image;
            if image.protocol == ImageProtocol::Sixel {
                image.draw_sixel(writer, placement.area, placement.first_row)?;
                continue;
            }
            let upload = if let Some(index) = self
                .uploads
                .iter()
                .position(|upload| Arc::ptr_eq(&upload.image, image))
                && let Some(upload) = self.uploads.remove(index)
            {
                upload
            } else {
                loop {
                    self.last_id = self.last_id.wrapping_add(1).max(FIRST_IMAGE_ID);
                    if !self.uploads.iter().any(|upload| upload.id == self.last_id) {
                        break;
                    }
                }
                let Some(file) = kitty::upload(writer, image, self.last_id)? else {
                    continue;
                };
                Upload {
                    image: Arc::clone(image),
                    id: self.last_id,
                    _file: file,
                }
            };
            let id = upload.id;
            self.uploads.push_back(upload);
            kitty::place(
                writer,
                image,
                placement.area,
                placement.first_row,
                id,
                index as u32 + 1,
            )?;
        }
        self.placements = placements;
        self.evict_unused(writer)
    }

    fn evict_unused(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let mut bytes: usize = self
            .uploads
            .iter()
            .map(|upload| upload.image.pixels.as_raw().len())
            .sum();
        while self.uploads.len() > MAX_UPLOADS || bytes > MAX_CACHE_BYTES {
            let Some(index) = self.uploads.iter().position(|upload| {
                !self
                    .placements
                    .iter()
                    .any(|placement| Arc::ptr_eq(&placement.image, &upload.image))
            }) else {
                // Visible images may exceed the cache budget; evict them once they leave view.
                break;
            };
            let upload = &self.uploads[index];
            writer.write_all(crate::pets::kitty_delete_image(upload.id).as_bytes())?;
            bytes -= upload.image.pixels.as_raw().len();
            self.uploads.remove(index);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::layout::Rect;

    fn image(protocol: ImageProtocol, width: u32, height: u32) -> Arc<TranscriptImage> {
        Arc::new(TranscriptImage {
            protocol,
            columns: width.div_ceil(8) as u16,
            rows: height.div_ceil(16) as u16,
            pixels: image::RgbaImage::from_pixel(width, height, image::Rgba([8, 80, 160, 255])),
            cell_width: 8,
            cell_height: 16,
        })
    }

    fn placement(image: &Arc<TranscriptImage>, y: u16) -> ImagePlacement {
        ImagePlacement {
            image: Arc::clone(image),
            area: Rect::new(2, y, image.columns, image.rows),
            first_row: 0,
        }
    }

    #[test]
    fn local_upload_survives_hiding_and_repeated_placements_until_reset() {
        let image = image(ImageProtocol::KittyLocalFile, 8, 16);
        let mut state = TranscriptImages::default();
        let mut output = Vec::new();
        state
            .draw(
                &mut output,
                vec![placement(&image, 2), placement(&image, 5)],
            )
            .unwrap();
        assert_eq!(state.uploads.len(), 1);
        let path = state.uploads[0]
            ._file
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let id = state.uploads[0].id;
        assert!(path.is_file());

        assert!(!state.remove_placements(&mut output).unwrap());
        state.draw(&mut output, Vec::new()).unwrap();
        assert!(state.placements.is_empty());
        assert!(path.is_file());
        state.draw(&mut output, vec![placement(&image, 3)]).unwrap();
        assert_eq!(state.uploads[0].id, id);
        let commands = String::from_utf8(output.clone()).unwrap();
        assert_eq!(commands.matches("_Ga=t").count(), 1);
        assert!(commands.contains(&format!("a=p,i={id},p=1,")));
        assert!(commands.contains(&format!("a=p,i={id},p=2,")));
        assert!(commands.contains(&format!("a=d,d=i,i={id},p=1,")));
        assert!(commands.contains(&format!("a=d,d=i,i={id},p=2,")));
        assert!(!commands.contains("d=I"));

        state.reset(&mut output).unwrap();
        assert!(state.uploads.is_empty());
        assert!(state.placements.is_empty());
        assert!(!path.exists());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains(&format!("a=d,d=I,i={id},"))
        );
    }

    #[test]
    fn failed_reset_still_forgets_uploads_from_the_old_screen() {
        let image = image(ImageProtocol::KittyLocalFile, 8, 16);
        let mut state = TranscriptImages::default();
        state
            .draw(&mut Vec::new(), vec![placement(&image, 2)])
            .unwrap();
        let path = state.uploads[0]
            ._file
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let mut full: &mut [u8] = &mut [];
        assert!(state.reset(&mut full).is_err());
        assert!(state.uploads.is_empty());
        assert!(state.placements.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn upload_cache_reuses_recent_entries_and_removes_the_least_recent_file() {
        let images = (0..=MAX_UPLOADS)
            .map(|index| {
                let protocol = if index < 2 {
                    ImageProtocol::KittyLocalFile
                } else {
                    ImageProtocol::Kitty
                };
                image(protocol, 8, 16)
            })
            .collect::<Vec<_>>();
        let mut state = TranscriptImages::default();
        let mut output = Vec::new();
        for image in &images[..MAX_UPLOADS] {
            state.remove_placements(&mut output).unwrap();
            state.draw(&mut output, vec![placement(image, 2)]).unwrap();
        }
        assert_eq!(state.uploads.len(), MAX_UPLOADS);
        let kept_path = state.uploads[0]
            ._file
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let evicted_path = state.uploads[1]
            ._file
            .as_ref()
            .unwrap()
            .path()
            .to_path_buf();
        let evicted_id = state.uploads[1].id;
        state.remove_placements(&mut output).unwrap();
        state
            .draw(&mut output, vec![placement(&images[0], 2)])
            .unwrap();
        assert!(Arc::ptr_eq(
            &state.uploads.back().unwrap().image,
            &images[0]
        ));
        state.remove_placements(&mut output).unwrap();
        state
            .draw(&mut output, vec![placement(&images[MAX_UPLOADS], 2)])
            .unwrap();

        assert_eq!(state.uploads.len(), MAX_UPLOADS);
        assert!(
            state
                .uploads
                .iter()
                .any(|upload| Arc::ptr_eq(&upload.image, &images[0]))
        );
        assert!(
            !state
                .uploads
                .iter()
                .any(|upload| Arc::ptr_eq(&upload.image, &images[1]))
        );
        assert!(kept_path.is_file());
        assert!(!evicted_path.exists());
        let commands = String::from_utf8(output).unwrap();
        assert_eq!(commands.matches("_Ga=t").count(), MAX_UPLOADS + 1);
        assert!(commands.contains(&format!("a=d,d=I,i={evicted_id},")));
    }

    #[test]
    fn byte_budget_evicts_hidden_uploads_and_counts_repeated_visible_images_once() {
        let width = 2048;
        let height = (MAX_CACHE_BYTES / 2 / 4 / width as usize) as u32;
        let visible = image(ImageProtocol::Kitty, width, height);
        let hidden = image(ImageProtocol::Kitty, width, height);
        assert_eq!(visible.pixels.as_raw().len() * 2, MAX_CACHE_BYTES);
        let visible_id = FIRST_IMAGE_ID;
        let hidden_id = FIRST_IMAGE_ID + 1;
        let mut state = TranscriptImages {
            placements: vec![placement(&visible, 2), placement(&visible, 5)],
            uploads: VecDeque::from([
                Upload {
                    image: Arc::clone(&visible),
                    id: visible_id,
                    _file: None,
                },
                Upload {
                    image: hidden,
                    id: hidden_id,
                    _file: None,
                },
            ]),
            last_id: hidden_id,
        };
        let mut output = Vec::new();
        state.evict_unused(&mut output).unwrap();
        assert_eq!(state.uploads.len(), 2);
        assert!(output.is_empty());

        let extra = image(ImageProtocol::Kitty, 1, 1);
        state.placements.push(placement(&extra, 8));
        state.uploads.push_back(Upload {
            image: extra,
            id: FIRST_IMAGE_ID + 2,
            _file: None,
        });
        state.evict_unused(&mut output).unwrap();
        assert_eq!(state.uploads.len(), 2);
        assert!(Arc::ptr_eq(&state.uploads[0].image, &visible));
        assert_eq!(state.placements.len(), 3);
        let commands = String::from_utf8(output).unwrap();
        assert_eq!(commands.matches("_Ga=d").count(), 1);
        assert!(commands.contains(&format!("a=d,d=I,i={hidden_id},")));
    }

    #[test]
    fn sixel_draws_without_kitty_uploads_and_requests_text_repaint_on_removal() {
        let image = image(ImageProtocol::Sixel, 8, 16);
        let mut state = TranscriptImages::default();
        let mut output = Vec::new();
        state.draw(&mut output, vec![placement(&image, 2)]).unwrap();
        assert!(state.uploads.is_empty());
        let commands = String::from_utf8(output).unwrap();
        assert!(commands.contains("\x1bP9;1;0q"));
        assert!(!commands.contains("\x1b_G"));
        assert!(state.remove_placements(&mut Vec::new()).unwrap());
        assert!(state.placements.is_empty());
    }
}
