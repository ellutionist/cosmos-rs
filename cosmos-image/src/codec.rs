//! 解码前检查尺寸与字节数；编码写入受限容器，不在完成后才检查输出大小。
use super::{area, MAX_BUFFER, MAX_ENCODED, MAX_SIDE};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbaImage};
use std::io::{self, Cursor, Write};

pub(super) fn format(name: &str) -> Result<ImageFormat, String> {
    match name {
        "png" => Ok(ImageFormat::Png),
        "jpeg" | "jpg" => Ok(ImageFormat::Jpeg),
        "webp" => Ok(ImageFormat::WebP),
        _ => Err("image format must be png, jpeg or webp".into()),
    }
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
pub(super) struct DecodeOptions {
    pub(super) max_width: Option<f64>,
    pub(super) max_height: Option<f64>,
    pub(super) max_pixels: Option<f64>,
    pub(super) auto_orient: bool,
}
pub(super) struct DecodeLimits {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pixels: u64,
    pub(super) orient: bool,
}
impl DecodeOptions {
    pub(super) fn limits(self) -> Result<DecodeLimits, String> {
        Ok(DecodeLimits {
            width: super::integer(
                self.max_width.unwrap_or(MAX_SIDE as f64),
                1,
                MAX_SIDE as u64,
                "maximum width",
            )? as u32,
            height: super::integer(
                self.max_height.unwrap_or(MAX_SIDE as f64),
                1,
                MAX_SIDE as u64,
                "maximum height",
            )? as u32,
            pixels: super::integer(
                self.max_pixels.unwrap_or((MAX_BUFFER / 4) as f64),
                1,
                MAX_BUFFER / 4,
                "maximum pixel count",
            )?,
            orient: self.auto_orient,
        })
    }
}
pub(super) fn decode(data: Vec<u8>, limits: DecodeLimits) -> Result<RgbaImage, String> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|err| format!("invalid image: {err}"))?;
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP)
    ) {
        return Err("unsupported image format (expected PNG, JPEG or WebP)".into());
    }
    let mut native = image::Limits::default();
    native.max_image_width = Some(limits.width);
    native.max_image_height = Some(limits.height);
    native.max_alloc = Some(MAX_BUFFER);
    reader.limits(native.clone());
    let mut decoder = reader
        .into_decoder()
        .map_err(|err| format!("image header: {err}"))?;
    let (width, height) = decoder.dimensions();
    let pixels = area(width, height)?;
    if pixels > limits.pixels {
        return Err("image exceeds maximum pixel count".into());
    }
    if decoder.total_bytes() > MAX_BUFFER {
        return Err("decoded image exceeds 128 MiB".into());
    }
    // 扣除解码目标缓冲后，将剩余额度交给支持分配限制的原生解码器。
    native
        .reserve(decoder.total_bytes())
        .map_err(|err| format!("image allocation limit: {err}"))?;
    decoder
        .set_limits(native)
        .map_err(|err| format!("image allocation limit: {err}"))?;
    let orientation = if limits.orient {
        Some(
            decoder
                .orientation()
                .map_err(|err| format!("image orientation: {err}"))?,
        )
    } else {
        None
    };
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|err| format!("image decode: {err}"))?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    if image.width() > limits.width || image.height() > limits.height {
        return Err("oriented image exceeds dimension limits".into());
    }
    Ok(image.into_rgba8())
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
pub(super) struct EncodeOptions {
    pub(super) quality: Option<f64>,
    pub(super) background: Option<super::ColorInput>,
}
struct Output {
    data: Vec<u8>,
    limit: usize,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.data.len()) {
            return Err(io::Error::other("encoded image exceeds output limit"));
        }
        if self.data.capacity() - self.data.len() < bytes.len() {
            self.data
                .try_reserve_exact(bytes.len())
                .map_err(|_| io::Error::other("encoded image allocation failed"))?;
        }
        self.data.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode(
    image: &RgbaImage,
    format: ImageFormat,
    quality: u8,
    background: Option<image::Rgba<u8>>,
) -> Result<Vec<u8>, String> {
    let mut output = Output {
        data: Vec::new(),
        limit: MAX_ENCODED,
    };
    let result = match format {
        ImageFormat::Png => image::codecs::png::PngEncoder::new(&mut output).write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        ),
        ImageFormat::WebP => image::codecs::webp::WebPEncoder::new_lossless(&mut output)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            ),
        ImageFormat::Jpeg => {
            let mut rgb = Vec::with_capacity(image.width() as usize * image.height() as usize * 3);
            for p in image.pixels() {
                for c in 0..3 {
                    rgb.push(if let Some(bg) = background {
                        ((p[c] as u32 * p[3] as u32 + bg[c] as u32 * (255 - p[3] as u32) + 127)
                            / 255) as u8
                    } else {
                        p[c]
                    });
                }
            }
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality).encode(
                &rgb,
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
        }
        _ => unreachable!("image format is validated before encoding"),
    };
    result.map_err(|err| format!("image encode: {err}"))?;
    Ok(output.data)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_limit_applies_during_writes() {
        let mut out = Output {
            data: Vec::new(),
            limit: 3,
        };
        out.write_all(b"abc").unwrap();
        assert!(out.write_all(b"d").is_err());
        assert_eq!(out.data, b"abc");
    }
}

#[cfg(test)]
mod compatibility {
    use super::*;
    fn limits() -> DecodeLimits {
        DecodeOptions::default().limits().unwrap()
    }
    fn png(image: DynamicImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }
    fn header(mut bytes: Vec<u8>, width: u32, height: u32) -> Vec<u8> {
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let mut crc = 0xffff_ffffu32;
        for &byte in &bytes[12..29] {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        bytes
    }
    #[test]
    fn oversized_headers_are_rejected_before_pixel_decode() {
        let source = png(DynamicImage::new_rgba8(1, 1));
        let error = decode(header(source.clone(), 16000, 16000), limits()).unwrap_err();
        assert!(error.contains("pixel buffer"), "{error}");
        let error = decode(header(source, 16001, 1), limits()).unwrap_err();
        assert!(error.contains("limit"), "{error}");
    }
    #[test]
    fn sixteen_bit_png_is_normalized_to_rgba8() {
        let input = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_pixel(
            2,
            1,
            image::Rgba([65535, 32768, 0, 65535]),
        );
        let result = decode(png(DynamicImage::ImageRgba16(input)), limits()).unwrap();
        assert_eq!(result.get_pixel(0, 0).0, [255, 128, 0, 255]);
    }
    #[test]
    fn jpeg_exif_orientation_is_explicit_and_limits_apply_after_rotation() {
        let original = RgbaImage::from_pixel(2, 3, image::Rgba([80, 140, 200, 255]));
        let bytes = encode(&original, ImageFormat::Jpeg, 95, None).unwrap();
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut tagged = vec![0xff, 0xd8, 0xff, 0xe1];
        tagged.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        tagged.extend_from_slice(exif);
        tagged.extend_from_slice(&bytes[2..]);
        assert_eq!(
            decode(tagged.clone(), limits()).unwrap().dimensions(),
            (2, 3)
        );
        let mut oriented = limits();
        oriented.orient = true;
        assert_eq!(
            decode(tagged.clone(), oriented).unwrap().dimensions(),
            (3, 2)
        );
        let mut bounded = limits();
        bounded.orient = true;
        bounded.width = 2;
        bounded.height = 3;
        assert!(decode(tagged, bounded).unwrap_err().contains("oriented"));
    }
}
