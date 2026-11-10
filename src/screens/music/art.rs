//! Album art: fetched from wherever the player points (a file, a URL, or
//! inline in the URL itself), decoded, scaled to cover the screen and
//! darkened toward the bottom for the text, ready to copy onto it.
//!
//! The URL comes from the player, and browsers take it from the page that's
//! playing, so it's treated with care: only a few schemes, a size cap on
//! the download and a cap on the picture's dimensions.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use tiny_skia::{
    FilterQuality, GradientStop, LinearGradient, Paint, Pixmap, PixmapPaint, Point, Rect,
    SpreadMode, Transform,
};
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::render::SIZE;

/// The most art we'll download or read.
const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// The largest picture we'll decode, either way.
const MAX_SIDE: usize = 2048;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Where the shade over the bottom starts, as a share of the height, and
/// how dark it gets at the bottom edge.
const SHADE_FROM: f32 = 0.45;
const SHADE_ALPHA: u8 = 235;

/// The art at `url`, scaled and shaded for the screen.
pub fn load(url: &str) -> Result<Pixmap> {
    let bytes = fetch(url)?;
    let art = decode(&bytes)?;
    Ok(compose(&art))
}

/// A local cover file may be overwritten at the same URL, even mid-track.
pub(super) fn file_stamp(url: &str) -> Option<(SystemTime, u64)> {
    let path = percent_decode(url.strip_prefix("file://")?);
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn fetch(url: &str) -> Result<Vec<u8>> {
    if let Some(data) = url.strip_prefix("data:") {
        // data:image/jpeg;base64,...
        let (kind, payload) = data.split_once(',').context("a data URL without data")?;
        ensure!(kind.ends_with(";base64"), "a data URL that isn't base64");
        ensure!(
            payload.len() as u64 <= MAX_BYTES * 4 / 3 + 4,
            "a data URL over {MAX_BYTES} bytes"
        );
        return Ok(base64::engine::general_purpose::STANDARD.decode(payload.trim())?);
    }
    if let Some(path) = url.strip_prefix("file://") {
        let path = percent_decode(path);
        let regular = std::fs::metadata(&path)
            .with_context(|| format!("opening {path}"))?
            .is_file();
        ensure!(regular, "art must be a regular file");
        // A pipe swapped in after the check mustn't strand the artwork worker.
        let file = std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("opening {path}"))?;
        ensure!(file.metadata()?.is_file(), "art must be a regular file");
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "{path} is too big");
        return Ok(bytes);
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .max_redirects(3)
            .user_agent(concat!("cooler-lcd/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let mut bytes = Vec::new();
        agent
            .get(url)
            // Some CDNs serve WebP unless asked for something else.
            .header("Accept", "image/jpeg, image/png")
            .call()
            .with_context(|| format!("fetching {url}"))?
            .body_mut()
            .with_config()
            // What's downloaded, which may be compressed...
            .limit(MAX_BYTES)
            .reader()
            // ...and what that expands to, which the limit doesn't count.
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_BYTES, "art at {url} is too big");
        return Ok(bytes);
    }
    bail!("art at {url:?}, which isn't a file, data or web URL")
}

/// Decodes a JPEG or PNG, told apart by their first bytes.
fn decode(bytes: &[u8]) -> Result<Pixmap> {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // RGB, made RGBA here: the decoder's own RGBA comes out wrong for
        // CMYK art (which Adobe tools export).
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::RGB)
            .set_max_width(MAX_SIDE)
            .set_max_height(MAX_SIDE);
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
        let rgb = decoder.decode().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (w, h) = decoder.dimensions().context("JPEG without dimensions")?;
        ensure!(rgb.len() == w * h * 3, "JPEG of the wrong size");
        // Opaque, so it's already premultiplied.
        let rgba = rgb
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect();
        let size = tiny_skia::IntSize::from_wh(w as u32, h as u32).context("an empty JPEG")?;
        return Pixmap::from_vec(rgba, size).context("JPEG of the wrong size");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // Width and height in the header chunk, ahead of decoding it all.
        let side = |at: usize| {
            bytes
                .get(at..at + 4)
                .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
        };
        ensure!(
            side(16) <= MAX_SIDE && side(20) <= MAX_SIDE,
            "a PNG over {MAX_SIDE} pixels"
        );
        return Pixmap::decode_png(bytes).context("decoding PNG");
    }
    bail!("art that's neither JPEG nor PNG")
}

