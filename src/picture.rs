//! Local images for script submissions. Decode and resize before handing
//! them to the frame loop; the panel only ever sees an opaque 480×480 copy.

use std::fs::File;
use std::io::{Cursor, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, imageops::FilterType};
use tiny_skia::{IntSize, Pixmap};

use crate::render::SIZE;

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;

pub fn load(path: &Path) -> Result<Pixmap> {
    // O_NONBLOCK avoids hanging a connection thread opening a named pipe.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "image must be a regular file");
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "image is larger than 10 MiB"
    );
    decode(&bytes).with_context(|| format!("decoding {}", path.display()))
}

fn decode(bytes: &[u8]) -> Result<Pixmap> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    ensure!(
        decoder.total_bytes() <= 128 * 1024 * 1024,
        "decoded image is larger than 128 MiB"
    );
    let orientation = decoder.orientation()?;
    let mut picture = DynamicImage::from_decoder(decoder)?;
    picture.apply_orientation(orientation);
    // Crop before enlarging: scaling a 4096×1 panorama to cover first
    // would allocate an enormous intermediate image just to discard it.
    let side = picture.width().min(picture.height());
    let cropped = picture.crop_imm(
        (picture.width() - side) / 2,
        (picture.height() - side) / 2,
        side,
        side,
    );
    let mut rgba = cropped.to_rgba8();
    // Composite before filtering so invisible RGB values in transparent
    // pixels cannot bleed into the visible edge. Renderer expects opacity.
    for p in rgba.as_mut().as_chunks_mut::<4>().0 {
        let alpha = p[3] as u16;
        for channel in &mut p[..3] {
            *channel = ((*channel as u16 * alpha + 127) / 255) as u8;
        }
        p[3] = 255;
    }
    let rgba = image::imageops::resize(&rgba, SIZE, SIZE, FilterType::Lanczos3).into_raw();
    Pixmap::from_vec(rgba, IntSize::from_wh(SIZE, SIZE).unwrap()).context("wrong image dimensions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgb, Rgba};

    fn bytes(image: DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        let image = if format == ImageFormat::Ico {
            DynamicImage::ImageRgba8(image.to_rgba8())
        } else {
            image
        };
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn reads_common_formats_and_centers_the_crop() {
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Bmp,
            ImageFormat::Ico,
            ImageFormat::Tiff,
        ] {
            let source =
                DynamicImage::ImageRgb8(ImageBuffer::from_pixel(16, 16, Rgb([20, 180, 60])));
            let picture = decode(&bytes(source, format)).unwrap();
            assert_eq!((picture.width(), picture.height()), (SIZE, SIZE));
            assert!(picture.pixel(240, 240).unwrap().green() > 170, "{format:?}");
            assert!(picture.pixels().iter().all(|p| p.alpha() == 255));
        }
        let source = DynamicImage::ImageRgb8(ImageBuffer::from_fn(900, 300, |x, _| {
            if x < 300 {
                Rgb([255, 0, 0])
            } else if x < 600 {
                Rgb([0, 255, 0])
            } else {
                Rgb([0, 0, 255])
            }
        }));
        let picture = decode(&bytes(source, ImageFormat::Png)).unwrap();
        for (x, y) in [(120, 120), (360, 360)] {
            assert!(picture.pixel(x, y).unwrap().green() > 250);
        }
    }

    #[test]
    fn composites_transparency_onto_black_without_a_music_shade() {
        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(16, 16, Rgba([200, 100, 50, 128])));
        let picture = decode(&bytes(source, ImageFormat::Png)).unwrap();
        for y in [0, 240, 479] {
            let p = picture.pixel(240, y).unwrap();
            assert_eq!(
                (p.red(), p.green(), p.blue(), p.alpha()),
                (100, 50, 25, 255)
            );
        }
    }

    #[test]
    fn extreme_aspect_ratios_and_transparent_edges_stay_bounded_and_clean() {
        for (width, height) in [(1, MAX_SIDE), (MAX_SIDE, 1)] {
            let source =
                DynamicImage::ImageRgb8(ImageBuffer::from_pixel(width, height, Rgb([0, 255, 0])));
            let picture = decode(&bytes(source, ImageFormat::Png)).unwrap();
            assert_eq!(picture.pixel(240, 240).unwrap().green(), 255);
        }
        let source = DynamicImage::ImageRgba8(ImageBuffer::from_fn(2, 2, |x, _| {
            if x == 0 {
                Rgba([255, 0, 0, 0])
            } else {
                Rgba([0, 0, 255, 255])
            }
        }));
        let picture = decode(&bytes(source, ImageFormat::Png)).unwrap();
        assert!(
            picture.pixels().iter().all(|p| p.red() == 0),
            "invisible red must not contaminate the blue edge"
        );
    }

    #[test]
    fn honors_exif_orientation() {
        let source = DynamicImage::ImageRgb8(ImageBuffer::from_fn(100, 100, |_, y| {
            if y < 50 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        }));
        let jpeg = bytes(source, ImageFormat::Jpeg);
        // Little-endian TIFF, one SHORT orientation tag: 6 (rotate clockwise).
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut oriented = vec![0xff, 0xd8, 0xff, 0xe1];
        oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        oriented.extend_from_slice(exif);
        oriented.extend_from_slice(&jpeg[2..]);
        let picture = decode(&oriented).unwrap();
        assert!(picture.pixel(40, 240).unwrap().blue() > 200);
        assert!(picture.pixel(440, 240).unwrap().red() > 200);
    }

    #[test]
    fn refuses_large_broken_and_nonregular_inputs() {
        assert!(decode(b"<svg/>").is_err());
        assert!(decode(b"\xff\xd8\xff").is_err());
        let large =
            DynamicImage::ImageRgb8(ImageBuffer::from_pixel(2, MAX_SIDE + 1, Rgb([0, 0, 0])));
        assert!(decode(&bytes(large, ImageFormat::Png)).is_err());
        let dir = std::env::temp_dir().join(format!("cooler-lcd-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_err());
        let path = dir.join("huge.png");
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(load(&path).unwrap_err().to_string().contains("10 MiB"));
        let fifo = dir.join("pipe");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the path is a valid NUL-terminated pathname in our temp dir.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(load(&fifo).is_err(), "a pipe with no writer must not block");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
