//! Rust 原生静态图像插件；图像是不可变共享值，处理任务不发送 Lua 到工作线程。
#![warn(unreachable_pub)]
mod codec;
mod work;
use cosmos::userdata::LuaBytes;
use cosmos::{Machine, Module};
use image::{
    imageops::{self, FilterType},
    Rgba, RgbaImage,
};
use std::sync::Arc;
const MAX_SIDE: u32 = 16_000;
const MAX_BUFFER: u64 = 128 * 1024 * 1024;
const MAX_ENCODED: usize = 64 * 1024 * 1024;
const MAX_WORK: u64 = 512 * 1024 * 1024;
const MAX_RESAMPLE: u64 = 256 * 1024 * 1024;
/// 安装 cosmos.image；只公开插件入口，图像与编解码实现保持私有。
pub struct Image;
impl Module for Image {
    fn register(&self, machine: &mut Machine) -> Result<(), String> {
        machine.register::<Picture>();
        Ok(())
    }
}
#[derive(Clone)]
struct Picture(Arc<RgbaImage>);
fn integer(value: f64, min: u64, max: u64, name: &str) -> Result<u64, String> {
    if !value.is_finite() || value.fract() != 0.0 || value < min as f64 || value > max as f64 {
        return Err(format!(
            "invalid {name} (expected an integer in {min}..={max})"
        ));
    }
    Ok(value as u64)
}
fn area(width: u32, height: u32) -> Result<u64, String> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Err("image dimensions must be in 1..=16000".into());
    }
    let count = width as u64 * height as u64;
    if count > MAX_BUFFER / 4 {
        return Err("image pixel buffer exceeds 128 MiB".into());
    }
    Ok(count)
}
fn dimensions(width: f64, height: f64) -> Result<(u32, u32), String> {
    let w = integer(width, 1, MAX_SIDE as u64, "width")? as u32;
    let h = integer(height, 1, MAX_SIDE as u64, "height")? as u32;
    area(w, h)?;
    Ok((w, h))
}
fn filter(name: Option<&str>) -> Result<FilterType, String> {
    match name.unwrap_or("lanczos3") {
        "nearest" => Ok(FilterType::Nearest),
        "triangle" => Ok(FilterType::Triangle),
        "catmull_rom" => Ok(FilterType::CatmullRom),
        "gaussian" => Ok(FilterType::Gaussian),
        "lanczos3" => Ok(FilterType::Lanczos3),
        _ => Err("unknown image resize filter".into()),
    }
}
#[derive(Default, cosmos::FromLua)]
#[lua(default)]
struct ColorInput {
    r: f64,
    g: f64,
    b: f64,
    a: Option<f64>,
}
impl ColorInput {
    fn rgba(self) -> Result<Rgba<u8>, String> {
        Ok(Rgba([
            integer(self.r, 0, 255, "red")? as u8,
            integer(self.g, 0, 255, "green")? as u8,
            integer(self.b, 0, 255, "blue")? as u8,
            integer(self.a.unwrap_or(255.0), 0, 255, "alpha")? as u8,
        ]))
    }
}
#[derive(cosmos::PushLua)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}
#[derive(cosmos::PushLua)]
struct Info {
    width: u32,
    height: u32,
    pixel_format: &'static str,
    bytes: u32,
}
#[derive(Clone, Copy)]
enum Pixels {
    Luma,
    Rgb,
    Rgba,
}
impl Pixels {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "luma8" => Ok(Self::Luma),
            "rgb8" => Ok(Self::Rgb),
            "rgba8" => Ok(Self::Rgba),
            _ => Err("pixel format must be luma8, rgb8 or rgba8".into()),
        }
    }
    fn channels(self) -> usize {
        match self {
            Self::Luma => 1,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
    fn decode(self, data: &[u8]) -> Rgba<u8> {
        match self {
            Self::Luma => Rgba([data[0], data[0], data[0], 255]),
            Self::Rgb => Rgba([data[0], data[1], data[2], 255]),
            Self::Rgba => Rgba([data[0], data[1], data[2], data[3]]),
        }
    }
}
impl Picture {
    fn wrap(image: RgbaImage) -> Self {
        Self(Arc::new(image))
    }
    fn region(
        &self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<(u32, u32, u32, u32), String> {
        let x = integer(x, 0, self.0.width() as u64 - 1, "x")? as u32;
        let y = integer(y, 0, self.0.height() as u64 - 1, "y")? as u32;
        let (w, h) = dimensions(width, height)?;
        if w > self.0.width() - x || h > self.0.height() - y {
            return Err("image region exceeds bounds".into());
        }
        Ok((x, y, w, h))
    }
    async fn resized(
        self,
        width: u32,
        height: u32,
        filter: FilterType,
        thumbnail: bool,
    ) -> Result<Self, String> {
        if (width, height) == self.0.dimensions() {
            return Ok(self);
        }
        let output = area(width, height)? * 4;
        let intermediate = if thumbnail {
            0
        } else {
            self.0.width() as u64 * height as u64 * 16
        };
        if intermediate > MAX_RESAMPLE || self.0.len() as u64 + output + intermediate > MAX_WORK {
            return Err("image resize exceeds working memory limit".into());
        }
        work::run(move || {
            move || {
                Ok(Self::wrap(if thumbnail {
                    imageops::thumbnail(&*self.0, width, height)
                } else {
                    imageops::resize(&*self.0, width, height, filter)
                }))
            }
        })
        .await
    }
}
#[cosmos::userdata(name = "image")]
impl Picture {
    async fn decode(data: LuaBytes, options: Option<codec::DecodeOptions>) -> Result<Self, String> {
        if data.len() > MAX_ENCODED {
            return Err("encoded image input exceeds 64 MiB".into());
        }
        let limits = options.unwrap_or_default().limits()?;
        work::run(move || {
            let data = data.to_vec();
            move || codec::decode(data, limits).map(Self::wrap)
        })
        .await
    }
    async fn new(width: f64, height: f64, color: Option<ColorInput>) -> Result<Self, String> {
        let (w, h) = dimensions(width, height)?;
        let color = color
            .map(ColorInput::rgba)
            .transpose()?
            .unwrap_or(Rgba([0; 4]));
        work::run(move || move || Ok(Self::wrap(RgbaImage::from_pixel(w, h, color)))).await
    }
    async fn from_pixels(
        width: f64,
        height: f64,
        format: String,
        data: LuaBytes,
    ) -> Result<Self, String> {
        let (w, h) = dimensions(width, height)?;
        let format = Pixels::parse(&format)?;
        if data.len() as u64 != area(w, h)? * format.channels() as u64 {
            return Err("pixel byte count does not match image dimensions".into());
        }
        work::run(move || {
            let data = data.to_vec();
            move || {
                if let Pixels::Rgba = format {
                    return RgbaImage::from_raw(w, h, data)
                        .map(Self::wrap)
                        .ok_or_else(|| "invalid RGBA buffer".into());
                }
                let mut out = RgbaImage::new(w, h);
                for (target, source) in out.pixels_mut().zip(data.chunks_exact(format.channels())) {
                    *target = format.decode(source);
                }
                Ok(Self::wrap(out))
            }
        })
        .await
    }
    fn width(&self) -> u32 {
        self.0.width()
    }
    fn height(&self) -> u32 {
        self.0.height()
    }
    fn info(&self) -> Info {
        Info {
            width: self.0.width(),
            height: self.0.height(),
            pixel_format: "rgba8",
            bytes: self.0.len() as u32,
        }
    }
    fn pixel(&self, x: f64, y: f64) -> Result<Color, String> {
        let (x, y, _, _) = self.region(x, y, 1.0, 1.0)?;
        let [r, g, b, a] = self.0.get_pixel(x, y).0;
        Ok(Color { r, g, b, a })
    }
    async fn pixels(self, format: Option<String>) -> Result<Vec<u8>, String> {
        let format = Pixels::parse(format.as_deref().unwrap_or("rgba8"))?;
        work::run(move || {
            move || {
                Ok(match format {
                    Pixels::Rgba => self.0.as_raw().clone(),
                    Pixels::Rgb => {
                        let mut bytes = Vec::with_capacity(
                            self.0.width() as usize * self.0.height() as usize * 3,
                        );
                        for pixel in self.0.pixels() {
                            bytes.extend_from_slice(&pixel.0[..3]);
                        }
                        bytes
                    }
                    Pixels::Luma => {
                        use image::Pixel;
                        self.0.pixels().map(|p| p.to_luma().0[0]).collect()
                    }
                })
            }
        })
        .await
    }
    async fn with_pixels(
        self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        format: String,
        data: LuaBytes,
    ) -> Result<Self, String> {
        let (x, y, w, h) = self.region(x, y, width, height)?;
        let format = Pixels::parse(&format)?;
        if data.len() as u64 != area(w, h)? * format.channels() as u64 {
            return Err("pixel byte count does not match patch dimensions".into());
        }
        work::run(move || {
            let data = data.to_vec();
            move || {
                let mut out = (*self.0).clone();
                for (i, chunk) in data.chunks_exact(format.channels()).enumerate() {
                    out.put_pixel(x + i as u32 % w, y + i as u32 / w, format.decode(chunk));
                }
                Ok(Self::wrap(out))
            }
        })
        .await
    }
    async fn crop(self, x: f64, y: f64, width: f64, height: f64) -> Result<Self, String> {
        let (x, y, w, h) = self.region(x, y, width, height)?;
        work::run(move || {
            move || {
                Ok(Self::wrap(
                    imageops::crop_imm(&*self.0, x, y, w, h).to_image(),
                ))
            }
        })
        .await
    }
    async fn resize(self, width: f64, height: f64, method: Option<String>) -> Result<Self, String> {
        let (w, h) = dimensions(width, height)?;
        self.resized(w, h, filter(method.as_deref())?, false).await
    }
    async fn thumbnail(
        self,
        width: f64,
        height: f64,
        upscale: Option<bool>,
    ) -> Result<Self, String> {
        let bound_w = integer(width, 1, MAX_SIDE as u64, "thumbnail width")? as u32;
        let bound_h = integer(height, 1, MAX_SIDE as u64, "thumbnail height")? as u32;
        let scale =
            (bound_w as f64 / self.0.width() as f64).min(bound_h as f64 / self.0.height() as f64);
        let scale = if upscale.unwrap_or(false) {
            scale
        } else {
            scale.min(1.0)
        };
        let w = ((self.0.width() as f64 * scale).round() as u32).max(1);
        let h = ((self.0.height() as f64 * scale).round() as u32).max(1);
        self.resized(w, h, FilterType::Nearest, true).await
    }
    async fn rotate(self, degrees: f64) -> Result<Self, String> {
        if ![0.0, 90.0, 180.0, 270.0].contains(&degrees) {
            return Err("image rotation must be 0, 90, 180 or 270 degrees clockwise".into());
        }
        if degrees == 0.0 {
            return Ok(self);
        }
        work::run(move || {
            move || {
                Ok(Self::wrap(match degrees as u16 {
                    90 => imageops::rotate90(&*self.0),
                    180 => imageops::rotate180(&*self.0),
                    _ => imageops::rotate270(&*self.0),
                }))
            }
        })
        .await
    }
    async fn flip(self, axis: String) -> Result<Self, String> {
        if axis != "horizontal" && axis != "vertical" {
            return Err("image flip axis must be horizontal or vertical".into());
        }
        work::run(move || {
            move || {
                Ok(Self::wrap(if axis == "horizontal" {
                    imageops::flip_horizontal(&*self.0)
                } else {
                    imageops::flip_vertical(&*self.0)
                }))
            }
        })
        .await
    }
    async fn encode(
        self,
        format: String,
        options: Option<codec::EncodeOptions>,
    ) -> Result<Vec<u8>, String> {
        let format = codec::format(&format)?;
        let opts = options.unwrap_or_default();
        if format != image::ImageFormat::Jpeg
            && (opts.quality.is_some() || opts.background.is_some())
        {
            return Err("quality and background options apply only to JPEG".into());
        }
        let quality = integer(opts.quality.unwrap_or(90.0), 1, 100, "JPEG quality")? as u8;
        let bg = opts.background.map(ColorInput::rgba).transpose()?;
        if bg.is_some_and(|v| v[3] != 255) {
            return Err("JPEG background must be opaque".into());
        }
        work::run(move || move || codec::encode(&self.0, format, quality, bg)).await
    }
}
