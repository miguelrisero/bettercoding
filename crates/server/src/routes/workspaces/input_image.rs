use std::{
    io::{Cursor, Read, Write},
    path::Path,
};

use image::{ImageEncoder, ImageFormat, ImageReader, Limits, codecs::png::PngEncoder};

use super::file_policy::{self, MAX_INPUT_BYTES};
use crate::error::ApiError;

fn invalid_image() -> Box<ApiError> {
    Box::new(ApiError::BadRequest("Invalid Input raster image".into()))
}

// The output writer enforces the existing Input byte cap during encoding.
struct RasterOutput(Vec<u8>);
impl Write for RasterOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_INPUT_BYTES {
            return Err(std::io::Error::other("Input image exceeds byte cap"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn raster_bytes(reader: impl Read, length: u64) -> Result<Vec<u8>, Box<ApiError>> {
    if length > MAX_INPUT_BYTES as u64 {
        return Err(Box::new(ApiError::PayloadTooLarge));
    }
    let mut bytes = Vec::new();
    reader
        .take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(ApiError::from)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Box::new(ApiError::PayloadTooLarge));
    }
    let format = image::guess_format(&bytes).map_err(|_| invalid_image())?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
    ) {
        return Err(invalid_image());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| invalid_image())?;
    if u64::from(decoded.width()) * u64::from(decoded.height()) * 4 > 16 * 1024 * 1024 {
        return Err(invalid_image());
    }
    let image = decoded.into_rgba8();
    let mut output = RasterOutput(Vec::new());
    // Encode pixels only: source metadata and trailing bytes never reach the iframe.
    PngEncoder::new(&mut output)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| invalid_image())?;
    Ok(output.0)
}

pub(super) fn raster_file(base: &Path, relative: &str) -> Result<Vec<u8>, Box<ApiError>> {
    let path = file_policy::resolve_existing_path(base, relative)?;
    let canonical_base = base.canonicalize().map_err(ApiError::from)?;
    let canonical_relative = path
        .strip_prefix(canonical_base)
        .map_err(|_| invalid_image())?;
    if canonical_relative
        .components()
        .any(|part| file_policy::is_hidden(&part.as_os_str().to_string_lossy()))
    {
        return Err(invalid_image());
    }
    if !matches!(
        path.extension()
            .and_then(|part| part.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    ) {
        return Err(invalid_image());
    }
    let file = std::fs::File::open(path).map_err(ApiError::from)?;
    let metadata = file.metadata().map_err(ApiError::from)?;
    if !metadata.is_file() {
        return Err(invalid_image());
    }
    raster_bytes(file, metadata.len())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Cursor};

    use super::*;

    fn png() -> Vec<u8> {
        use base64::Engine;
        let source = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j3ioAAAAASUVORK5CYII=").unwrap();
        assert!(raster_bytes(Cursor::new(&source), source.len() as u64).is_err());
        let mut valid = Vec::new();
        PngEncoder::new(&mut valid)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        valid
    }

    #[test]
    fn input_raster_rejects_spoofed_content_and_strips_trailing_bytes() {
        assert!(raster_bytes(Cursor::new(b"SECRET=value"), 12).is_err());
        assert!(raster_bytes(Cursor::new(b"<svg onload='alert(1)'/>"), 24).is_err());
        let mut source = png();
        source.extend_from_slice(b"SECRET=value");
        let result = raster_bytes(Cursor::new(&source), source.len() as u64).unwrap();
        assert!(result.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(!result.windows(6).any(|part| part == b"SECRET"));
    }

    #[test]
    fn input_raster_enforces_decode_and_encoded_output_limits() {
        let mut source = Vec::new();
        PngEncoder::new(&mut source)
            .write_image(&vec![0; 4097 * 4], 4097, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        assert!(raster_bytes(Cursor::new(&source), source.len() as u64).is_err());
        let mut output = RasterOutput(Vec::new());
        assert!(output.write_all(&vec![0; MAX_INPUT_BYTES]).is_ok());
        assert!(output.write_all(&[0]).is_err());
        assert_eq!(output.0.len(), MAX_INPUT_BYTES);
    }

    #[test]
    fn input_raster_rejects_oversize_before_reading() {
        struct Counting<'a>(&'a Cell<usize>);
        impl Read for Counting<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                self.0.set(self.0.get() + 1);
                output.fill(0);
                Ok(output.len())
            }
        }
        let reads = Cell::new(0);
        assert!(raster_bytes(Counting(&reads), 256 * 1024 + 1).is_err());
        assert_eq!(reads.get(), 0);
        // A growing file must also fail without an unbounded read.
        assert!(raster_bytes(Counting(&reads), 1).is_err());
        assert!(reads.get() < 64);
    }

    #[test]
    fn input_raster_keeps_valid_images_and_rejects_sensitive_symlink() {
        let root = std::env::var_os("INPUT_RICH_TEST_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pixel.png"), png()).unwrap();
        std::fs::write(root.join(".env"), b"SECRET=value").unwrap();
        std::fs::write(root.join("spoof.png"), b"SECRET=value").unwrap();
        assert!(!raster_file(&root, "pixel.png").unwrap().is_empty());
        assert!(raster_file(&root, "spoof.png").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(".env", root.join("public.png")).unwrap();
            assert!(raster_file(&root, "public.png").is_err());
        }
        // Retain fixtures: the brief forbids cleanup deletion.
    }
}
