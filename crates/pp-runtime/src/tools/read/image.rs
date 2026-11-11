use super::Read;
use super::ReadContext;
use super::ReadResult;
use super::ToolImage;
use super::selector::Target;
use ::image::{DynamicImage, ImageFormat, ImageReader as Decoder, Limits, imageops::FilterType};
use anyhow::{Result, ensure};
use base64::Engine;
use pp_llm::http::MAX_INPUT_BYTES;
use serde_json::json;
use std::io::Cursor;

/// 供应商按单边像素拒绝图片：DeepSeek 超过 8192 直接返回 400，Anthropic 上限 8000。
/// 超过上限时等比缩小后再发送，否则整条请求（含历史）都会被拒绝。
const MAX_IMAGE_SIDE: u32 = 8000;

pub(super) struct ImageReader;
pub(super) struct ImageInput {
    pub(super) target: Target,
    pub(super) bytes: Vec<u8>,
}

impl Read for ImageReader {
    type Input = ImageInput;

    fn read(
        &self,
        ImageInput { target, bytes }: Self::Input,
        _context: &mut ReadContext<'_, '_>,
    ) -> Result<ReadResult> {
        target.validate_image()?;
        decode_image(&target.source, &bytes)
    }
}

pub(crate) fn decode_image(source: &str, bytes: &[u8]) -> Result<ReadResult> {
    ensure!(bytes.len() <= MAX_INPUT_BYTES, "image exceeds 20 MiB limit");
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
    let mut converted = Cursor::new(Vec::new());
    let (data, mime, sent) = if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        let scaled = decoded.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, FilterType::Triangle);
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
        data.len() <= MAX_INPUT_BYTES,
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
        format!("\nResized from: {width} x {height} (max {MAX_IMAGE_SIDE} px per side)")
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
    Ok(ReadResult {
        value,
        images: vec![ToolImage { data_url }],
    })
}

/// 缩放后优先无损 PNG（截图与文字保持清晰）；超过输入上限时退回 JPEG，避免重新触发体积拒绝。
fn encode_image(image: &DynamicImage) -> Result<(Vec<u8>, &'static str)> {
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, ImageFormat::Png)
        .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
    let png = encoded.into_inner();
    if png.len() <= MAX_INPUT_BYTES {
        return Ok((png, "image/png"));
    }
    let mut encoded = Cursor::new(Vec::new());
    image
        .to_rgb8()
        .write_to(&mut encoded, ImageFormat::Jpeg)
        .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
    Ok((encoded.into_inner(), "image/jpeg"))
}

pub(super) fn is_image(bytes: &[u8]) -> bool {
    ::image::guess_format(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::{WebCache, run};
    use pp_config::config::EditMode;
    use pp_config::config::ToolLimits;
    use pp_core::cancellation::CancellationToken;
    use std::path::Path;

    fn read_image(bytes: Vec<u8>) -> Result<ReadResult> {
        ImageReader.read(
            ImageInput {
                target: Target::parse("fixture", Path::new("."))?,
                bytes,
            },
            &mut ReadContext {
                snapshots: None,
                limits: ToolLimits::default(),
                mode: EditMode::Replace,
                cache: &mut WebCache::default(),
                fetch: &mut |_, _| unreachable!(),
                cancellation: &CancellationToken::new(),
            },
        )
    }

    #[test]
    fn read_images_validate_and_keep_payload_out_of_display() {
        use base64::Engine;
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
            image::ImageFormat::Gif,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 3)
                .write_to(&mut bytes, format)
                .unwrap();
            let result = read_image(bytes.into_inner()).unwrap();
            assert_eq!(result.value["width"], 2);
            assert_eq!(result.value["height"], 3);
            assert_eq!(result.images.len(), 1);
            assert!(!result.value.to_string().contains("base64"));
            let data = result.images[0].data_url.split_once(',').unwrap().1;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            assert!(image::load_from_memory(&decoded).is_ok());
            if format == image::ImageFormat::Gif {
                assert_eq!(result.value["mime_type"], "image/png");
            }
        }
        assert!(read_image(b"not an image".to_vec()).is_err());
        assert!(read_image(vec![0; MAX_INPUT_BYTES + 1]).is_err());
        assert!(read_image(b"GIF89a\xff\xff\xff\xff\x00\x00\x00".to_vec()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        image::DynamicImage::new_rgb8(1, 1).save(&path).unwrap();
        assert!(
            run(
                "image.png:1",
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace,
                &pp_config::skills::Skills::default().shared(),
                &mut WebCache::default(),
                &mut |_, _| unreachable!(),
                &CancellationToken::new()
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_side_is_scaled_into_the_provider_limit() {
        use base64::Engine;
        for (width, height) in [(120, MAX_IMAGE_SIDE + 1000), (MAX_IMAGE_SIDE + 1000, 120)] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(width, height)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            let result = read_image(bytes.into_inner()).unwrap();
            let sent_width = u32::try_from(result.value["width"].as_u64().unwrap()).unwrap();
            let sent_height = u32::try_from(result.value["height"].as_u64().unwrap()).unwrap();
            assert_eq!(sent_width.max(sent_height), MAX_IMAGE_SIDE);
            assert_eq!(result.value["mime_type"], "image/png");
            let scale = f64::from(MAX_IMAGE_SIDE) / f64::from(width.max(height));
            assert!((f64::from(sent_width) - f64::from(width) * scale).abs() <= 1.0);
            assert!((f64::from(sent_height) - f64::from(height) * scale).abs() <= 1.0);
            let content = result.value["content"].as_str().unwrap();
            assert!(content.contains(&format!("Dimensions: {sent_width} x {sent_height}")));
            assert!(content.contains(&format!("Resized from: {width} x {height}")));
            let data = result.images[0].data_url.split_once(',').unwrap().1;
            let decoded = image::load_from_memory(
                &base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                (decoded.width(), decoded.height()),
                (sent_width, sent_height)
            );
        }
        // 未超过上限时保持原始字节，不重新编码。
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(3, MAX_IMAGE_SIDE)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let source = bytes.into_inner();
        let result = read_image(source.clone()).unwrap();
        assert!(
            !result.value["content"]
                .as_str()
                .unwrap()
                .contains("Resized from")
        );
        let data = result.images[0].data_url.split_once(',').unwrap().1;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            source
        );
    }
}
