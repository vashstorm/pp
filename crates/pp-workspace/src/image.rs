use ::image::{DynamicImage, ImageFormat, ImageReader as Decoder, Limits, imageops::FilterType};
use anyhow::{Result, ensure};
use base64::Engine;
use serde_json::{Value, json};
use std::io::Cursor;

/// Per-input byte limit for image encoding (kept in sync with `pp_llm::http::MAX_INPUT_BYTES`).
pub const MAX_IMAGE_INPUT_BYTES: usize = 20 * 1024 * 1024;

/// Providers reject images by per-side pixels: DeepSeek returns 400 above 8192, Anthropic caps at 8000.
/// Kept as a hard safety red line; no call site may relax it beyond this value.
pub const MAX_IMAGE_SIDE: u32 = 8000;

/// Optimal input side limit for vision models: 2K/4K/Retina screenshots above 1568 px bloat vision tokens
/// and base64 size without improving readability, so they are downscaled proportionally to this side by default.
pub const VISION_MAX_IMAGE_SIDE: u32 = 1568;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolImage {
    pub data_url: String,
}

pub struct DecodedImage {
    pub value: Value,
    pub images: Vec<ToolImage>,
}

pub fn decode_image(source: &str, bytes: &[u8]) -> Result<DecodedImage> {
    ensure!(
        bytes.len() <= MAX_IMAGE_INPUT_BYTES,
        "image exceeds 20 MiB limit"
    );
    let format =
        ::image::guess_format(bytes).map_err(|_| anyhow::anyhow!("unsupported image format"))?;
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ),
        "unsupported image format"
    );
    let mut reader = Decoder::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits.clone());
    let (width, height) = Decoder::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| anyhow::anyhow!("invalid image dimensions"))?;
    ensure!(
        u64::from(width) * u64::from(height) <= 32_000_000,
        "image exceeds 32000000 pixel limit"
    );
    let decoded = reader
        .decode()
        .map_err(|_| anyhow::anyhow!("image decoding failed"))?;
    let target_side = VISION_MAX_IMAGE_SIDE.min(MAX_IMAGE_SIDE);
    let mut converted = Cursor::new(Vec::new());
    let (data, mime, sent) = if width > target_side || height > target_side {
        let scaled = decoded.resize(target_side, target_side, FilterType::Triangle);
        let (encoded, mime) = encode_image(&scaled)?;
        converted = Cursor::new(encoded);
        (
            converted.get_ref().as_slice(),
            mime,
            (scaled.width(), scaled.height()),
        )
    } else if format == ImageFormat::Gif {
        decoded
            .write_to(&mut converted, ImageFormat::Png)
            .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
        (converted.get_ref().as_slice(), "image/png", (width, height))
    } else {
        (bytes, format.to_mime_type(), (width, height))
    };
    ensure!(
        data.len() <= MAX_IMAGE_INPUT_BYTES,
        "converted image exceeds 20 MiB limit"
    );
    let data_url = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    );
    // The size the model sees is the usable size; also report the original size so resolution is not misread.
    let resized = if sent == (width, height) {
        String::new()
    } else {
        format!("\nResized from: {width} x {height} (max {target_side} px per side)")
    };
    let content = format!(
        "Image: {source}\nFormat: {mime}\nDimensions: {} x {}\nBytes: {}{}{}",
        sent.0,
        sent.1,
        bytes.len(),
        if format == ImageFormat::Gif {
            "\nGIF: first frame"
        } else {
            ""
        },
        resized
    );
    let mut value = json!({"ok":true,"kind":"image","content":content,"mime_type":mime,"width":sent.0,"height":sent.1,"bytes":bytes.len()});
    if source.starts_with("skill://") {
        value["source"] = source.into();
    }
    Ok(DecodedImage {
        value,
        images: vec![ToolImage { data_url }],
    })
}

/// After scaling, keep lossless PNG when alpha is present; otherwise compare PNG and JPEG sizes and take the smaller one,
/// keeping flat screenshots crisp while shrinking large photo-like images, without exceeding the input byte limit.
fn encode_image(image: &DynamicImage) -> Result<(Vec<u8>, &'static str)> {
    let mut png_buf = Cursor::new(Vec::new());
    image
        .write_to(&mut png_buf, ImageFormat::Png)
        .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
    let png = png_buf.into_inner();
    if image.color().has_alpha() && png.len() <= MAX_IMAGE_INPUT_BYTES {
        return Ok((png, "image/png"));
    }
    let mut jpg_buf = Cursor::new(Vec::new());
    image
        .to_rgb8()
        .write_to(&mut jpg_buf, ImageFormat::Jpeg)
        .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
    let jpg = jpg_buf.into_inner();
    if png.len() <= MAX_IMAGE_INPUT_BYTES && png.len() <= jpg.len() {
        Ok((png, "image/png"))
    } else {
        Ok((jpg, "image/jpeg"))
    }
}

pub fn is_image(bytes: &[u8]) -> bool {
    ::image::guess_format(bytes).is_ok()
}