/// `art` scaled to cover the screen, centered and cropped, on black (for
/// any transparency), with the shade over the bottom.
fn compose(art: &Pixmap) -> Pixmap {
    let mut out = Pixmap::new(SIZE, SIZE).unwrap();
    out.fill(tiny_skia::Color::BLACK);
    // Halve big art first: one bicubic step down from 2000 pixels would
    // skip most of them and alias.
    let mut halved: Option<Pixmap> = None;
    loop {
        let art = halved.as_ref().unwrap_or(art);
        if art.width().min(art.height()) < 2 * SIZE {
            break;
        }
        halved = Some(scale(art, 0.5, FilterQuality::Bilinear));
    }
    let art = halved.as_ref().unwrap_or(art);
    let side = art.width().min(art.height()) as f32;
    let k = SIZE as f32 / side;
    let (dx, dy) = (
        (SIZE as f32 - art.width() as f32 * k) / 2.0,
        (SIZE as f32 - art.height() as f32 * k) / 2.0,
    );
    let paint = PixmapPaint {
        quality: FilterQuality::Bicubic,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(
        0,
        0,
        art.as_ref(),
        &paint,
        Transform::from_row(k, 0.0, 0.0, k, dx, dy),
        None,
    );
    shade(&mut out);
    out
}

fn scale(px: &Pixmap, k: f32, quality: FilterQuality) -> Pixmap {
    let w = ((px.width() as f32 * k).round() as u32).max(1);
    let h = ((px.height() as f32 * k).round() as u32).max(1);
    let mut out = Pixmap::new(w, h).unwrap();
    let paint = PixmapPaint {
        quality,
        ..PixmapPaint::default()
    };
    out.draw_pixmap(0, 0, px.as_ref(), &paint, Transform::from_scale(k, k), None);
    out
}

/// Darkens the bottom of `px` from clear at `SHADE_FROM` down to
/// `SHADE_ALPHA` black at the edge, easing in so there's no visible line
/// where it starts.
pub fn shade(px: &mut Pixmap) {
    let s = SIZE as f32;
    let top = s * SHADE_FROM;
    let black = |a: f32| tiny_skia::Color::from_rgba8(0, 0, 0, (a * SHADE_ALPHA as f32) as u8);
    let stops = [0.0, 0.25, 0.5, 0.75, 1.0]
        .map(|t: f32| GradientStop::new(t, black(t * t * (3.0 - 2.0 * t))))
        .to_vec();
    let Some(shader) = LinearGradient::new(
        Point::from_xy(0.0, top),
        Point::from_xy(0.0, s),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    ) else {
        return;
    };
    let paint = Paint {
        shader,
        ..Paint::default()
    };
    if let Some(rect) = Rect::from_xywh(0.0, top, s, s - top) {
        px.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

/// `%20` and the like in a file URL's path, back to the bytes they stand
/// for.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let rgb: Vec<u8> = (0..w as usize * h as usize * 3)
            .map(|i| (i % 251) as u8)
            .collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90)
            .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    #[test]
    fn covers_the_screen_with_any_art() {
        // Wide art is cropped at the sides, big art scaled down.
        for (w, h) in [(640, 640), (300, 200), (2000, 2040)] {
            let art = decode(&jpeg(w, h)).unwrap();
            assert_eq!((art.width(), art.height()), (w as u32, h as u32));
            let out = compose(&art);
            assert_eq!((out.width(), out.height()), (SIZE, SIZE));
            // The top corner is art, not the black underneath.
            assert!(out.pixel(1, 1).unwrap().green() > 0 || out.pixel(1, 1).unwrap().red() > 0);
        }
    }

    #[test]
    fn decodes_cmyk_art() {
        // Pure red in CMYK: no cyan, full magenta and yellow, no black.
        let cmyk: Vec<u8> = [0, 255, 255, 0].repeat(16 * 16);
        let mut bytes = Vec::new();
        jpeg_encoder::Encoder::new(&mut bytes, 90)
            .encode(&cmyk, 16, 16, jpeg_encoder::ColorType::Cmyk)
            .unwrap();
        let art = decode(&bytes).unwrap();
        for p in [art.pixel(0, 0).unwrap(), art.pixel(15, 15).unwrap()] {
            assert!(p.red() > 200 && p.green() < 60 && p.blue() < 60, "{p:?}");
            assert_eq!(p.alpha(), 255);
        }
    }

    #[test]
    fn reads_art_inline_and_from_files() {
        let bytes = jpeg(16, 16);
        let inline = format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert_eq!(fetch(&inline).unwrap(), bytes);
        let path = std::env::temp_dir().join(format!("cooler-lcd art {}.jpg", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let link = path.with_extension("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let linked = fetch(&format!("file://{}", link.display()).replace(' ', "%20"));
        std::fs::remove_file(link).unwrap();
        let url = format!("file://{}", path.display()).replace(' ', "%20");
        let read = fetch(&url);
        let _ = std::fs::remove_file(&path);
        assert_eq!(linked.unwrap(), bytes);
        assert_eq!(read.unwrap(), bytes);
        assert!(fetch("ftp://example.com/a.jpg").is_err());
        assert!(fetch("data:image/png,raw").is_err());
    }

    #[test]
    fn local_art_rejects_pipes_symlinks_to_pipes_and_devices() {
        use std::sync::mpsc;
        use std::thread;

        let dir = std::env::temp_dir().join(format!("cooler-lcd-art-types-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated pathname in our test directory.
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        let link = dir.join("linked pipe");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        for path in [&fifo, &link, &dir, std::path::Path::new("/dev/zero")] {
            let url = format!("file://{}", path.display()).replace(' ', "%20");
            let (done, result) = mpsc::channel();
            let worker = thread::spawn(move || {
                let _ = done.send(fetch(&url));
            });
            let result = result.recv_timeout(Duration::from_secs(2));
            if result.is_err() {
                // Release a regressed FIFO reader before failing the test.
                drop(
                    std::fs::File::options()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&fifo),
                );
            }
            worker.join().unwrap();
            assert!(
                result.expect("artwork load blocked").is_err(),
                "{}",
                path.display()
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn caps_web_art_at_its_size_once_decompressed() {
        use std::io::Write;

        // Just over the cap of zeros: under a hundredth of it on the way.
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![0; MAX_BYTES as usize + 1]).unwrap();
        let body = gzip.finish().unwrap();
        assert!((body.len() as u64) < MAX_BYTES / 100);

        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/cover.png", server.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let (mut request, mut byte) = (Vec::new(), [0]);
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            // The reader may hang up once it has seen too much.
            let _ = stream.write_all(&body);
        });
        // Its size, so a failure doesn't print the bytes.
        let fetched = fetch(&url).map(|bytes| bytes.len());
        let refused = fetched.expect_err("art over the cap");
        assert!(refused.to_string().contains("too big"), "{refused:#}");
    }

    #[test]
    fn refuses_what_it_cant_decode() {
        assert!(decode(b"RIFF....WEBP").is_err());
        assert!(decode(&[0xFF, 0xD8, 0xFF]).is_err(), "a truncated JPEG");
        assert!(decode(&jpeg(16, 2100)).is_err(), "too tall");
        let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        huge.extend_from_slice(&100_000u32.to_be_bytes());
        huge.extend_from_slice(&100_000u32.to_be_bytes());
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn decodes_file_url_paths() {
        assert_eq!(percent_decode("/tmp/a%20b%C3%A9.jpg"), "/tmp/a bé.jpg");
        assert_eq!(percent_decode("/100%"), "/100%");
    }
}
