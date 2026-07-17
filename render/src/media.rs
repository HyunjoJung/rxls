//! Bounded embedded-image decoding independent of output backends.

use std::io::Cursor;

use jpeg_decoder::PixelFormat;
use rxls::{DrawingCrop, Image, ImageFmt};

use crate::error::{LimitKind, RenderError};
use crate::layout::RenderLimits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

pub(crate) fn decode_image(
    image: &Image,
    crop: Option<DrawingCrop>,
    limits: &RenderLimits,
    retained_decoded_bytes: &mut u64,
) -> Result<Option<DecodedImage>, RenderError> {
    let dimensions = match image.format {
        ImageFmt::Png => png_dimensions(&image.data),
        ImageFmt::Jpeg => jpeg_dimensions(&image.data),
    };
    let Some((width, height)) = dimensions else {
        return Ok(None);
    };
    let decoded_bytes = preflight_dimensions(width, height, limits)?;
    let peak = retained_decoded_bytes
        .checked_add(decoded_bytes)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::DecodedMediaBytes,
        limits.max_decoded_media_bytes,
        peak,
    )?;

    let decoded = match image.format {
        ImageFmt::Png => decode_png(&image.data, limits.max_decoded_media_bytes),
        ImageFmt::Jpeg => decode_jpeg(&image.data, limits.max_decoded_media_bytes),
    };
    let Some(decoded) = decoded else {
        return Ok(None);
    };
    if decoded.width != width || decoded.height != height {
        return Ok(None);
    }
    let Some(decoded) = crop_image(decoded, crop) else {
        return Ok(None);
    };
    let retained = retained_decoded_bytes
        .checked_add(decoded.rgba.len() as u64)
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(
        LimitKind::DecodedMediaBytes,
        limits.max_decoded_media_bytes,
        retained,
    )?;
    *retained_decoded_bytes = retained;
    Ok(Some(decoded))
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24
        || &data[..8] != b"\x89PNG\r\n\x1a\n"
        || &data[12..16] != b"IHDR"
        || u32::from_be_bytes(data[8..12].try_into().ok()?) != 13
    {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    (width != 0 && height != 0).then_some((width, height))
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(data));
    decoder.read_info().ok()?;
    let info = decoder.info()?;
    Some((u32::from(info.width), u32::from(info.height)))
}

fn preflight_dimensions(
    width: u32,
    height: u32,
    limits: &RenderLimits,
) -> Result<u64, RenderError> {
    enforce(
        LimitKind::ImageDimension,
        limits.max_image_dimension,
        u64::from(width.max(height)),
    )?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or(RenderError::CoordinateOverflow)?;
    enforce(LimitKind::ImagePixels, limits.max_image_pixels, pixels)?;
    pixels.checked_mul(4).ok_or(RenderError::CoordinateOverflow)
}

fn decode_png(data: &[u8], max_decoded_bytes: u64) -> Option<DecodedImage> {
    let decoder_limit = usize::try_from(max_decoded_bytes).unwrap_or(usize::MAX);
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(data),
        png::Limits {
            bytes: decoder_limit,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0_u8; reader.output_buffer_size()];
    let output = reader.next_frame(&mut buffer).ok()?;
    buffer.truncate(output.buffer_size());
    let rgba = expand_png_rgba(&buffer, output.color_type)?;
    Some(DecodedImage {
        width: output.width,
        height: output.height,
        rgba,
    })
}

fn expand_png_rgba(bytes: &[u8], color: png::ColorType) -> Option<Vec<u8>> {
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::Rgb => 3,
        png::ColorType::Indexed => return None,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgba => 4,
    };
    if bytes.len() % channels != 0 {
        return None;
    }
    let pixels = bytes.len() / channels;
    let mut rgba = Vec::with_capacity(pixels.checked_mul(4)?);
    for pixel in bytes.chunks_exact(channels) {
        match color {
            png::ColorType::Grayscale => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]);
            }
            png::ColorType::Rgb => rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            png::ColorType::GrayscaleAlpha => {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            }
            png::ColorType::Rgba => rgba.extend_from_slice(pixel),
            png::ColorType::Indexed => return None,
        }
    }
    Some(rgba)
}

