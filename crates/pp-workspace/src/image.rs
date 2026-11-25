use ::image::{DynamicImage, ImageFormat, ImageReader as Decoder, Limits, imageops::FilterType};
use anyhow::{Result, ensure};
use base64::Engine;
use serde_json::{Value, json};
use std::io::Cursor;

/// 图片编码单次输入体积上限（与 `pp_llm::http::MAX_INPUT_BYTES` 保持一致）。
pub const MAX_IMAGE_INPUT_BYTES: usize = 20 * 1024 * 1024;

/// 供应商按单边像素拒绝图片：DeepSeek 超过 8192 直接返回 400，Anthropic 上限 8000。
/// 保留为硬安全红线常量，任何调用都不得放宽超过此值。
pub const MAX_IMAGE_SIDE: u32 = 8000;

/// 视觉模型最优输入边长上限：超过 1568 px 的 2K/4K/Retina 截图会显著膨胀 vision token
/// 与 base64 体积却不提升可读性，默认等比缩小到该边长以内。
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
    // 模型看到的尺寸才是可用尺寸，同时说明原图大小，避免误判分辨率。
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

/// 缩放后含透明通道保留无损 PNG；无透明通道时对比 PNG 与 JPEG 体积择小者，
/// 既保持纯色截图清晰又大幅压缩照片类大图体积，同时确保不超过输入体积上限。
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
