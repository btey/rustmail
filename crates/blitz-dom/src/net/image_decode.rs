use crate::node::RasterImageData;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::{io::Cursor, sync::Arc};

#[derive(Clone, Copy, Debug)]
pub struct ImageDecodeLimits {
    pub max_source_dimension: u32,
    pub max_source_pixels: u64,
    /// JPEGs can be admitted at a larger source size because the decoder uses
    /// native IDCT scaling before allocating the retained pixel buffer.
    pub max_jpeg_source_dimension: u32,
    pub max_jpeg_source_pixels: u64,
    pub max_alloc: u64,
    pub target_dimension: u32,
}

impl ImageDecodeLimits {
    /// Bound decoding and retain intrinsic dimensions independently of pixels.
    /// JPEG uses native IDCT scaling; other formats are resized after their
    /// bounded decode. The first frame is used for animated formats.
    ///
    /// EXIF orientation is applied, as `image-orientation: from-image` is the
    /// CSS default, so the intrinsic size is that of the upright image.
    pub fn decode(self, bytes: &[u8]) -> Result<RasterImageData, String> {
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let is_jpeg = reader.format() == Some(ImageFormat::Jpeg);
        let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
        let (width, height) = decoder.dimensions();
        let (max_source_dimension, max_source_pixels) = if is_jpeg {
            (self.max_jpeg_source_dimension, self.max_jpeg_source_pixels)
        } else {
            (self.max_source_dimension, self.max_source_pixels)
        };
        if width == 0
            || height == 0
            || width > max_source_dimension
            || height > max_source_dimension
            || u64::from(width) * u64::from(height) > max_source_pixels
            || self.target_dimension == 0
        {
            return Err("Image dimensions exceed the document decoder limits".into());
        }
        // Malformed metadata must not lose an otherwise valid image.
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);

        let target = self.target_dimension;
        let scaled_jpeg = (is_jpeg && width.max(height) > target)
            .then(|| self.decode_scaled_jpeg(bytes, width, height))
            .flatten();
        let decoded = match scaled_jpeg {
            Some(image) => image,
            None => {
                // The general decoder allocates the full source, so the
                // allocation limit still bounds JPEGs that could not be scaled.
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(max_source_dimension);
                limits.max_image_height = Some(max_source_dimension);
                limits.max_alloc = Some(self.max_alloc);
                decoder.set_limits(limits).map_err(|e| e.to_string())?;
                DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?
            }
        };
        let mut decoded = if decoded.width().max(decoded.height()) > target {
            decoded.thumbnail(target, target)
        } else {
            decoded
        };
        decoded.apply_orientation(orientation);
        let (width, height) = match orientation {
            Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH => (height, width),
            _ => (width, height),
        };

        let pixel_width = decoded.width();
        let pixel_height = decoded.height();
        let mut image =
            RasterImageData::new(width, height, Arc::new(decoded.into_rgba8().into_raw()));
        image.pixel_width = pixel_width;
        image.pixel_height = pixel_height;
        Ok(image)
    }

    /// Decode a large RGB or greyscale JPEG at a reduced size using the
    /// decoder's IDCT scaling. `None` defers to the general decoder, which
    /// handles everything else (CMYK, streams this decoder rejects).
    fn decode_scaled_jpeg(self, bytes: &[u8], width: u32, height: u32) -> Option<DynamicImage> {
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
        decoder.set_max_decoding_buffer_size(self.max_alloc.min(usize::MAX as u64) as usize);
        decoder.read_info().ok()?;
        let info = decoder.info()?;
        if !matches!(
            info.pixel_format,
            jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8
        ) {
            return None;
        }
        let divisor = width
            .max(height)
            .div_ceil(self.target_dimension)
            .next_power_of_two()
            .min(8);
        let (w, h) = decoder
            .scale(
                width.div_ceil(divisor) as u16,
                height.div_ceil(divisor) as u16,
            )
            .ok()?;
        let pixels = decoder.decode().ok()?;
        Some(match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                DynamicImage::ImageRgb8(image::RgbImage::from_raw(w.into(), h.into(), pixels)?)
            }
            _ => DynamicImage::ImageLuma8(image::GrayImage::from_raw(w.into(), h.into(), pixels)?),
        })
    }
}