fn decode_jpeg(data: &[u8], max_decoded_bytes: u64) -> Option<DecodedImage> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(data));
    decoder.set_max_decoding_buffer_size(usize::try_from(max_decoded_bytes).unwrap_or(usize::MAX));
    let decoded = decoder.decode().ok()?;
    let info = decoder.info()?;
    let pixels = usize::from(info.width).checked_mul(usize::from(info.height))?;
    let mut rgba = Vec::with_capacity(pixels.checked_mul(4)?);
    match info.pixel_format {
        PixelFormat::L8 => {
            if decoded.len() != pixels {
                return None;
            }
            for value in decoded {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        PixelFormat::L16 => {
            if decoded.len() != pixels.checked_mul(2)? {
                return None;
            }
            for value in decoded.chunks_exact(2) {
                rgba.extend_from_slice(&[value[0], value[0], value[0], 255]);
            }
        }
        PixelFormat::RGB24 => {
            if decoded.len() != pixels.checked_mul(3)? {
                return None;
            }
            for value in decoded.chunks_exact(3) {
                rgba.extend_from_slice(&[value[0], value[1], value[2], 255]);
            }
        }
        PixelFormat::CMYK32 => {
            if decoded.len() != pixels.checked_mul(4)? {
                return None;
            }
            for value in decoded.chunks_exact(4) {
                let convert = |channel: u8| {
                    let inverted = u16::from(255 - channel);
                    let black = u16::from(255 - value[3]);
                    ((inverted * black + 127) / 255) as u8
                };
                rgba.extend_from_slice(&[
                    convert(value[0]),
                    convert(value[1]),
                    convert(value[2]),
                    255,
                ]);
            }
        }
    }
    Some(DecodedImage {
        width: u32::from(info.width),
        height: u32::from(info.height),
        rgba,
    })
}

fn crop_image(image: DecodedImage, crop: Option<DrawingCrop>) -> Option<DecodedImage> {
    let Some(crop) = crop else {
        return Some(image);
    };
    let left = crop.left_ppm.min(1_000_000);
    let top = crop.top_ppm.min(1_000_000);
    let right = crop.right_ppm.min(1_000_000);
    let bottom = crop.bottom_ppm.min(1_000_000);
    if left.saturating_add(right) >= 1_000_000 || top.saturating_add(bottom) >= 1_000_000 {
        return None;
    }
    let edge =
        |extent: u32, ppm: u32| u32::try_from(u64::from(extent) * u64::from(ppm) / 1_000_000).ok();
    let first_x = edge(image.width, left)?;
    let first_y = edge(image.height, top)?;
    let last_x = image.width.checked_sub(edge(image.width, right)?)?;
    let last_y = image.height.checked_sub(edge(image.height, bottom)?)?;
    if first_x >= last_x || first_y >= last_y {
        return None;
    }
    if first_x == 0 && first_y == 0 && last_x == image.width && last_y == image.height {
        return Some(image);
    }
    let width = last_x - first_x;
    let height = last_y - first_y;
    let capacity = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
    let mut rgba = Vec::with_capacity(capacity);
    let source_stride = usize::try_from(u64::from(image.width) * 4).ok()?;
    let first_byte = usize::try_from(u64::from(first_x) * 4).ok()?;
    let last_byte = usize::try_from(u64::from(last_x) * 4).ok()?;
    for row in first_y..last_y {
        let start = usize::try_from(row).ok()?.checked_mul(source_stride)?;
        rgba.extend_from_slice(&image.rgba[start + first_byte..start + last_byte]);
    }
    Some(DecodedImage {
        width,
        height,
        rgba,
    })
}

fn enforce(kind: LimitKind, limit: u64, actual: u64) -> Result<(), RenderError> {
    if actual > limit {
        Err(RenderError::LimitExceeded {
            kind,
            limit,
            actual,
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut output, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(rgba).unwrap();
        }
        output
    }

    #[test]
    fn png_alpha_is_retained_exactly() {
        let source = [
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 255, 255, 255, 0,
        ];
        let image = Image::new(rgba_png(4, 1, &source), ImageFmt::Png, (0, 0));
        let mut retained = 0;
        let decoded = decode_image(&image, None, &RenderLimits::default(), &mut retained)
            .unwrap()
            .unwrap();
        assert_eq!((decoded.width, decoded.height), (4, 1));
        assert_eq!(decoded.rgba, source);
        assert_eq!(retained, 16);
    }

    #[test]
    fn image_header_limits_fail_before_decode_allocation() {
        let mut data = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        data.extend_from_slice(&20_000_u32.to_be_bytes());
        data.extend_from_slice(&1_u32.to_be_bytes());
        let image = Image::new(data, ImageFmt::Png, (0, 0));
        let mut retained = 0;
        assert_eq!(
            decode_image(&image, None, &RenderLimits::default(), &mut retained),
            Err(RenderError::LimitExceeded {
                kind: LimitKind::ImageDimension,
                limit: 16_384,
                actual: 20_000,
            })
        );
        assert_eq!(retained, 0);
    }
}
